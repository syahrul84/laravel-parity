<?php

// Written by laravel-parity (https://github.com/syahrul84/laravel-parity).
//
// Records the requests that saved something (POST, PUT, PATCH, DELETE answered 2xx) to
// storage/logs/parity-requests.jsonl, so laravel-parity can send the same saves to the old and the upgraded app on
// its test databases. Values whose name looks like a secret (password, token, card ...) and uploaded files are left
// out. At most PER_ROUTE different requests per route are kept.
//
// Install it on a TEST or STAGING copy of the OLD app, use the app for a while (or let testers use it), copy the file
// to your machine, run `laravel-parity import-log <pair file> parity-requests.jsonl`, then remove this middleware.

namespace App\Http\Middleware;

use Closure;

class ParityRecorder
{
    const PER_ROUTE = 5;

    const SECRET = '/token|secret|password|passwd|signature|api_?key|auth|session|otp|pin$|card|cvv|cvc|iban|bank_?acc/i';

    public function handle($request, Closure $next)
    {
        $response = $next($request);
        try {
            $this->record($request, $response);
        } catch (\Throwable $e) {
            // recording must never break the app
        }

        return $response;
    }

    private function record($request, $response)
    {
        $method = strtoupper($request->getMethod());
        if (! in_array($method, array('POST', 'PUT', 'PATCH', 'DELETE'), true)) {
            return;
        }
        $status = $response->getStatusCode();
        if ($status < 200 || $status >= 300) {
            return;
        }
        $route = $request->route();
        if (! is_object($route) || ! method_exists($route, 'uri')) {
            return;
        }
        $uri = $route->uri();
        $parameters = method_exists($route, 'originalParameters') ? $route->originalParameters() : $route->parameters();
        $values = array();
        foreach ((array) $parameters as $name => $value) {
            if (is_scalar($value)) {
                $values[$name] = (string) $value;
            }
        }
        $body = $request->isJson() ? $request->json()->all() : $request->request->all();
        $entry = array(
            'method' => $method,
            'route' => $uri,
            'values' => $values,
            'query' => $this->clean($request->query->all(), 0),
            'body' => $this->clean($body, 0),
            'status' => $status,
        );

        $dir = storage_path('logs');
        $file = $dir.'/parity-requests.jsonl';
        $seen = $dir.'/parity-requests.seen';
        $key = $method.' '.$uri;
        $shape = md5(json_encode(array_keys((array) $entry['body'])).json_encode(array_keys((array) $entry['query'])));
        $handle = fopen($seen, 'c+');
        if (! $handle) {
            return;
        }
        flock($handle, LOCK_EX);
        $known = json_decode((string) stream_get_contents($handle), true);
        $known = is_array($known) ? $known : array();
        $shapes = isset($known[$key]) ? $known[$key] : array();
        // a new combination of fields is worth keeping; the same fields again only until the route has enough
        if (count($shapes) < self::PER_ROUTE || (! in_array($shape, $shapes, true) && count($shapes) < self::PER_ROUTE * 2)) {
            $shapes[] = $shape;
            $known[$key] = $shapes;
            file_put_contents($file, json_encode($entry)."\n", FILE_APPEND | LOCK_EX);
            ftruncate($handle, 0);
            rewind($handle);
            fwrite($handle, json_encode($known));
        }
        flock($handle, LOCK_UN);
        fclose($handle);
    }

    private function clean($data, $depth)
    {
        if (! is_array($data)) {
            if (is_string($data) && strlen($data) > 2000) {
                return substr($data, 0, 2000);
            }

            return is_object($data) ? null : $data;
        }
        if ($depth > 6) {
            return array();
        }
        $out = array();
        foreach ($data as $key => $value) {
            if (is_string($key) && preg_match(self::SECRET, $key)) {
                continue;
            }
            if (is_object($value)) {
                // an uploaded file
                continue;
            }
            $out[$key] = $this->clean($value, $depth + 1);
        }

        return $out;
    }
}
