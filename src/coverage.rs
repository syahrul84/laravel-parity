//! Code coverage of the new app: the lines of app/ its own tests cover, and its own tests plus the generated ones.
//! Measured with the app's own pcov or Xdebug and php-code-coverage (`php/coverage.php`); the tool never installs
//! either. The app's own tests are measured once and reused until its code or tests change.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{Config, Side};
use crate::runner;
use crate::say;
use crate::testgen;

const COVERAGE_PHP: &str = include_str!("../php/coverage.php");

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Driver {
    Pcov,
    Xdebug,
}

impl Driver {
    pub fn name(self) -> &'static str {
        match self {
            Driver::Pcov => "pcov",
            Driver::Xdebug => "Xdebug",
        }
    }
}

/// The coverage driver the new app's PHP has loaded, pcov first (much faster).
pub fn driver(side: &Side) -> Option<Driver> {
    let output = runner::run_in_app(side, &["php", "-m"]).ok()?;
    driver_from(&String::from_utf8_lossy(&output.stdout))
}

fn driver_from(modules: &str) -> Option<Driver> {
    let has = |name: &str| modules.lines().any(|l| l.trim().eq_ignore_ascii_case(name));
    if has("pcov") {
        Some(Driver::Pcov)
    } else if has("xdebug") {
        Some(Driver::Xdebug)
    } else {
        None
    }
}

/// What to say when the new app's PHP has no driver.
pub fn missing_note() -> String {
    "not measured: the new app's PHP has neither pcov nor Xdebug (install pcov, e.g. `apt-get install php8.x-pcov` \
     or `pecl install pcov`; with pcov.enabled=0 in its ini the running app is not slowed down)"
        .into()
}

/// The PHPUnit command from the pair file, with the flags that write coverage to `out` when `cover` is given.
/// Coverage needs the command to start with php (or phpunit, which then runs through php).
pub fn command(config: &Config, cover: Option<(Driver, &str)>, newer: bool) -> Vec<String> {
    let base = config
        .tests
        .phpunit
        .clone()
        .unwrap_or_else(|| "php vendor/bin/phpunit".into());
    let mut args: Vec<String> = base.split_whitespace().map(String::from).collect();
    let Some((driver, out)) = cover else {
        return args;
    };
    let app_dir = format!("{}/app", config.new.app_path.trim_end_matches('/'));
    let flags: Vec<String> = match driver {
        Driver::Pcov => vec![
            "-d".into(),
            "pcov.enabled=1".into(),
            "-d".into(),
            format!("pcov.directory={app_dir}"),
        ],
        Driver::Xdebug => vec!["-d".into(), "xdebug.mode=coverage".into()],
    };
    match args.first().map(String::as_str) {
        Some("php") => {
            args.splice(1..1, flags);
        }
        Some(first) if first.ends_with("phpunit") => {
            let mut php = vec!["php".to_string()];
            php.extend(flags);
            args.splice(0..0, php);
        }
        _ => return args,
    }
    args.extend(["--coverage-php".into(), out.to_string()]);
    // PHPUnit 10+ measures nothing without a <source> filter; this adds app/ when phpunit.xml has none
    if newer {
        args.extend(["--coverage-filter".into(), app_dir]);
    }
    args
}

/// Copy coverage.php into the app's scratch folder.
pub fn script(side: &Side) -> Result<String, String> {
    let local = tempfile::tempdir().map_err(|e| e.to_string())?;
    let file = local.path().join("coverage.php");
    std::fs::write(&file, COVERAGE_PHP).map_err(|e| e.to_string())?;
    runner::put(side, &file, "coverage.php")
}

/// Add the coverage files `parts` to `total` (all scratch paths) in one PHP run, then remove the parts.
pub fn merge(side: &Side, script: &str, total: &str, parts: &[String]) {
    if parts.is_empty() {
        return;
    }
    let app = format!("--app={}", side.app_path);
    let mut args = vec!["php", script, &app, "merge", total];
    args.extend(parts.iter().map(String::as_str));
    let _ = runner::run_in_app(side, &args);
    for part in parts {
        runner::remove(side, part);
    }
}

/// Copy a coverage file the app wrote to runs/<name>/coverage/<name>.
pub fn fetch(config: &Config, remote: &str, name: &str) -> Option<PathBuf> {
    let dir = config.work.join("coverage");
    let _ = std::fs::create_dir_all(&dir);
    let local = dir.join(name);
    let _ = std::fs::remove_file(&local);
    runner::fetch(&config.new, remote, &local).then_some(local)
}

/// A fingerprint of what the app's own tests depend on: app/, tests/ (without generated files), database/, routes/,
/// config/, composer.lock and phpunit.xml. The same fingerprint reuses the last measurement.
pub fn fingerprint(repo: &Path, driver: Driver) -> String {
    let mut files = vec![];
    for dir in ["app", "tests", "database", "routes", "config"] {
        collect(&repo.join(dir), &mut files);
    }
    for file in ["composer.lock", "phpunit.xml", "phpunit.xml.dist"] {
        let path = repo.join(file);
        if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    let mut hash = Sha256::new();
    hash.update(driver.name());
    for file in files {
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        if file.extension().is_some_and(|e| e == "php") {
            let text = String::from_utf8_lossy(&bytes);
            if text.contains(testgen::MARKER) || text.contains(testgen::AI_GROUP) {
                continue;
            }
        }
        let rel = file.strip_prefix(repo).unwrap_or(&file);
        hash.update(rel.to_string_lossy().replace('\\', "/"));
        hash.update([0]);
        hash.update(&bytes);
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == "vendor" || name == "node_modules" || name == ".git" {
            continue;
        }
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}

/// The app's own tests, measured: the coverage file and whether it was reused, or why there is none.
pub struct Own {
    pub file: Option<PathBuf>,
    pub reused: bool,
    pub note: Option<String>,
}

/// Measure the app's own tests (every test except the generated group), or reuse the last measurement when nothing
/// they depend on changed.
pub fn own(config: &Config, driver: Driver, newer: bool) -> Own {
    let key = fingerprint(&config.new.repo, driver);
    let saved = config.work.join("coverage").join("own.json");
    let file = config.work.join("coverage").join("own.cov");
    let last: Value = std::fs::read_to_string(&saved)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if last["key"] == key.as_str() && file.is_file() {
        say(
            "Coverage of the app's own tests: nothing they depend on changed, reusing the last measurement.",
        );
        return Own {
            file: Some(file),
            reused: true,
            note: None,
        };
    }
    say(&format!(
        "Measuring the coverage of the app's own tests with {} (this runs the whole suite once; it is reused until \
         app/, tests/, database/, routes/ or config/ change) ...",
        driver.name()
    ));
    let side = &config.new;
    runner::prepare_scratch(side);
    let remote = runner::scratch(side, "own.cov");
    runner::remove(side, &remote);
    let mut args = command(config, Some((driver, &remote)), newer);
    args.extend([
        "--exclude-group".into(),
        format!("{},{}", testgen::GROUP, testgen::AI_GROUP),
    ]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    let output = runner::run_in_app(side, &refs);
    let fetched = fetch(config, &remote, "own.cov");
    runner::remove(side, &remote);
    match fetched {
        Some(file) => {
            let _ = std::fs::write(
                &saved,
                json!({"key": key, "seconds": started.elapsed().as_secs()}).to_string(),
            );
            Own {
                file: Some(file),
                reused: false,
                note: None,
            }
        }
        None => {
            let tail = output
                .map(|o| {
                    let text = String::from_utf8_lossy(&o.stdout).into_owned()
                        + &String::from_utf8_lossy(&o.stderr);
                    let lines: Vec<&str> = text.trim().lines().collect();
                    lines[lines.len().saturating_sub(3)..].join(" ")
                })
                .unwrap_or_else(|e| e);
            Own {
                file: None,
                reused: false,
                note: Some(format!(
                    "the app's own tests stopped before PHPUnit wrote coverage: {}",
                    tail.chars().take(300).collect::<String>()
                )),
            }
        }
    }
}

/// Lines of app/ covered by the app's own tests, the generated ones (and the AI-written ones), and all of them,
/// overall and per area.
pub fn summary(
    config: &Config,
    script: &str,
    own: Option<&Path>,
    generated: &[&Path],
) -> Result<Value, String> {
    let side = &config.new;
    let own_remote = own
        .map(|f| runner::put(side, f, "own-in.cov"))
        .transpose()?;
    let mut generated_remote = vec![];
    for (i, file) in generated.iter().enumerate() {
        generated_remote.push(runner::put(side, file, &format!("generated-in-{i}.cov"))?);
    }
    let app = format!("--app={}", side.app_path);
    let mut args = vec![
        "php",
        script,
        &app,
        "summary",
        own_remote.as_deref().unwrap_or("-"),
    ];
    if generated_remote.is_empty() {
        args.push("-");
    }
    args.extend(generated_remote.iter().map(String::as_str));
    let output = runner::run_in_app(side, &args);
    for remote in own_remote.iter().chain(generated_remote.iter()) {
        runner::remove(side, remote);
    }
    let output = output?;
    json_out(&output, "{\"own\"", "coverage summary")
}

/// The JSON a PHP script printed (after anything the app's bootstrap printed), or its error.
fn json_out(output: &std::process::Output, start: &str, what: &str) -> Result<Value, String> {
    let text = String::from_utf8_lossy(&output.stdout);
    let at = text.find(start).unwrap_or(0);
    serde_json::from_str(&text[at..]).map_err(|_| {
        format!(
            "could not read the {what}: {}",
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(300)
                .collect::<String>()
        )
    })
}

/// The methods with the most lines no test runs (own, generated and AI-written tests together), thinnest first;
/// `filter` keeps files whose path contains it (Models, Services/Billing ...).
pub fn gaps(config: &Config, filter: Option<&str>, limit: usize) -> Result<Value, String> {
    let dir = config.work.join("coverage");
    let files: Vec<PathBuf> = ["own.cov", "generated.cov", "ai.cov"]
        .iter()
        .map(|f| dir.join(f))
        .filter(|f| f.is_file())
        .collect();
    if files.is_empty() {
        return Err("no coverage measured yet: run `tests` (generate_tests) first, with pcov or Xdebug in the new app's PHP".into());
    }
    let side = &config.new;
    runner::prepare_scratch(side);
    let script = script(side)?;
    let mut remote = vec![];
    for (i, file) in files.iter().enumerate() {
        remote.push(runner::put(side, file, &format!("gaps-{i}.cov"))?);
    }
    let app = format!("--app={}", side.app_path);
    let limit = limit.to_string();
    let mut args = vec![
        "php",
        script.as_str(),
        &app,
        "gaps",
        &limit,
        filter.unwrap_or("-"),
    ];
    args.extend(remote.iter().map(String::as_str));
    let output = runner::run_in_app(side, &args);
    for r in remote.iter().chain(std::iter::once(&script)) {
        runner::remove(side, r);
    }
    let mut found = json_out(&output?, "{\"total\"", "coverage gaps")?;
    // where a test for each belongs, next to the app's own tests
    for gap in found["gaps"].as_array_mut().into_iter().flatten() {
        let class = gap["class"].as_str().unwrap_or_default().to_string();
        let kind = if class.contains("\\Http\\Controllers\\") {
            crate::layout::Kind::Feature
        } else {
            crate::layout::Kind::Unit
        };
        let mut place = crate::layout::place(config, kind, &class);
        place.class = place.class.replace("ParityTest", "CoverageTest");
        place.file = place.file.with_file_name(format!("{}.php", place.class));
        gap["test_file"] = json!(
            place
                .file
                .strip_prefix(&config.new.repo)
                .unwrap_or(&place.file)
                .to_string_lossy()
                .replace('\\', "/")
        );
        gap["test_class"] = json!(place.fqcn());
    }
    Ok(found)
}

/// The AI-written tests (group laravel-parity-ai): how many pass, and their coverage.
pub struct Ai {
    pub file: Option<PathBuf>,
    pub passed: usize,
    pub failed: usize,
    pub failures: Vec<(String, String)>,
}

/// Run tests with coverage: the AI group when `files` is empty, else those files (paths in the new repo). The
/// coverage of the AI group is kept in runs/<name>/coverage/ai.cov: replaced by a group run, added to by a run of
/// some files.
pub fn ai_tests(config: &Config, driver: Driver, script: &str) -> Ai {
    run_ai(config, driver, script, &[])
}

fn run_ai(config: &Config, driver: Driver, script: &str, files: &[String]) -> Ai {
    let side = &config.new;
    runner::prepare_scratch(side);
    let total = runner::scratch(side, "ai.cov");
    let part = runner::scratch(side, "ai-part.cov");
    runner::remove(side, &total);
    let local = config.work.join("coverage").join("ai.cov");
    if !files.is_empty() && local.is_file() {
        let _ = runner::put(side, &local, "ai.cov");
    }
    let mut results = testgen::Outcomes::new();
    if files.is_empty() {
        testgen::phpunit(
            config,
            &["--group", testgen::AI_GROUP],
            &mut results,
            Some((driver, &part)),
        );
        merge(side, script, &total, std::slice::from_ref(&part));
    } else {
        for file in files {
            testgen::phpunit(config, &[file], &mut results, Some((driver, &part)));
            merge(side, script, &total, std::slice::from_ref(&part));
        }
    }
    let file = fetch(config, &total, "ai.cov");
    runner::remove(side, &total);
    if file.is_none() && files.is_empty() {
        let _ = std::fs::remove_file(&local);
    }
    let mut failures: Vec<(String, String)> = results
        .iter()
        .filter_map(|((c, m), v)| v.as_ref().map(|msg| (format!("{c}::{m}"), msg.clone())))
        .collect();
    failures.sort();
    Ai {
        file: file.or_else(|| local.is_file().then_some(local.clone())),
        passed: results.values().filter(|v| v.is_none()).count(),
        failed: failures.len(),
        failures,
    }
}

/// Run the given test files (written by an AI assistant from the gaps) with coverage, add what they cover to the
/// totals, and say how far the app is from the target.
pub fn check_tests(config: &Config, files: &[String]) -> Result<Value, String> {
    if files.is_empty() {
        return Err("name the test files to run (paths in the new repo, e.g. tests/Unit/Models/UnitCoverageTest.php)".into());
    }
    runner::ensure_ready(&config.new)?;
    let driver = driver(&config.new).ok_or_else(missing_note)?;
    let script = script(&config.new)?;
    let ai = run_ai(config, driver, &script, files);
    let dir = config.work.join("coverage");
    let own = dir.join("own.cov");
    let generated = dir.join("generated.cov");
    let mut extra: Vec<&Path> = vec![];
    if generated.is_file() {
        extra.push(&generated);
    }
    if let Some(f) = &ai.file {
        extra.push(f);
    }
    let summary = summary(
        config,
        &script,
        own.is_file().then_some(own.as_path()),
        &extra,
    );
    runner::remove(&config.new, &script);
    let mut summary = summary?;
    summary["driver"] = json!(driver.name());
    summary["target"] = json!(config.tests.target());
    // keep tests.json's coverage current, so the report shows the new number
    let tests_file = config.work.join("tests.json");
    if let Ok(text) = std::fs::read_to_string(&tests_file)
        && let Ok(mut tests) = serde_json::from_str::<Value>(&text)
    {
        let mut kept = summary.clone();
        if let Some(note) = tests["coverage"]["own_note"].as_str() {
            kept["own_note"] = json!(note);
        }
        tests["coverage"] = kept;
        let _ = std::fs::write(
            &tests_file,
            serde_json::to_string_pretty(&tests).unwrap_or_default(),
        );
    }
    Ok(json!({
        "passed": ai.passed,
        "failed": ai.failures.iter().map(|(t, m)| json!({"test": t, "message": m})).collect::<Vec<_>>(),
        "coverage": line(&summary),
        "percent": percent(&summary["with"]),
        "target": config.tests.target(),
    }))
}

/// Percent with one decimal, from a {lines, covered} object.
pub fn percent(part: &Value) -> Option<f64> {
    let lines = part["lines"].as_f64()?;
    let covered = part["covered"].as_f64()?;
    (lines > 0.0).then(|| (covered * 1000.0 / lines).round() / 10.0)
}

/// The areas whose coverage the generated tests raised most, largest gain first.
pub fn gains(summary: &Value, limit: usize) -> Vec<Value> {
    let mut areas: Vec<Value> = summary["areas"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a["with"].as_i64().unwrap_or(0) > a["own"].as_i64().unwrap_or(0))
        .collect();
    areas.sort_by_key(|a| {
        std::cmp::Reverse(a["with"].as_i64().unwrap_or(0) - a["own"].as_i64().unwrap_or(0))
    });
    areas.truncate(limit);
    areas
}

/// One line for the report: "Lines of app/ covered: 12.1% by the app's own tests, 25.7% with the generated ones".
pub fn line(coverage: &Value) -> Option<String> {
    if let Some(note) = coverage["note"].as_str() {
        return Some(format!("Code coverage: {note}."));
    }
    let with = percent(&coverage["with"])?;
    let driver = coverage["driver"].as_str().unwrap_or("?");
    let target = coverage["target"]
        .as_f64()
        .filter(|t| *t > with)
        .map(|t| format!(" Target {t}%: {:.1} points to go.", t - with))
        .unwrap_or_default();
    Some(match percent(&coverage["own"]) {
        Some(own) => {
            format!(
                "Lines of app/ covered: {own}% by the app's own tests, {with}% with the generated ones (+{:.1} points; \
             {driver}).",
                ((with - own) * 10.0).round() / 10.0
            ) + &target
        }
        None => {
            format!(
                "Lines of app/ covered by the generated tests: {with}% ({driver}; the app's own tests were not measured)."
            ) + &target
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(phpunit: Option<&str>) -> Config {
        let dir = std::env::temp_dir().join(format!("lp-coverage-{}", std::process::id()));
        let pairs = dir.join("pairs");
        std::fs::create_dir_all(&pairs).unwrap();
        let file = pairs.join("shop.toml");
        let tests = phpunit
            .map(|p| format!("[tests]\nphpunit = '{p}'\n"))
            .unwrap_or_default();
        std::fs::write(
            &file,
            format!(
                "[old]\nrepo = '/nonexistent/old'\ncontainer = 'shop-old'\n[new]\nrepo = '/nonexistent/new'\n\
                 container = 'shop'\napp_path = '/var/www/html'\n[database]\nsource = 'shop'\n{tests}"
            ),
        )
        .unwrap();
        crate::config::load(&file).unwrap()
    }

    #[test]
    fn phpunit_with_coverage() {
        assert_eq!(
            driver_from("[PHP Modules]\nCore\npcov\nxdebug\n"),
            Some(Driver::Pcov)
        );
        assert_eq!(driver_from("Core\nXdebug\n"), Some(Driver::Xdebug));
        assert_eq!(driver_from("Core\npcovx\n"), None);

        let c = config(None);
        assert_eq!(command(&c, None, true), ["php", "vendor/bin/phpunit"]);
        assert_eq!(
            command(&c, Some((Driver::Pcov, "/tmp/x.cov")), true),
            [
                "php",
                "-d",
                "pcov.enabled=1",
                "-d",
                "pcov.directory=/var/www/html/app",
                "vendor/bin/phpunit",
                "--coverage-php",
                "/tmp/x.cov",
                "--coverage-filter",
                "/var/www/html/app"
            ]
        );
        assert_eq!(
            command(&c, Some((Driver::Xdebug, "x.cov")), false),
            [
                "php",
                "-d",
                "xdebug.mode=coverage",
                "vendor/bin/phpunit",
                "--coverage-php",
                "x.cov"
            ]
        );
        let c = config(Some("vendor/bin/phpunit --stop-on-defect"));
        assert_eq!(
            command(&c, Some((Driver::Xdebug, "x.cov")), false)[..4],
            ["php", "-d", "xdebug.mode=coverage", "vendor/bin/phpunit"]
        );
        // a command the tool cannot add a PHP flag to runs without coverage
        let c = config(Some("composer test"));
        assert_eq!(
            command(&c, Some((Driver::Pcov, "x.cov")), true),
            ["composer", "test"]
        );
    }

    #[test]
    fn numbers_and_lines() {
        let summary = json!({
            "own": {"lines": 1000, "covered": 121},
            "generated": {"lines": 1000, "covered": 200},
            "with": {"lines": 1000, "covered": 257},
            "driver": "pcov",
            "areas": [
                {"area": "Models", "lines": 100, "own": 50, "generated": 10, "with": 60},
                {"area": "Http/Controllers", "lines": 800, "own": 60, "generated": 180, "with": 190},
                {"area": "Mail", "lines": 100, "own": 11, "generated": 0, "with": 11}
            ]
        });
        assert_eq!(percent(&summary["own"]), Some(12.1));
        assert_eq!(percent(&summary["with"]), Some(25.7));
        assert_eq!(percent(&json!({"lines": 0, "covered": 0})), None);
        assert_eq!(percent(&Value::Null), None);
        let top = gains(&summary, 5);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0]["area"], "Http/Controllers");
        assert_eq!(
            line(&summary).unwrap(),
            "Lines of app/ covered: 12.1% by the app's own tests, 25.7% with the generated ones (+13.6 points; pcov)."
        );
        let only = json!({"own": null, "with": {"lines": 10, "covered": 5}, "driver": "Xdebug"});
        assert!(
            line(&only)
                .unwrap()
                .contains("50% (Xdebug; the app's own tests were not measured)")
        );
        assert_eq!(
            line(&json!({"note": "not measured: no driver"})).unwrap(),
            "Code coverage: not measured: no driver."
        );
    }

    #[test]
    fn fingerprint_ignores_generated_files() {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        std::fs::create_dir_all(root.join("app/Models")).unwrap();
        std::fs::create_dir_all(root.join("tests/Feature/Parity")).unwrap();
        std::fs::write(root.join("app/Models/Shop.php"), "<?php class Shop {}").unwrap();
        std::fs::write(root.join("phpunit.xml"), "<phpunit/>").unwrap();
        let first = fingerprint(root, Driver::Pcov);
        assert_ne!(first, fingerprint(root, Driver::Xdebug));
        std::fs::write(
            root.join("tests/Feature/Parity/ShopParityTest.php"),
            format!("<?php /** {} */", testgen::MARKER),
        )
        .unwrap();
        assert_eq!(first, fingerprint(root, Driver::Pcov));
        std::fs::write(root.join("tests/Feature/ShopTest.php"), "<?php // mine").unwrap();
        let second = fingerprint(root, Driver::Pcov);
        assert_ne!(first, second);
        std::fs::write(root.join("app/Models/Shop.php"), "<?php class Shop { }").unwrap();
        assert_ne!(second, fingerprint(root, Driver::Pcov));
    }
}
