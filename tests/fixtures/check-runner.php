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
$expect = ['ping' => 200, 'valid' => 200, 'invalid' => 422];
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

if ($failures) {
    fwrite(STDERR, "\n".implode("\n", $failures)."\n");
    exit(1);
}
echo "runner OK\n";
