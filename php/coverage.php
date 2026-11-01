<?php

// Written by laravel-parity (https://github.com/syahrul84/laravel-parity).
//
// Reads the files PHPUnit writes with --coverage-php, with the app's own php-code-coverage:
//
//   php coverage.php --app=<app> merge <out> <in>          add <in> to <out> (copied when <out> does not exist yet)
//   php coverage.php --app=<app> summary <own|-> <generated|-> [<more>...]
//   php coverage.php --app=<app> gaps <limit> <filter|-> <file>...
//
// summary prints JSON: the lines of app/ the app's own tests cover, the generated ones (with any more files), both
// together, and the same per area (Http/Controllers, Models ...). gaps prints the methods with the most lines no
// test runs, with those lines. PHP 7.1 syntax: the new app of an upgrade can be an old Laravel too.

ini_set('memory_limit', '-1');
error_reporting(E_ALL & ~E_DEPRECATED & ~E_USER_DEPRECATED);

$app = null;
$words = array();
foreach (array_slice($argv, 1) as $arg) {
    if (strpos($arg, '--app=') === 0) {
        $app = rtrim(substr($arg, 6), '/');
    } else {
        $words[] = $arg;
    }
}
if ($app === null || count($words) < 3) {
    fwrite(STDERR, "usage: php coverage.php --app=<app> merge <out> <in> | summary <own|-> <generated|-> [<more>...] | gaps <limit> <filter|-> <file>...\n");
    exit(2);
}
require $app.'/vendor/autoload.php';

function parity_load($file)
{
    if ($file === '-' || ! is_file($file)) {
        return null;
    }
    $coverage = include $file;

    return is_object($coverage) ? $coverage : null;
}

function parity_save($coverage, $file)
{
    if (class_exists('SebastianBergmann\CodeCoverage\Report\PHP')) {
        $writer = new SebastianBergmann\CodeCoverage\Report\PHP();
    } else {
        $writer = new PHP_CodeCoverage_Report_PHP();
    }
    $writer->process($coverage, $file);
}

// path => array(executable lines, covered lines), for every file the report knows
function parity_files($coverage)
{
    $out = array();
    if ($coverage === null) {
        return $out;
    }
    foreach ($coverage->getReport() as $node) {
        if (! ($node instanceof SebastianBergmann\CodeCoverage\Node\File) && ! ($node instanceof PHP_CodeCoverage_Report_Node_File)) {
            continue;
        }
        $path = method_exists($node, 'pathAsString') ? $node->pathAsString() : $node->getPath();
        $lines = method_exists($node, 'numberOfExecutableLines') ? $node->numberOfExecutableLines() : $node->getNumExecutableLines();
        $covered = method_exists($node, 'numberOfExecutedLines') ? $node->numberOfExecutedLines() : $node->getNumExecutedLines();
        $out[$path] = array($lines, $covered);
    }

    return $out;
}

// Http/Controllers/Admin/UnitController.php -> Http/Controllers; Models/Unit.php -> Models
function parity_area($path, $app)
{
    $path = str_replace('\\', '/', $path);
    foreach (array($app.'/app/', '/app/') as $prefix) {
        $at = strpos($path, $prefix);
        if ($at !== false) {
            $path = substr($path, $at + strlen($prefix));
            break;
        }
    }
    $parts = explode('/', $path);
    array_pop($parts);
    if (! $parts) {
        return '(app)';
    }

    return implode('/', array_slice($parts, 0, 2));
}

if ($words[0] === 'merge') {
    $out = parity_load($words[1]);
    foreach (array_slice($words, 2) as $file) {
        $in = parity_load($file);
        if ($in === null) {
            continue;
        }
        if ($out === null) {
            $out = $in;
        } else {
            $out->merge($in);
        }
    }
    if ($out !== null) {
        parity_save($out, $words[1]);
    }
    exit(0);
}

if ($words[0] === 'gaps') {
    $limit = (int) $words[1];
    $filter = $words[2] === '-' ? '' : str_replace('\\', '/', $words[2]);
    $all = null;
    foreach (array_slice($words, 3) as $file) {
        $one = parity_load($file);
        if ($one === null) {
            continue;
        }
        if ($all === null) {
            $all = $one;
        } else {
            $all->merge($one);
        }
    }
    $gaps = array();
    if ($all !== null) {
        foreach ($all->getReport() as $node) {
            if (! ($node instanceof SebastianBergmann\CodeCoverage\Node\File) && ! ($node instanceof PHP_CodeCoverage_Report_Node_File)) {
                continue;
            }
            $path = str_replace('\\', '/', method_exists($node, 'pathAsString') ? $node->pathAsString() : $node->getPath());
            $at = strpos($path, $app.'/');
            $relative = $at === 0 ? substr($path, strlen($app) + 1) : $path;
            if ($filter !== '' && stripos($relative, $filter) === false) {
                continue;
            }
            $lines = method_exists($node, 'lineCoverageData') ? $node->lineCoverageData() : $node->getCoverageData();
            $classes = method_exists($node, 'classesAndTraits') ? $node->classesAndTraits() : $node->getClassesAndTraits();
            foreach ($classes as $className => $class) {
                // php-code-coverage 12 gives objects (ProcessedClassType ...) with the keys as public properties
                $class = is_object($class) ? get_object_vars($class) : $class;
                $namespace = isset($class['namespace']) && $class['namespace'] !== '' ? $class['namespace'].'\\' : '';
                $full = strpos($className, '\\') === false ? $namespace.$className : $className;
                foreach ($class['methods'] as $methodName => $method) {
                    $method = is_object($method) ? get_object_vars($method) : $method;
                    $missed = array();
                    for ($line = $method['startLine']; $line <= $method['endLine']; $line++) {
                        if (array_key_exists($line, $lines) && is_array($lines[$line]) && count($lines[$line]) === 0) {
                            $missed[] = $line;
                        }
                    }
                    if (! $missed) {
                        continue;
                    }
                    // 12, 13, 14, 20 -> "12-14, 20"
                    $ranges = array();
                    $start = $prev = array_shift($missed);
                    $count = 1;
                    foreach ($missed as $line) {
                        $count++;
                        if ($line !== $prev + 1) {
                            $ranges[] = $start === $prev ? (string) $start : "$start-$prev";
                            $start = $line;
                        }
                        $prev = $line;
                    }
                    $ranges[] = $start === $prev ? (string) $start : "$start-$prev";
                    $gaps[] = array(
                        'class' => $full,
                        'method' => $methodName,
                        'file' => $relative,
                        'line' => $method['startLine'],
                        'lines' => $method['executableLines'],
                        'covered' => $method['executedLines'],
                        'missed' => $count,
                        'missed_lines' => implode(', ', $ranges),
                    );
                }
            }
        }
    }
    usort($gaps, function ($a, $b) {
        return $b['missed'] - $a['missed'];
    });
    echo json_encode(array('total' => count($gaps), 'gaps' => array_slice($gaps, 0, $limit)));
    exit(0);
}

$own = parity_load($words[1]);
$generated = parity_load($words[2]);
foreach (array_slice($words, 3) as $file) {
    $more = parity_load($file);
    if ($more !== null) {
        if ($generated === null) {
            $generated = $more;
        } else {
            $generated->merge($more);
        }
    }
}
$ownFiles = parity_files($own);
$generatedFiles = parity_files($generated);
if ($own !== null && $generated !== null) {
    $own->merge($generated);
    $bothFiles = parity_files($own);
} else {
    $bothFiles = $ownFiles ? $ownFiles : $generatedFiles;
}

// a file only one run loaded still counts in both: its lines are the merged run's
$totals = array('own' => array(0, 0), 'generated' => array(0, 0), 'with' => array(0, 0));
$areas = array();
foreach ($bothFiles as $path => $both) {
    $area = parity_area($path, $app);
    if (! isset($areas[$area])) {
        $areas[$area] = array('area' => $area, 'lines' => 0, 'own' => 0, 'generated' => 0, 'with' => 0);
    }
    $ownCovered = isset($ownFiles[$path]) ? $ownFiles[$path][1] : 0;
    $generatedCovered = isset($generatedFiles[$path]) ? $generatedFiles[$path][1] : 0;
    $areas[$area]['lines'] += $both[0];
    $areas[$area]['own'] += $ownCovered;
    $areas[$area]['generated'] += $generatedCovered;
    $areas[$area]['with'] += $both[1];
    $totals['own'][1] += $ownCovered;
    $totals['generated'][1] += $generatedCovered;
    $totals['with'][1] += $both[1];
    foreach (array('own', 'generated', 'with') as $key) {
        $totals[$key][0] += $both[0];
    }
}
$side = function ($pair, $measured) {
    return $measured ? array('lines' => $pair[0], 'covered' => $pair[1]) : null;
};
echo json_encode(array(
    'own' => $side($totals['own'], $own !== null),
    'generated' => $side($totals['generated'], $generated !== null),
    'with' => $side($totals['with'], $own !== null || $generated !== null),
    'areas' => array_values($areas),
));
