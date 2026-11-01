<?php
/*
 * laravel-parity runner. Copied into the app's container and run with the app's own PHP, so the old app runs on its
 * old PHP/Laravel and the new app on its new ones. It never opens a port: it boots the app in this process and hands
 * each request to the HTTP kernel, the way a feature test does.
 *
 *   php replay.php --app=/opt/localhost/api --job=/tmp/laravel-parity/job.json --out=/tmp/laravel-parity/out.jsonl
 *
 * Safety: every database connection that pointed at the app's own database is repointed at the test database named
 * in the job before the first request, and the run stops if the default connection is not on it. Each request runs
 * in a transaction that is rolled back. Mail, notifications, queued jobs, broadcasts and outgoing HTTP (Laravel's
 * client) are faked and recorded, never sent.
 *
 * Kept to PHP 7.1 syntax so the same file runs on Laravel 5.5 / PHP 7.x and on Laravel 13 / PHP 8.5.
 */

error_reporting(E_ALL & ~E_DEPRECATED & ~E_USER_DEPRECATED);
ini_set('display_errors', 'stderr');
ini_set('memory_limit', '2G');

$opts = getopt('', array('app:', 'job:', 'out:'));
if (empty($opts['app']) || empty($opts['job']) || empty($opts['out'])) {
    fwrite(STDERR, "usage: php replay.php --app=<laravel root> --job=<job.json> --out=<out.jsonl>\n");
    exit(2);
}
$appPath = rtrim($opts['app'], '/');
$job = json_decode(file_get_contents($opts['job']), true);
$out = fopen($opts['out'], 'w');
// the tool stops a run that takes too long by this pid (a command that never ends)
@file_put_contents(dirname($opts['out']).'/pid', (string) getmypid());

chdir($appPath);
require $appPath.'/vendor/autoload.php';

// PHP's own http(s) streams (file_get_contents of a URL) ignore http_proxy; send them to the same dead proxy
if (getenv('https_proxy')) {
    stream_context_set_default(array('http' => array(
        'proxy' => str_replace('http://', 'tcp://', getenv('https_proxy')),
        'request_fulluri' => true,
        'timeout' => 5,
    )));
}
if (! defined('LARAVEL_START')) {
    define('LARAVEL_START', microtime(true));
}

/** Records what the app broadcasts right away (ShouldBroadcastNow); queued broadcasts are read from the queue fake. */
class ParityBroadcaster extends Illuminate\Broadcasting\Broadcasters\Broadcaster
{
    public static $sent = array();

    public function auth($request)
    {
        return true;
    }

    public function validAuthenticationResponse($request, $result)
    {
        return $result;
    }

    public function broadcast(array $channels, $event, array $payload = array())
    {
        $names = array();
        foreach ($channels as $channel) {
            $names[] = (string) $channel;
        }
        unset($payload['socket']);
        self::$sent[] = array('event' => $event, 'channels' => $names, 'payload' => parity_value($payload, 0));
    }
}

/** A JSON-safe copy of a value: objects become their class name (or array form), depth is capped. */
function parity_value($value, $depth)
{
    if ($depth > 6) {
        return '…';
    }
    if (is_null($value) || is_scalar($value)) {
        return is_string($value) && strlen($value) > 2000 ? substr($value, 0, 2000).'…' : $value;
    }
    if ($value instanceof DateTimeInterface) {
        return $value->format('Y-m-d H:i:s');
    }
    if (is_array($value)) {
        $copy = array();
        foreach ($value as $k => $v) {
            $copy[$k] = parity_value($v, $depth + 1);
        }

        return $copy;
    }
    if (is_object($value)) {
        if ($value instanceof JsonSerializable) {
            return parity_value($value->jsonSerialize(), $depth + 1);
        }
        if (method_exists($value, 'toArray')) {
            return parity_value($value->toArray(), $depth + 1);
        }

        return '['.get_class($value).']';
    }

    return '['.gettype($value).']';
}

/** A protected property of a fake (the fakes' read methods differ between Laravel versions; the arrays do not). */
function parity_prop($object, $name)
{
    $class = new ReflectionClass($object);
    while ($class && ! $class->hasProperty($name)) {
        $class = $class->getParentClass();
    }
    if (! $class) {
        return array();
    }
    $prop = $class->getProperty($name);
    $prop->setAccessible(true);

    return (array) $prop->getValue($object);
}

function parity_emit($out, array $row)
{
    $json = json_encode($row, JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE | JSON_PARTIAL_OUTPUT_ON_ERROR);
    fwrite($out, $json."\n");
    fflush($out);
}

/** A fresh application, bootstrapped and repointed at the test database. */
function parity_boot($appPath, array $job, $console, $request = null)
{
    $app = require $appPath.'/bootstrap/app.php';
    $kernel = $app->make($console ? 'Illuminate\Contracts\Console\Kernel' : 'Illuminate\Contracts\Http\Kernel');
    if (! $console) {
        // the HTTP kernel binds the request before bootstrapping; some service providers read it while booting
        $app->instance('request', $request ?: Illuminate\Http\Request::create('/'));
    }
    $kernel->bootstrap();

    $config = $app['config'];
    $default = $config->get('database.default');
    $original = $config->get("database.connections.$default.database");
    $target = $job['database'];
    foreach ((array) $config->get('database.connections') as $name => $connection) {
        if ($name === $default || (isset($connection['database']) && $connection['database'] === $original)) {
            foreach ($target as $key => $value) {
                $config->set("database.connections.$name.$key", $value);
            }
            $app['db']->purge($name);
        }
    }
    $actual = $app['db']->connection()->getDatabaseName();
    if ($actual !== $target['database']) {
        fwrite(STDERR, "refusing to run: the app is on database '$actual', not the test database '{$target['database']}'\n");
        exit(3);
    }

    // an in-memory cache per request: rate limiters (throttle) would otherwise count every request of the run, and
    // the app's real cache (redis) is left alone; sessions likewise
    $config->set('cache.default', 'array');
    $config->set('cache.limiter', 'array');
    $config->set('session.driver', 'array');
    // service providers that ran during bootstrap (RateLimiter::for ...) already resolved the cache: repoint them
    $app->forgetInstance('cache.store');
    if ($app->resolved('Illuminate\Cache\RateLimiter')) {
        $limiter = $app->make('Illuminate\Cache\RateLimiter');
        $property = new ReflectionProperty($limiter, 'cache');
        $property->setAccessible(true);
        $property->setValue($limiter, $app['cache']->store('array'));
    }

    if (! isset($GLOBALS['parity_broadcast_default'])) {
        $GLOBALS['parity_broadcast_default'] = (string) $config->get('broadcasting.default');
    }
    $config->set('broadcasting.connections.parity', array('driver' => 'parity'));
    $config->set('broadcasting.default', 'parity');
    $app->make('Illuminate\Broadcasting\BroadcastManager')->extend('parity', function () {
        return new ParityBroadcaster();
    });

    return array($app, $kernel);
}

/** Whether a class (or a parent) declares a property / method; false when the class is missing. */
function parity_has($class, $member, $method)
{
    if (! class_exists($class)) {
        return false;
    }
    $reflection = new ReflectionClass($class);

    return $method ? $reflection->hasMethod($member) : $reflection->hasProperty($member);
}

/**
 * The Laravel internals the runner reads to record side effects. A newer Laravel may rename one; then that kind of
 * side effect is reported as NOT compared instead of silently comparing as empty on both apps.
 */
function parity_capabilities()
{
    $fakes = 'Illuminate\Support\Testing\Fakes\\';

    return array(
        'jobs' => parity_has($fakes.'QueueFake', 'jobs', false),
        'mails' => parity_has($fakes.'MailFake', 'mailables', false) && parity_has($fakes.'MailFake', 'queuedMailables', false),
        'notifications' => parity_has($fakes.'NotificationFake', 'notifications', false),
        'broadcasts' => parity_has('Illuminate\Broadcasting\BroadcastEvent', 'getPayloadFromEvent', true),
        'http' => class_exists('Illuminate\Support\Facades\Http'),
        'logs' => class_exists('Illuminate\Log\Events\MessageLogged'),
        'rate_limiter' => parity_has('Illuminate\Cache\RateLimiter', 'cache', false),
        'channels' => parity_has('Illuminate\Broadcasting\Broadcasters\Broadcaster', 'channels', false)
            && parity_has('Illuminate\Broadcasting\Broadcasters\Broadcaster', 'verifyUserCanAccessChannel', true)
            && parity_has('Illuminate\Broadcasting\BroadcastManager', 'drivers', false),
    );
}

function parity_info($app)
{
    return array(
        'capabilities' => parity_capabilities(),
        'php' => PHP_VERSION,
        'laravel' => $app->version(),
        'url' => (string) $app['config']->get('app.url'),
        'web_url' => (string) $app['config']->get('app.web_url', ''),
        'database' => $app['db']->connection()->getDatabaseName(),
        'mysql' => $app['db']->connection()->getPdo()->getAttribute(PDO::ATTR_SERVER_VERSION),
    );
}

/** A bearer token per guard, made once per run (outside any transaction) for the user named in the job. */
function parity_tokens($app, array $job)
{
    $tokens = array();
    $notes = array();
    foreach ((array) $job['users'] as $guard => $id) {
        $user = parity_user($app, $guard, $id);
        if (! $user) {
            $notes[$guard] = "no user $id for guard $guard";
            continue;
        }
        try {
            $tokens[$guard] = method_exists($user, 'createToken') ? $user->createToken('laravel-parity')->accessToken : null;
        } catch (Throwable $e) {
            $tokens[$guard] = null;
            $notes[$guard] = 'token not made ('.get_class($e).': '.$e->getMessage().'); signed in through the guard only';
        }
    }

    return array($tokens, $notes);
}

function parity_user($app, $guard, $id)
{
    $provider = $app['config']->get("auth.guards.$guard.provider");
    $model = $app['config']->get("auth.providers.$provider.model");

    return $model ? $model::find($id) : null;
}

function parity_server(array $headers)
{
    $server = array('HTTP_ACCEPT' => 'application/json', 'CONTENT_TYPE' => 'application/json');
    foreach ($headers as $name => $value) {
        $server['HTTP_'.strtoupper(str_replace('-', '_', $name))] = (string) $value;
    }

    return $server;
}

function parity_response($response)
{
    $row = array(
        'status' => $response->getStatusCode(),
        'content_type' => (string) $response->headers->get('Content-Type'),
        'disposition' => (string) $response->headers->get('Content-Disposition'),
        'location' => (string) $response->headers->get('Location'),
    );
    if ($response instanceof Symfony\Component\HttpFoundation\BinaryFileResponse) {
        $row['kind'] = 'file';
        $row['size'] = $response->getFile()->getSize();

        return $row;
    }
    if ($response instanceof Symfony\Component\HttpFoundation\StreamedResponse) {
        ob_start();
        try {
            $response->sendContent();
        } catch (Throwable $e) {
        }
        $content = (string) ob_get_clean();
        $row['kind'] = 'stream';
        $row['size'] = strlen($content);

        return $row;
    }
    $content = (string) $response->getContent();
    $json = json_decode($content, true);
    if ($content !== '' && json_last_error() === JSON_ERROR_NONE) {
        $row['kind'] = 'json';
        $row['json'] = $json;
    } else {
        $row['kind'] = $content === '' ? 'empty' : 'text';
        $row['size'] = strlen($content);
        $row['sha1'] = sha1($content);
        $row['text'] = substr($content, 0, 500);
    }

    return $row;
}

/** What the request did besides answering: rows written, jobs queued, mail, notifications, broadcasts, HTTP calls. */
function parity_effects($app, array $writes, array $logs)
{
    $jobs = array();
    $broadcasts = ParityBroadcaster::$sent;
    $queue = $app['queue'];
    if ($queue instanceof Illuminate\Support\Testing\Fakes\QueueFake) {
        foreach (parity_prop($queue, 'jobs') as $class => $pushed) {
            foreach ($pushed as $entry) {
                $job = isset($entry['job']) ? $entry['job'] : null;
                if ($job instanceof Illuminate\Broadcasting\BroadcastEvent) {
                    $broadcasts[] = parity_broadcast($job);
                    continue;
                }
                $jobs[] = is_object($job) ? get_class($job) : (string) $class;
            }
        }
    }

    $mails = array();
    $mailer = $app['mailer'];
    if ($mailer instanceof Illuminate\Support\Testing\Fakes\MailFake) {
        foreach (array('mailables', 'queuedMailables') as $prop) {
            foreach (parity_prop($mailer, $prop) as $mailable) {
                $to = array();
                foreach ((array) (isset($mailable->to) ? $mailable->to : array()) as $recipient) {
                    $to[] = is_array($recipient) && isset($recipient['address']) ? $recipient['address'] : parity_value($recipient, 5);
                }
                $mails[] = array('class' => get_class($mailable), 'to' => $to);
            }
        }
    }

    $notifications = array();
    $notifier = $app['Illuminate\Contracts\Notifications\Dispatcher'];
    if ($notifier instanceof Illuminate\Support\Testing\Fakes\NotificationFake) {
        foreach (parity_prop($notifier, 'notifications') as $notifiable => $byId) {
            foreach ((array) $byId as $id => $byClass) {
                foreach ((array) $byClass as $class => $sent) {
                    foreach ((array) $sent as $entry) {
                        $notifications[] = array(
                            'notifiable' => $notifiable.'#'.$id,
                            'class' => $class,
                            'channels' => isset($entry['channels']) ? array_values((array) $entry['channels']) : array(),
                        );
                    }
                }
            }
        }
    }

    $http = array();
    if (class_exists('Illuminate\Support\Facades\Http')) {
        try {
            foreach (Illuminate\Support\Facades\Http::recorded() as $pair) {
                $http[] = $pair[0]->method().' '.$pair[0]->url();
            }
        } catch (Throwable $e) {
        }
    }

    return array(
        'writes' => $writes,
        'jobs' => $jobs,
        'mails' => $mails,
        'notifications' => $notifications,
        'broadcasts' => $broadcasts,
        'http' => $http,
        'logs' => $logs,
    );
}

function parity_broadcast($job)
{
    $event = $job->event;
    $channels = array();
    try {
        foreach ((array) $event->broadcastOn() as $channel) {
            $channels[] = (string) $channel;
        }
    } catch (Throwable $e) {
        $channels[] = 'error: '.$e->getMessage();
    }
    $payload = null;
    try {
        $method = new ReflectionMethod($job, 'getPayloadFromEvent');
        $method->setAccessible(true);
        $payload = parity_value($method->invoke($job, $event), 0);
    } catch (Throwable $e) {
    }

    return array(
        'event' => method_exists($event, 'broadcastAs') ? $event->broadcastAs() : get_class($event),
        'channels' => $channels,
        'payload' => $payload,
    );
}

/** What a request, a command or a job did besides answering, collected while it runs. */
class ParityRecorder
{
    public static $writes = array();
    public static $logs = array();
    public static $queries = 0;
}

/** Fixed clock, fakes for everything that leaves the app, and listeners for writes and logged errors. */
function parity_record($app, array $job)
{
    ParityBroadcaster::$sent = array();
    ParityRecorder::$writes = array();
    ParityRecorder::$logs = array();
    ParityRecorder::$queries = 0;
    if (! empty($job['clock'])) {
        foreach (array('Illuminate\Support\Carbon', 'Carbon\Carbon', 'Carbon\CarbonImmutable') as $carbon) {
            if (class_exists($carbon)) {
                $carbon::setTestNow($carbon::parse($job['clock']));
            }
        }
    }
    // every disk goes to a throwaway folder: a DELETE must not remove real uploads, a POST must not leave files
    foreach (array_keys((array) $app['config']->get('filesystems.disks')) as $disk) {
        try {
            Illuminate\Support\Facades\Storage::fake($disk);
        } catch (Throwable $e) {
        }
    }
    Illuminate\Support\Facades\Queue::fake();
    Illuminate\Support\Facades\Mail::fake();
    Illuminate\Support\Facades\Notification::fake();
    if (class_exists('Illuminate\Support\Facades\Http')) {
        Illuminate\Support\Facades\Http::fake();
    }
    $app['db']->listen(function ($query) {
        ParityRecorder::$queries++;
        if (preg_match('/^\s*(insert|update|delete|replace)\b/i', $query->sql)) {
            ParityRecorder::$writes[] = array('sql' => $query->sql, 'bindings' => parity_value($query->bindings, 0));
        }
    });
    $app['events']->listen('Illuminate\Log\Events\MessageLogged', function ($event) {
        if (in_array($event->level, array('emergency', 'alert', 'critical', 'error', 'warning'), true)) {
            ParityRecorder::$logs[] = $event->level.': '.substr((string) $event->message, 0, 400);
        }
    });
}

/** Roll back, record the effects into the row, and leave the app for the next one. */
function parity_finish($app, $connection, array $row, $started)
{
    $row['ms'] = (int) round((microtime(true) - $started) * 1000);
    while ($connection->transactionLevel() > 0) {
        $connection->rollBack();
    }
    $row['queries'] = ParityRecorder::$queries;
    $row['effects'] = parity_effects($app, ParityRecorder::$writes, ParityRecorder::$logs);

    foreach (array('Illuminate\Support\Carbon', 'Carbon\Carbon', 'Carbon\CarbonImmutable') as $carbon) {
        if (class_exists($carbon)) {
            $carbon::setTestNow();
        }
    }
    $app->flush();
    Illuminate\Support\Facades\Facade::clearResolvedInstances();

    return $row;
}

function parity_request($appPath, array $job, array $spec, array $tokens)
{
    $guard = isset($spec['guard']) ? $spec['guard'] : null;
    $headers = array_merge((array) $job['headers'], isset($spec['headers']) ? (array) $spec['headers'] : array());
    if ($guard && ! empty($tokens[$guard])) {
        $headers['Authorization'] = 'Bearer '.$tokens[$guard];
    }
    $method = strtoupper($spec['method']);
    $uri = '/'.ltrim($spec['uri'], '/');
    if (! empty($spec['query'])) {
        $uri .= (strpos($uri, '?') === false ? '?' : '&').http_build_query($spec['query']);
    }
    $body = isset($spec['body']) ? $spec['body'] : null;
    $request = Illuminate\Http\Request::create(
        $uri,
        $method,
        array(),
        array(),
        array(),
        parity_server($headers),
        $method === 'GET' || $body === null ? null : json_encode($body)
    );

    list($app, $kernel) = parity_boot($appPath, $job, false, $request);
    parity_record($app, $job);

    if ($guard && empty($tokens[$guard]) && isset($job['users'][$guard])) {
        // no token could be made: sign in through the guard instead
        $user = parity_user($app, $guard, $job['users'][$guard]);
        if ($user) {
            $app['auth']->guard($guard)->setUser($user);
            $app['auth']->shouldUse($guard);
        }
    }

    $connection = $app['db']->connection();
    $connection->beginTransaction();
    $started = microtime(true);
    $row = array('id' => $spec['id']);
    // a buffer like PHP-FPM has: libraries such as mPDF refuse to send a file once anything reached the output
    $level = ob_get_level();
    ob_start();
    try {
        $response = $kernel->handle($request);
        $row = array_merge($row, parity_response($response));
    } catch (Throwable $e) {
        $row['status'] = 'runner-error';
        $row['error'] = get_class($e).': '.$e->getMessage();
    }
    $printed = '';
    while (ob_get_level() > $level) {
        $printed = ob_get_clean().$printed;
    }
    // older code prints files itself (mPDF Output(), readfile) and returns an empty response: that is the answer
    if ($printed !== '' && isset($row['kind']) && in_array($row['kind'], array('empty', 'text'), true)
        && (! isset($row['size']) || $row['size'] === 0 || $row['kind'] === 'empty')) {
        $row['kind'] = 'printed';
        $row['size'] = strlen($printed);
        $row['sha1'] = sha1($printed);
        $row['content_type'] = substr($printed, 0, 4) === '%PDF' ? 'application/pdf' : 'text/plain (printed)';
    }

    return parity_finish($app, $connection, $row, $started);
}

/** The app's own console commands, its schedule and its queued jobs (what can run outside a request). */
function parity_console_list($app, $kernel, $appPath)
{
    $commands = array();
    foreach ($kernel->all() as $name => $command) {
        $class = get_class($command);
        if (strpos($class, 'App\\') !== 0) {
            continue;
        }
        $required = array();
        foreach ($command->getDefinition()->getArguments() as $argument) {
            if ($argument->isRequired()) {
                $required[] = $argument->getName();
            }
        }
        $commands[] = array('name' => $name, 'class' => $class, 'required' => $required);
    }

    $schedule = array();
    try {
        foreach ($app->make('Illuminate\Console\Scheduling\Schedule')->events() as $event) {
            $schedule[] = array(
                'task' => method_exists($event, 'getSummaryForDisplay') ? $event->getSummaryForDisplay() : (string) $event->command,
                'command' => (string) $event->command,
                'expression' => $event->expression,
                'timezone' => is_object($event->timezone) ? $event->timezone->getName() : (string) $event->timezone,
                'without_overlapping' => (bool) $event->withoutOverlapping,
                'on_one_server' => (bool) $event->onOneServer,
            );
        }
    } catch (Throwable $e) {
        $schedule[] = array('error' => get_class($e).': '.$e->getMessage());
    }

    $jobs = array();
    if (is_dir("$appPath/app/Jobs")) {
        $files = new RecursiveIteratorIterator(new RecursiveDirectoryIterator("$appPath/app/Jobs", FilesystemIterator::SKIP_DOTS));
        foreach ($files as $file) {
            if (substr($file->getFilename(), -4) !== '.php') {
                continue;
            }
            $source = file_get_contents($file->getPathname());
            if (! preg_match('/^\s*namespace\s+([\w\\\\]+)\s*;/m', $source, $ns) || ! preg_match('/^\s*(?:final\s+|abstract\s+)*class\s+(\w+)/m', $source, $cl)) {
                continue;
            }
            $class = $ns[1].'\\'.$cl[1];
            try {
                if (! class_exists($class)) {
                    continue;
                }
                $reflection = new ReflectionClass($class);
                if ($reflection->isAbstract() || ! $reflection->hasMethod('handle')) {
                    continue;
                }
                $params = array();
                $constructor = $reflection->getConstructor();
                foreach ($constructor ? $constructor->getParameters() : array() as $param) {
                    $type = $param->getType();
                    $params[] = array(
                        'name' => $param->getName(),
                        'type' => $type ? ($type instanceof ReflectionNamedType ? $type->getName() : (string) $type) : null,
                        'optional' => $param->isOptional(),
                        'nullable' => $param->allowsNull(),
                    );
                }
                $jobs[] = array('class' => $class, 'params' => $params,
                    'queued' => $reflection->implementsInterface('Illuminate\Contracts\Queue\ShouldQueue'));
            } catch (Throwable $e) {
            }
        }
    }

    return array('commands' => $commands, 'schedule' => $schedule, 'jobs' => $jobs);
}

/** A job's constructor arguments: given values, else the newest row of a model, a default, or null. */
function parity_job_args(ReflectionClass $reflection, array $given)
{
    $args = array();
    $constructor = $reflection->getConstructor();
    foreach ($constructor ? $constructor->getParameters() : array() as $param) {
        $name = $param->getName();
        $type = $param->getType();
        $class = $type instanceof ReflectionNamedType && ! $type->isBuiltin() ? $type->getName() : null;
        if (array_key_exists($name, $given)) {
            $value = $given[$name];
            if ($class && is_subclass_of($class, 'Illuminate\Database\Eloquent\Model')) {
                $value = $class::find($value);
                if (! $value) {
                    return array(null, "no $class with id {$given[$name]}");
                }
            }
            $args[] = $value;
        } elseif ($class && is_subclass_of($class, 'Illuminate\Database\Eloquent\Model')) {
            $model = new $class();
            $row = $class::query()->orderBy($model->getKeyName(), 'desc')->first();
            if (! $row) {
                return array(null, "no row in ".$model->getTable()." for \$$name");
            }
            $args[] = $row;
        } elseif ($param->isOptional()) {
            $args[] = $param->getDefaultValue();
        } elseif ($type && $param->allowsNull()) {
            // only a declared ?Type: an untyped $data given null is a guess the job was never written for
            $args[] = null;
        } else {
            return array(null, "needs \$$name".($type ? ' ('.($type instanceof ReflectionNamedType ? $type->getName() : (string) $type).')' : ''));
        }
    }

    return array($args, null);
}

/** One command or job, in a rolled-back transaction with everything leaving the app faked. */
function parity_console_run($appPath, array $job, array $item)
{
    list($app, $kernel) = parity_boot($appPath, $job, true);
    parity_record($app, $job);
    $connection = $app['db']->connection();
    $row = array('id' => $item['id']);
    $instance = null;
    if ($item['kind'] === 'job') {
        $reflection = new ReflectionClass($item['class']);
        list($args, $why) = parity_job_args($reflection, isset($item['args']) ? (array) $item['args'] : array());
        if ($why !== null) {
            $row['status'] = 'not-run';
            $row['error'] = $why;

            return parity_finish($app, $connection, $row, microtime(true));
        }
        $instance = $reflection->newInstanceArgs($args);
    }
    $connection->beginTransaction();
    $started = microtime(true);
    $level = ob_get_level();
    ob_start();
    try {
        if ($instance) {
            $app->call(array($instance, 'handle'));
            $row['status'] = 0;
            $row['output'] = '';
        } else {
            $arguments = isset($item['arguments']) ? (array) $item['arguments'] : array();
            $arguments['--no-interaction'] = true;
            $row['status'] = $kernel->call($item['name'], $arguments);
            $row['output'] = substr($kernel->output(), 0, 4000);
        }
    } catch (Throwable $e) {
        $row['status'] = 'runner-error';
        $row['error'] = get_class($e).': '.$e->getMessage();
        $row['file'] = substr($e->getFile(), strlen($appPath) + 1).':'.$e->getLine();
    }
    $printed = '';
    while (ob_get_level() > $level) {
        $printed = ob_get_clean().$printed;
    }
    if ($printed !== '') {
        $row['output'] = (isset($row['output']) ? $row['output'] : '').substr($printed, 0, 2000);
    }

    return parity_finish($app, $connection, $row, $started);
}

/** Who may listen on each broadcast channel (routes/channels.php), asked the way /broadcasting/auth asks. */
function parity_channels($app, array $job)
{
    $manager = $app->make('Illuminate\Broadcasting\BroadcastManager');
    $patterns = array();
    $options = array();
    foreach (parity_prop($manager, 'drivers') as $driver) {
        if ($driver instanceof ParityBroadcaster) {
            continue;
        }
        $patterns = parity_prop($driver, 'channels');
        $options = parity_has(get_class($driver), 'channelOptions', false) ? parity_prop($driver, 'channelOptions') : array();
        if ($patterns) {
            break;
        }
    }
    $auth = new ParityBroadcaster();
    foreach (array('channels' => $patterns, 'channelOptions' => $options) as $name => $value) {
        if (parity_has('ParityBroadcaster', $name, false)) {
            $property = new ReflectionProperty('Illuminate\Broadcasting\Broadcasters\Broadcaster', $name);
            $property->setAccessible(true);
            $property->setValue($auth, $value);
        }
    }
    $verify = new ReflectionMethod($auth, 'verifyUserCanAccessChannel');
    $verify->setAccessible(true);
    // Pusher / Reverb refuse a channel before its callback when no user signs in through the channel's guards
    $retrieve = method_exists($auth, 'retrieveUser') ? new ReflectionMethod($auth, 'retrieveUser') : null;
    if ($retrieve) {
        $retrieve->setAccessible(true);
    }

    // a guest never reaches the channel callbacks: /broadcasting/auth refuses before them
    $who = array();
    foreach ((array) $job['users'] as $guard => $id) {
        $user = parity_user($app, $guard, $id);
        if ($user) {
            $who[$guard] = array($guard, $user);
        }
    }
    $names = array();
    foreach (array_keys($patterns) as $pattern) {
        if (strpos($pattern, '{') === false) {
            $names[$pattern] = $pattern;
        }
    }
    foreach ((array) $job['channels'] as $name) {
        $names[$name] = $name;
    }
    $answers = array();
    foreach ($who as $label => $pair) {
        $user = $pair ? $pair[1] : null;
        $list = $names;
        // a channel per user (App.Models.User.{id}, admin.{userId}): that user's own
        foreach (array_keys($patterns) as $pattern) {
            if ($user && preg_match('/^[^{]*\{\w+\}$/', $pattern)) {
                $list[preg_replace('/\{\w+\}$/', '{user}', $pattern)] = preg_replace('/\{\w+\}$/', (string) $user->getKey(), $pattern);
            }
        }
        foreach ($list as $key => $channel) {
            if ($user) {
                $app['auth']->guard($pair[0])->setUser($user);
            }
            $request = Illuminate\Http\Request::create('/broadcasting/auth', 'POST', array('channel_name' => 'private-'.$channel));
            // a channel limited to a guard (['guards' => ['admin']]) sees the user only through that guard
            $request->setUserResolver(function ($guard = null) use ($user, $label) {
                return $guard === null || $guard === $label ? $user : null;
            });
            try {
                if (! ($retrieve ? $retrieve->invoke($auth, $request, $channel) : $request->user())) {
                    $answers[] = array('channel' => $key, 'who' => $label, 'answer' => 'denied');
                    continue;
                }
                $result = $verify->invoke($auth, $request, $channel);
                $answer = $result === true ? 'allowed' : 'allowed '.json_encode(parity_value($result, 0));
            } catch (Symfony\Component\HttpKernel\Exception\AccessDeniedHttpException $e) {
                $answer = 'denied';
            } catch (Throwable $e) {
                $answer = 'error: '.get_class($e).': '.substr($e->getMessage(), 0, 200);
            }
            $answers[] = array('channel' => $key, 'who' => $label, 'answer' => $answer);
        }
    }
    $route = null;
    foreach ($app['router']->getRoutes() as $r) {
        // broadcasting/auth, or prefixed (Laravel 11 withBroadcasting: api/broadcasting/auth)
        if (substr($r->uri(), -strlen('broadcasting/auth')) === 'broadcasting/auth') {
            $route = array('uri' => $r->uri(), 'methods' => array_values(array_diff($r->methods(), array('HEAD'))), 'middleware' => array_values($r->gatherMiddleware()));
        }
    }
    $driver = $GLOBALS['parity_broadcast_default'];

    return array(
        'driver' => $driver,
        'driver_config' => array_keys(array_filter((array) $app['config']->get("broadcasting.connections.$driver", array()), function ($v) {
            return $v !== null && $v !== '';
        })),
        'patterns' => array_keys($patterns),
        'auth_route' => $route,
        'answers' => $answers,
    );
}

/** The app's own classes under app/ (PSR-4 from composer.json), with their public methods and parameters. */
function parity_units($appPath, array $job)
{
    $composer = json_decode((string) @file_get_contents("$appPath/composer.json"), true);
    $roots = array();
    foreach ((array) (isset($composer['autoload']['psr-4']) ? $composer['autoload']['psr-4'] : array()) as $prefix => $dirs) {
        foreach ((array) $dirs as $dir) {
            if (rtrim($dir, '/') === 'app') {
                $roots[$prefix] = rtrim($dir, '/');
            }
        }
    }
    if (! $roots) {
        $roots = array('App\\' => 'app');
    }
    $skip = isset($job['skip']) ? (array) $job['skip'] : array();
    $units = array();
    foreach ($roots as $prefix => $dir) {
        if (! is_dir("$appPath/$dir")) {
            continue;
        }
        $files = new RecursiveIteratorIterator(new RecursiveDirectoryIterator("$appPath/$dir", FilesystemIterator::SKIP_DOTS));
        foreach ($files as $file) {
            $path = $file->getPathname();
            if (substr($path, -4) !== '.php') {
                continue;
            }
            $relative = substr($path, strlen("$appPath/$dir/"), -4);
            $class = $prefix.str_replace('/', '\\', $relative);
            $folder = strpos($relative, '/') === false ? '' : substr($relative, 0, strrpos($relative, '/'));
            $skipped = false;
            foreach ($skip as $pattern) {
                if ($folder === $pattern || strpos($folder.'/', $pattern.'/') === 0) {
                    $skipped = true;
                }
            }
            if ($skipped) {
                continue;
            }
            // only a file that declares the class its path names: loading a copy (Report_old.php declaring Report)
            // would declare a class twice, a fatal error nothing can catch
            $code = (string) @file_get_contents($path);
            $base = basename($relative);
            $namespace = substr($class, 0, strrpos($class, '\\'));
            if (! preg_match('/^\s*(?:abstract\s+|final\s+|readonly\s+)*class\s+'.preg_quote($base, '/').'\b/m', $code)
                || ! preg_match('/^\s*namespace\s+'.preg_quote($namespace, '/').'\s*;/m', $code)) {
                continue;
            }
            try {
                if (! class_exists($class)) {
                    continue;
                }
                $reflection = new ReflectionClass($class);
            } catch (Throwable $e) {
                continue;
            }
            if ($reflection->isAbstract() || $reflection->isInterface() || $reflection->isTrait()) {
                continue;
            }
            $methods = array();
            foreach ($reflection->getMethods(ReflectionMethod::IS_PUBLIC) as $method) {
                $name = $method->getName();
                if ($method->getDeclaringClass()->getName() !== $class || strpos($name, '__') === 0 || $method->getFileName() === false) {
                    continue;
                }
                if (strpos($method->getFileName(), "$appPath/vendor/") === 0) {
                    continue;
                }
                $params = array();
                foreach ($method->getParameters() as $param) {
                    $params[] = parity_param($param);
                }
                $return = $method->getReturnType();
                $methods[] = array(
                    'name' => $name,
                    'static' => $method->isStatic(),
                    'line' => $method->getStartLine(),
                    'end' => $method->getEndLine(),
                    'params' => $params,
                    'returns' => $return ? parity_type_name($return) : null,
                );
            }
            $model = $reflection->isSubclassOf('Illuminate\Database\Eloquent\Model');
            $units[] = array(
                'class' => $class,
                'file' => "$dir/$relative.php",
                'kind' => $model ? 'model' : 'class',
                'methods' => $methods,
            );
        }
    }

    return $units;
}

function parity_type_name($type)
{
    if ($type instanceof ReflectionNamedType) {
        return ($type->allowsNull() && $type->getName() !== 'mixed' && $type->getName() !== 'null' ? '?' : '').$type->getName();
    }

    return (string) $type;
}

function parity_param(ReflectionParameter $param)
{
    $type = $param->getType();
    $row = array(
        'name' => $param->getName(),
        'type' => $type ? parity_type_name($type) : null,
        'optional' => $param->isOptional(),
        'nullable' => $type ? $type->allowsNull() : true,
        'variadic' => $param->isVariadic(),
    );
    if ($param->isDefaultValueAvailable()) {
        try {
            $row['default'] = parity_value($param->getDefaultValue(), 0);
        } catch (Throwable $e) {
        }
    }

    return $row;
}

/**
 * Arguments for a constructor or method the way a unit test would build them: values the job gives, the newest row
 * for a model, a request, now for a date, the container for other classes, the default, or null for a ?Type. Each
 * comes with how it was made, so the generated test can make it the same way. ($args, $how, $why)
 */
function parity_args($app, $function, array $given)
{
    $args = array();
    $how = array();
    if (! $function) {
        return array($args, $how, null);
    }
    foreach ($function->getParameters() as $param) {
        $name = $param->getName();
        $type = $param->getType();
        $class = $type instanceof ReflectionNamedType && ! $type->isBuiltin() ? $type->getName() : null;
        if ($param->isVariadic()) {
            break;
        }
        if (array_key_exists($name, $given)) {
            $value = $given[$name];
            if ($class && is_subclass_of($class, 'Illuminate\Database\Eloquent\Model') && ! is_object($value)) {
                $value = $class::find($value);
                if (! $value) {
                    return array(null, null, "no $class with id {$given[$name]}");
                }
            }
            $args[] = $value;
            $how[] = array('how' => 'given', 'value' => parity_value($given[$name], 0));
        } elseif ($class && is_subclass_of($class, 'Illuminate\Database\Eloquent\Model')) {
            $model = new $class();
            $row = $class::query()->orderBy($model->getKeyName(), 'desc')->first();
            if (! $row) {
                if ($param->isOptional() || $param->allowsNull()) {
                    $args[] = $param->isOptional() ? $param->getDefaultValue() : null;
                    $how[] = array('how' => 'null');
                    continue;
                }

                return array(null, null, 'no row in '.$model->getTable()." for \$$name");
            }
            $args[] = $row;
            $how[] = array('how' => 'model', 'class' => $class);
        } elseif ($class && ($class === 'Illuminate\Http\Request' || is_subclass_of($class, 'Illuminate\Http\Request'))) {
            $args[] = $class::create('/', 'GET');
            $how[] = array('how' => 'request', 'class' => $class);
        } elseif ($class && ($class === 'DateTimeInterface' || is_subclass_of($class, 'DateTimeInterface') || $class === 'Carbon\Carbon' || $class === 'Illuminate\Support\Carbon')) {
            $args[] = Illuminate\Support\Carbon::now();
            $how[] = array('how' => 'now');
        } elseif ($param->isOptional()) {
            $args[] = $param->getDefaultValue();
            $how[] = array('how' => 'default', 'value' => parity_value($param->getDefaultValue(), 0));
        } elseif ($class && $class !== 'Closure') {
            try {
                $args[] = $app->make($class);
                $how[] = array('how' => 'make', 'class' => $class);
            } catch (Throwable $e) {
                if ($param->allowsNull()) {
                    $args[] = null;
                    $how[] = array('how' => 'null');
                } else {
                    return array(null, null, "needs \$$name ($class)");
                }
            }
        } elseif ($type && $param->allowsNull()) {
            $args[] = null;
            $how[] = array('how' => 'null');
        } else {
            return array(null, null, "needs \$$name".($type ? ' ('.parity_type_name($type).')' : ''));
        }
    }

    return array($args, $how, null);
}

/** What a method gave back: its type, a JSON copy, and the keys or the related class a test can check. */
function parity_result($value)
{
    $row = array('type' => is_object($value) ? get_class($value) : gettype($value));
    if ($value instanceof Illuminate\Database\Eloquent\Relations\Relation) {
        $row['related'] = get_class($value->getRelated());
        try {
            $row['count'] = $value->count();
        } catch (Throwable $e) {
            $row['count'] = 'error: '.get_class($e);
        }

        return $row;
    }
    if ($value instanceof Illuminate\Database\Eloquent\Builder || $value instanceof Illuminate\Database\Query\Builder) {
        try {
            $row['count'] = $value->count();
        } catch (Throwable $e) {
            $row['count'] = 'error: '.get_class($e);
        }

        return $row;
    }
    $copy = parity_value($value, 0);
    $row['value'] = $copy;
    if (is_array($copy)) {
        $keys = array();
        foreach (array_keys($copy) as $key) {
            if (is_string($key)) {
                $keys[] = $key;
            }
        }
        $row['keys'] = $keys;
    }

    return $row;
}

/** A model's own settings, the same on every row: what the upgrade's $dates -> $casts and similar moves change. */
function parity_model_config($model)
{
    $casts = $model->getCasts();
    if (method_exists($model, 'getDates')) {
        foreach ($model->getDates() as $date) {
            if (! isset($casts[$date])) {
                $casts[$date] = 'datetime';
            }
        }
    }
    ksort($casts);
    $fillable = $model->getFillable();
    sort($fillable);
    $hidden = $model->getHidden();
    sort($hidden);

    return array(
        'table' => $model->getTable(),
        'key' => $model->getKeyName(),
        'fillable' => $fillable,
        'hidden' => $hidden,
        'casts' => $casts,
    );
}

/** One method of one class, on the newest row for a model; rolled back with everything leaving the app faked. */
function parity_probe($app, $appPath, array $job, array $item)
{
    $row = array('id' => $item['id']);
    $class = $item['class'];
    $method = $item['method'];
    $given = isset($item['args']) ? (array) $item['args'] : array();
    parity_record($app, $job);
    $connection = $app['db']->connection();
    $connection->beginTransaction();
    $started = microtime(true);
    $level = ob_get_level();
    ob_start();
    try {
        if (isset($item['code'])) {
            $function = eval('return function ($app) { '.$item['code'].' };');
            $row += array('status' => 'ok') + parity_result($function($app));
        } else {
            $reflection = new ReflectionClass($class);
            $model = $reflection->isSubclassOf('Illuminate\Database\Eloquent\Model');
            $instance = null;
            if ($model) {
                $blank = new $class();
                $instance = $class::query()->orderBy($blank->getKeyName(), 'desc')->first();
                $row['instance'] = $instance ? 'newest' : 'new';
                if (! $instance) {
                    $instance = $blank;
                }
            } elseif ($method !== '@config' && $reflection->getMethod($method)->isStatic()) {
                $row['static'] = true;
            } elseif ($method !== '@config') {
                list($args, $how, $why) = parity_args($app, $reflection->getConstructor(), isset($item['construct']) ? (array) $item['construct'] : array());
                if ($why !== null) {
                    $row['status'] = 'not-run';
                    $row['error'] = $why;
                } else {
                    $row['construct'] = $how;
                    $instance = $reflection->newInstanceArgs($args);
                }
            }
            if (! isset($row['status'])) {
                if ($method === '@config') {
                    $row += array('status' => 'ok', 'type' => 'config', 'value' => parity_model_config($instance));
                } elseif ($model && strpos($method, 'scope') === 0 && strlen($method) > 5) {
                    // a scope runs through the query builder, without its first ($query) parameter
                    $function = new ReflectionMethod($class, $method);
                    $params = $function->getParameters();
                    array_shift($params);
                    list($args, $how, $why) = parity_args_list($app, $params, $given);
                    if ($why !== null) {
                        $row += array('status' => 'not-run', 'error' => $why);
                    } else {
                        $row['args'] = $how;
                        $query = call_user_func_array(array($class::query(), lcfirst(substr($method, 5))), $args);
                        $row += array('status' => 'ok') + parity_result($query);
                    }
                } elseif ($model && preg_match('/^get(\w+)Attribute$/', $method, $m) && $m[1] !== '') {
                    $attribute = Illuminate\Support\Str::snake($m[1]);
                    $row['attribute'] = $attribute;
                    $row += array('status' => 'ok') + parity_result($instance->getAttribute($attribute));
                } elseif ($model && preg_match('/^set\w+Attribute$/', $method)) {
                    $row += array('status' => 'not-run', 'error' => 'a mutator runs when the attribute is saved');
                } else {
                    $function = new ReflectionMethod($class, $method);
                    list($args, $how, $why) = parity_args($app, $function, $given);
                    if ($why !== null) {
                        $row += array('status' => 'not-run', 'error' => $why);
                    } else {
                        $row['args'] = $how;
                        $result = $function->isStatic() ? $function->invokeArgs(null, $args) : $function->invokeArgs($instance, $args);
                        $row += array('status' => 'ok') + parity_result($result);
                    }
                }
            }
        }
    } catch (Throwable $e) {
        $row['status'] = 'threw';
        $row['error'] = get_class($e).': '.substr($e->getMessage(), 0, 400);
        $file = $e->getFile();
        $row['file'] = (strpos($file, $appPath.'/') === 0 ? substr($file, strlen($appPath) + 1) : $file).':'.$e->getLine();
    }
    while (ob_get_level() > $level) {
        ob_end_clean();
    }
    $row['ms'] = (int) round((microtime(true) - $started) * 1000);
    while ($connection->transactionLevel() > 0) {
        $connection->rollBack();
    }
    $row['queries'] = ParityRecorder::$queries;
    $row['effects'] = parity_effects($app, ParityRecorder::$writes, ParityRecorder::$logs);
    foreach (array('Illuminate\Support\Carbon', 'Carbon\Carbon', 'Carbon\CarbonImmutable') as $carbon) {
        if (class_exists($carbon)) {
            $carbon::setTestNow();
        }
    }

    return $row;
}

/** parity_args for a list of parameters (a scope without its $query). */
function parity_args_list($app, array $params, array $given)
{
    $fake = new ParityParams($params);

    return parity_args($app, $fake, $given);
}

/** Stands in for a ReflectionFunctionAbstract where only getParameters() is read. */
class ParityParams
{
    private $params;

    public function __construct(array $params)
    {
        $this->params = $params;
    }

    public function getParameters()
    {
        return $this->params;
    }
}

$mode = isset($job['mode']) ? $job['mode'] : 'replay';

if ($mode === 'units') {
    list($app, $kernel) = parity_boot($appPath, $job, true);
    parity_emit($out, array('info' => parity_info($app), 'units' => parity_units($appPath, $job)));
    exit(0);
}

if ($mode === 'probe') {
    // one app for the whole batch; each method runs in its own rolled-back transaction
    $seconds = isset($job['timeout']) ? (int) $job['timeout'] : 30;
    list($app, $kernel) = parity_boot($appPath, $job, true);
    foreach ((array) $job['items'] as $item) {
        if (function_exists('pcntl_async_signals') && function_exists('pcntl_alarm')) {
            pcntl_async_signals(true);
            pcntl_signal(SIGALRM, function () use ($out, $item, $seconds) {
                parity_emit($out, array('id' => $item['id'], 'status' => 'timeout', 'error' => "still running after {$seconds}s"));
                exit(75);
            });
            pcntl_alarm($seconds);
        }
        try {
            parity_emit($out, parity_probe($app, $appPath, $job, $item));
        } catch (Throwable $e) {
            parity_emit($out, array('id' => $item['id'], 'status' => 'runner-error', 'error' => get_class($e).': '.$e->getMessage()));
        }
        if (function_exists('pcntl_alarm')) {
            pcntl_alarm(0);
        }
        if (headers_sent()) {
            exit(75);
        }
    }
    exit(0);
}

if ($mode === 'artisan') {
    list($app, $kernel) = parity_boot($appPath, $job, true);
    parity_emit($out, array('info' => parity_info($app)));
    foreach ((array) $job['commands'] as $command) {
        $name = $command[0];
        $arguments = isset($command[1]) ? (array) $command[1] : array();
        try {
            $code = $kernel->call($name, $arguments);
            parity_emit($out, array('command' => $name, 'exit' => $code, 'output' => $kernel->output()));
        } catch (Throwable $e) {
            parity_emit($out, array('command' => $name, 'exit' => 1, 'output' => get_class($e).': '.$e->getMessage()));
        }
    }
    exit(0);
}

if ($mode === 'console') {
    list($app, $kernel) = parity_boot($appPath, $job, true);
    parity_emit($out, array('info' => parity_info($app), 'console' => parity_console_list($app, $kernel, $appPath)));
    exit(0);
}

if ($mode === 'console-run') {
    $seconds = isset($job['timeout']) ? (int) $job['timeout'] : 120;
    set_time_limit($seconds);
    foreach ((array) $job['items'] as $item) {
        if (function_exists('pcntl_async_signals') && function_exists('pcntl_alarm')) {
            pcntl_async_signals(true);
            pcntl_signal(SIGALRM, function () use ($out, $item, $seconds) {
                parity_emit($out, array('id' => $item['id'], 'status' => 'timeout', 'error' => "still running after {$seconds}s"));
                exit(0);
            });
            pcntl_alarm($seconds);
        }
        try {
            parity_emit($out, parity_console_run($appPath, $job, $item));
        } catch (Throwable $e) {
            parity_emit($out, array('id' => $item['id'], 'status' => 'runner-error', 'error' => get_class($e).': '.$e->getMessage()));
        }
        if (function_exists('pcntl_alarm')) {
            pcntl_alarm(0);
        }
    }
    exit(0);
}

if ($mode === 'channels') {
    list($app, $kernel) = parity_boot($appPath, $job, false);
    parity_emit($out, array('info' => parity_info($app), 'channels' => parity_channels($app, $job)));
    exit(0);
}

if ($mode === 'lint') {
    // every PHP file of the app, parsed by this PHP (the app's own version): what it cannot parse fails at runtime
    $errors = array();
    $count = 0;
    $dirs = isset($job['dirs']) ? (array) $job['dirs'] : array('app', 'routes', 'database', 'config', 'bootstrap');
    foreach ($dirs as $dir) {
        if (! is_dir("$appPath/$dir")) {
            continue;
        }
        $files = new RecursiveIteratorIterator(new RecursiveDirectoryIterator("$appPath/$dir", FilesystemIterator::SKIP_DOTS));
        foreach ($files as $file) {
            if (substr($file->getFilename(), -4) !== '.php' || strpos($file->getPathname(), '/cache/') !== false) {
                continue;
            }
            $count++;
            try {
                token_get_all(file_get_contents($file->getPathname()), TOKEN_PARSE);
            } catch (ParseError $e) {
                $errors[] = array(
                    'file' => substr($file->getPathname(), strlen($appPath) + 1),
                    'line' => $e->getLine(),
                    'message' => $e->getMessage(),
                );
            }
        }
    }
    parity_emit($out, array('lint' => array('php' => PHP_VERSION, 'files' => $count, 'errors' => $errors)));
    exit(0);
}

if ($mode === 'routes') {
    // the routes the app really has once booted, package routes (Passport, Sanctum ...) included
    list($app, $kernel) = parity_boot($appPath, $job, false);
    $routes = array();
    foreach ($app['router']->getRoutes() as $route) {
        $routes[] = array('methods' => array_values(array_diff($route->methods(), array('HEAD'))), 'uri' => $route->uri());
    }
    parity_emit($out, array('info' => parity_info($app), 'routes' => $routes));
    exit(0);
}

list($app, $kernel) = parity_boot($appPath, $job, false);
list($tokens, $notes) = parity_tokens($app, $job);
parity_emit($out, array('info' => parity_info($app), 'auth' => $notes));
$app->flush();

foreach ((array) $job['requests'] as $spec) {
    try {
        parity_emit($out, parity_request($appPath, $job, $spec, $tokens));
    } catch (Throwable $e) {
        parity_emit($out, array('id' => $spec['id'], 'status' => 'runner-error', 'error' => get_class($e).': '.$e->getMessage()));
    }
    // a library closed our output buffer and printed straight out (SimpleXLSXGen downloadAs): from now on PHP
    // reports headers as sent for every request in this process. Stop; the tool runs the rest in a fresh process.
    if (headers_sent()) {
        exit(75);
    }
}
