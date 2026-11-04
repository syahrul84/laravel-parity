<?php

// CI: run php/replay.php against a fresh Laravel app (SQLite) and fail when a Laravel internal the runner reads is
// missing, or a request does not replay as expected. This is what warns about a new Laravel version before release.
//
//   php tests/fixtures/check-runner.php /path/to/laravel-app

$app = rtrim($argv[1] ?? '', '/');
if ($app === '' || ! is_file("$app/artisan")) {
    fwrite(STDERR, "usage: php check-runner.php <laravel app>\n");
    exit(2);
}
$runner = dirname(__DIR__, 2).'/php/replay.php';
$dir = sys_get_temp_dir().'/parity-ci-'.getmypid();
@mkdir($dir);
$sqlite = "$dir/parity.sqlite";
touch($sqlite);

file_put_contents("$dir/job.json", json_encode([
    'mode' => 'replay',
    'clock' => '2026-01-01 09:00:00',
    'users' => new stdClass(),
    'headers' => new stdClass(),
    'database' => ['driver' => 'sqlite', 'database' => $sqlite],
    'requests' => [
        ['id' => 'ping', 'method' => 'GET', 'uri' => 'parity/ping'],
        ['id' => 'valid', 'method' => 'GET', 'uri' => 'parity/validate', 'query' => ['name' => 'ok']],
        ['id' => 'invalid', 'method' => 'GET', 'uri' => 'parity/validate'],
        // a request that changes a PHP setting, then one that must still start the app (its .env parses)
        ['id' => 'spoil', 'method' => 'GET', 'uri' => 'parity/spoil'],
        ['id' => 'after', 'method' => 'GET', 'uri' => 'parity/ping'],
        // the app ends the process: recorded as this request's answer, the app's doing (last: nothing runs after it)
        ['id' => 'exit', 'method' => 'GET', 'uri' => 'parity/exit'],
    ],
]));

passthru(sprintf('php %s --app=%s --job=%s --out=%s', escapeshellarg($runner), escapeshellarg($app), escapeshellarg("$dir/job.json"), escapeshellarg("$dir/out.jsonl")), $code);
$rows = [];
foreach (file("$dir/out.jsonl") ?: [] as $line) {
    $row = json_decode($line, true);
    $rows[$row['id'] ?? 'info'] = $row;
}

$failures = [];
$info = $rows['info']['info'] ?? [];
echo 'Laravel '.($info['laravel'] ?? '?').', PHP '.PHP_VERSION.PHP_EOL;
foreach ($info['capabilities'] ?? [] as $name => $ok) {
    echo ($ok ? 'ok      ' : 'MISSING ')."$name\n";
    if (! $ok) {
        $failures[] = "capability $name is missing: the runner cannot record it on this Laravel version";
    }
}
if (empty($info['capabilities'])) {
    $failures[] = 'the runner did not report its capabilities (exit '.$code.')';
}
// spoil's own answer is whatever the app does with that setting; the next request must not inherit it
$expect = ['ping' => 200, 'valid' => 200, 'invalid' => 422, 'after' => 200];
foreach ($expect as $id => $status) {
    $got = $rows[$id]['status'] ?? 'nothing';
    echo "$id: $got (expected $status)\n";
    if ($got !== $status) {
        $failures[] = "$id answered $got, expected $status".(isset($rows[$id]['error']) ? ': '.$rows[$id]['error'] : '');
    }
}
if (($rows['invalid']['json']['errors']['name'] ?? null) === null) {
    $failures[] = 'the 422 answer has no validation error for name';
}
$exit = $rows['exit'] ?? [];
echo 'exit: '.($exit['error'] ?? 'nothing')." (exit code $code)\n";
if (($exit['cause'] ?? null) !== 'app' || empty($exit['stopped']) || strpos($exit['error'] ?? '', 'bye') === false || $code !== 75) {
    $failures[] = 'the request that called exit was not recorded as the app ending the process (with what it printed, exit 75): '.json_encode($exit);
}

// the other modes: list commands / jobs / schedule, run one command, ask the broadcast channels
$modes = [
    'console' => ['mode' => 'console'],
    'console-run' => ['mode' => 'console-run', 'clock' => '2026-01-01 09:00:00', 'timeout' => 60, 'users' => new stdClass(),
        'items' => [['id' => 'env', 'kind' => 'command', 'name' => 'env']]],
    'channels' => ['mode' => 'channels', 'users' => new stdClass(), 'channels' => []],
    'units' => ['mode' => 'units', 'skip' => ['Http/Controllers', 'Providers']],
    'probe' => ['mode' => 'probe', 'clock' => '2026-01-01 09:00:00', 'timeout' => 30, 'users' => new stdClass(),
        'items' => [['id' => 'code', 'class' => '', 'method' => '', 'code' => 'return ["sum" => 1 + 1, "now" => now()->toDateString()];']]],
];
foreach ($modes as $name => $job) {
    $job['database'] = ['driver' => 'sqlite', 'database' => $sqlite];
    file_put_contents("$dir/$name.json", json_encode($job));
    @unlink("$dir/$name.jsonl");
    passthru(sprintf('php %s --app=%s --job=%s --out=%s', escapeshellarg($runner), escapeshellarg($app), escapeshellarg("$dir/$name.json"), escapeshellarg("$dir/$name.jsonl")), $code);
    $lines = array_map(fn ($l) => json_decode($l, true), file("$dir/$name.jsonl") ?: []);
    $ok = match ($name) {
        'console' => is_array($lines[0]['console']['commands'] ?? null) && is_array($lines[0]['console']['schedule'] ?? null),
        'console-run' => ($lines[0]['status'] ?? null) === 0,
        'channels' => is_array($lines[0]['channels']['answers'] ?? null),
        'units' => in_array('App\\Models\\User', array_column($lines[0]['units'] ?? [], 'class'), true),
        'probe' => ($lines[0]['status'] ?? null) === 'ok' && ($lines[0]['value'] ?? null) === ['sum' => 2, 'now' => '2026-01-01']
            && ($lines[0]['keys'] ?? null) === ['sum', 'now'],
    };
    echo "$name: ".($ok ? 'ok' : 'FAILED')."\n";
    if (! $ok) {
        $failures[] = "mode $name did not answer as expected (exit $code): ".json_encode($lines[0] ?? null);
    }
}

// the recorder middleware (laravel-parity recorder): a save through it lands in storage/logs/parity-requests.jsonl
$recorder = dirname(__DIR__, 2).'/php/ParityRecorder.php';
file_put_contents("$dir/recorder.php", '<?php
require '.var_export($recorder, true).';
require '.var_export("$app/vendor/autoload.php", true).';
$app = require '.var_export("$app/bootstrap/app.php", true).';
$kernel = $app->make(Illuminate\Contracts\Http\Kernel::class);
$app->make(Illuminate\Contracts\Console\Kernel::class)->bootstrap();
@unlink(storage_path("logs/parity-requests.jsonl"));
@unlink(storage_path("logs/parity-requests.seen"));
$app["router"]->put("parity/units/{unit}", function () { return ["ok" => true]; })
    ->middleware(App\Http\Middleware\ParityRecorder::class);
$request = Illuminate\Http\Request::create("/parity/units/7", "PUT", [], [], [], ["CONTENT_TYPE" => "application/json"],
    json_encode(["name" => "Block A", "password" => "x"]));
$kernel->handle($request);
echo file_get_contents(storage_path("logs/parity-requests.jsonl"));
@unlink(storage_path("logs/parity-requests.jsonl"));
@unlink(storage_path("logs/parity-requests.seen"));
');
$recorded = json_decode(trim((string) shell_exec(sprintf('php %s 2>&1', escapeshellarg("$dir/recorder.php")))), true);
$ok = ($recorded['route'] ?? null) === 'parity/units/{unit}' && ($recorded['values']['unit'] ?? null) === '7'
    && ($recorded['body'] ?? null) === ['name' => 'Block A'];
echo 'recorder: '.($ok ? 'ok' : 'FAILED')."\n";
if (! $ok) {
    $failures[] = 'the recorder middleware did not record the save as expected: '.json_encode($recorded);
}

// the coverage reader (tests step): PHPUnit's --coverage-php files, merged and summed by php/coverage.php
if (extension_loaded('pcov')) {
    $script = dirname(__DIR__, 2).'/php/coverage.php';
    $phpunit = shell_exec(sprintf('cd %s && php -d pcov.enabled=1 -d pcov.directory=%s vendor/bin/phpunit --coverage-php %s tests/Unit 2>&1',
        escapeshellarg($app), escapeshellarg("$app/app"), escapeshellarg("$dir/own.cov")));
    @unlink("$dir/merged.cov");
    shell_exec(sprintf('php %s --app=%s merge %s %s 2>&1', escapeshellarg($script), escapeshellarg($app), escapeshellarg("$dir/merged.cov"), escapeshellarg("$dir/own.cov")));
    $summary = json_decode((string) shell_exec(sprintf('php %s --app=%s summary %s %s', escapeshellarg($script), escapeshellarg($app),
        escapeshellarg("$dir/own.cov"), escapeshellarg("$dir/merged.cov"))), true);
    $ok = ($summary['own']['lines'] ?? 0) > 0 && ($summary['with']['lines'] ?? 0) === $summary['own']['lines']
        && is_array($summary['areas'] ?? null);
    echo 'coverage: '.($ok ? 'ok' : 'FAILED')."\n";
    if (! $ok) {
        $failures[] = 'php/coverage.php did not read the coverage PHPUnit wrote: '.json_encode($summary)."\nPHPUnit said:\n".$phpunit;
    }
} else {
    echo "coverage: skipped (no pcov)\n";
}

if ($failures) {
    fwrite(STDERR, "\n".implode("\n", $failures)."\n");
    exit(1);
}
echo "runner OK\n";
