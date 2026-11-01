//! Where a generated test goes. "mirror" (the default) puts it next to the app's own tests, following app/:
//! app/Http/Controllers/Admin/UnitController.php -> tests/Feature/Controllers/Admin/UnitControllerParityTest.php
//! (or tests/Feature/Http/Controllers/... when the app's tests use that), app/Models/Unit.php ->
//! tests/Unit/Models/UnitParityTest.php. "folder" puts every feature test in [tests] dir, as before.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::Config;
use crate::testgen::MARKER;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Feature,
    Unit,
}

#[derive(Debug, PartialEq)]
pub struct Place {
    /// absolute path of the test file
    pub file: PathBuf,
    pub namespace: String,
    /// the test class, without namespace
    pub class: String,
}

impl Place {
    pub fn fqcn(&self) -> String {
        format!("{}\\{}", self.namespace, self.class)
    }
}

/// PSR-4 prefixes of the repo's composer.json: (namespace prefix without trailing \, folder without trailing /).
fn psr4(repo: &Path, section: &str) -> Vec<(String, String)> {
    let composer: Value = std::fs::read_to_string(repo.join("composer.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut out = vec![];
    if let Some(map) = composer[section]["psr-4"].as_object() {
        for (prefix, dirs) in map {
            let dir = match dirs {
                Value::String(d) => d.clone(),
                Value::Array(list) => list
                    .first()
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                _ => continue,
            };
            out.push((
                prefix.trim_end_matches('\\').to_string(),
                dir.trim_end_matches('/').to_string(),
            ));
        }
    }
    out
}

/// The namespace of the tests/ folder (Tests by default).
fn tests_namespace(repo: &Path) -> String {
    psr4(repo, "autoload-dev")
        .into_iter()
        .find(|(_, dir)| dir == "tests")
        .map(|(prefix, _)| prefix)
        .unwrap_or_else(|| "Tests".into())
}

/// The class's folders under app/ (App\Http\Controllers\Admin\X -> [Http, Controllers, Admin]); None for a class
/// outside the app's own namespace.
pub fn app_segments(repo: &Path, fqcn: &str) -> Option<Vec<String>> {
    let fqcn = fqcn.trim_start_matches('\\');
    let mut prefixes = psr4(repo, "autoload");
    if prefixes.is_empty() {
        prefixes.push(("App".into(), "app".into()));
    }
    let (prefix, _) = prefixes
        .iter()
        .filter(|(p, d)| d == "app" && fqcn.starts_with(&format!("{p}\\")))
        .max_by_key(|(p, _)| p.len())?;
    let rest = &fqcn[prefix.len() + 1..];
    let mut parts: Vec<String> = rest.split('\\').map(String::from).collect();
    parts.pop();
    Some(parts)
}

fn clean(name: &str) -> String {
    let short = name.rsplit('\\').next().unwrap_or(name);
    let clean: String = short
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if clean.is_empty() {
        "Closure".into()
    } else {
        clean
    }
}

/// Where the generated test for `fqcn` (a controller, a model, a service ...) goes.
pub fn place(config: &Config, kind: Kind, fqcn: &str) -> Place {
    let repo = &config.new.repo;
    let class = format!("{}ParityTest", clean(fqcn));
    if kind == Kind::Feature && config.tests.layout() == "folder" {
        return Place {
            file: repo.join(config.tests.dir()).join(format!("{class}.php")),
            namespace: config
                .tests
                .namespace
                .clone()
                .unwrap_or_else(|| "Tests\\Feature\\Parity".into()),
            class,
        };
    }
    let root = match kind {
        Kind::Feature => "Feature",
        Kind::Unit => "Unit",
    };
    let mut segments = app_segments(repo, fqcn).unwrap_or_else(|| vec!["Routes".into()]);
    // controllers follow the app's own habit: tests/Feature/Controllers when it has that and no Http folder
    if kind == Kind::Feature
        && segments.first().is_some_and(|s| s == "Http")
        && !repo.join("tests/Feature/Http").is_dir()
        && repo.join("tests/Feature/Controllers").is_dir()
    {
        segments.remove(0);
    }
    let mut file = repo.join("tests").join(root);
    for s in &segments {
        file = file.join(s);
    }
    let mut namespace = format!("{}\\{root}", tests_namespace(repo));
    for s in &segments {
        namespace.push('\\');
        namespace.push_str(s);
    }
    Place {
        file: file.join(format!("{class}.php")),
        namespace,
        class,
    }
}

/// Remove the tool's own test files (with the marker) of this kind that this run did not write again: a route that
/// got a test of its own, a method that is gone, or the old single-folder layout.
pub fn remove_stale(config: &Config, kind: Kind, written: &[PathBuf]) -> usize {
    let tests = config.new.repo.join("tests");
    let mut files = vec![];
    collect(&tests, &mut files);
    let unit = tests.join("Unit");
    let mut removed = 0;
    for file in files {
        let in_unit = file.starts_with(&unit);
        if (kind == Kind::Unit) != in_unit || written.contains(&file) {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        if text.contains(MARKER) && std::fs::remove_file(&file).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|e| e == "php") {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(repo: &Path, tests: &str) -> Config {
        let pairs = repo.join("pairs");
        std::fs::create_dir_all(&pairs).unwrap();
        let file = pairs.join("shop.toml");
        std::fs::write(
            &file,
            format!(
                "[old]\nrepo = '{0}'\nphp = 'php'\n[new]\nrepo = '{0}'\nphp = 'php'\n[database]\nsource = 'shop'\n{tests}",
                repo.display()
            ),
        )
        .unwrap();
        crate::config::load(&file).unwrap()
    }

    #[test]
    fn mirror_and_folder() {
        let dir = tempfile::tempdir().unwrap();
        let repo = &dir.path().canonicalize().unwrap();
        std::fs::write(
            repo.join("composer.json"),
            r#"{"autoload": {"psr-4": {"App\\": "app/", "Database\\Factories\\": "database/factories/"}},
                "autoload-dev": {"psr-4": {"Shop\\Tests\\": "tests/"}}}"#,
        )
        .unwrap();
        let c = config(repo, "");
        let p = place(
            &c,
            Kind::Feature,
            "App\\Http\\Controllers\\Admin\\UnitController",
        );
        assert_eq!(
            p.file,
            repo.join("tests/Feature/Http/Controllers/Admin/UnitControllerParityTest.php")
        );
        assert_eq!(
            p.namespace,
            "Shop\\Tests\\Feature\\Http\\Controllers\\Admin"
        );
        assert_eq!(
            p.fqcn(),
            "Shop\\Tests\\Feature\\Http\\Controllers\\Admin\\UnitControllerParityTest"
        );
        // the app keeps controller tests in tests/Feature/Controllers
        std::fs::create_dir_all(repo.join("tests/Feature/Controllers")).unwrap();
        let p = place(
            &c,
            Kind::Feature,
            "\\App\\Http\\Controllers\\Admin\\UnitController",
        );
        assert_eq!(
            p.file,
            repo.join("tests/Feature/Controllers/Admin/UnitControllerParityTest.php")
        );
        assert_eq!(p.namespace, "Shop\\Tests\\Feature\\Controllers\\Admin");
        let p = place(&c, Kind::Unit, "App\\Models\\Unit");
        assert_eq!(p.file, repo.join("tests/Unit/Models/UnitParityTest.php"));
        assert_eq!(p.namespace, "Shop\\Tests\\Unit\\Models");
        assert_eq!(
            place(&c, Kind::Unit, "App\\Helper").file,
            repo.join("tests/Unit/HelperParityTest.php")
        );
        // closures and package controllers
        let p = place(&c, Kind::Feature, "Closure");
        assert_eq!(
            p.file,
            repo.join("tests/Feature/Routes/ClosureParityTest.php")
        );
        assert_eq!(
            place(
                &c,
                Kind::Feature,
                "Laravel\\Passport\\Http\\Controllers\\AccessTokenController"
            )
            .class,
            "AccessTokenControllerParityTest"
        );

        let c = config(
            repo,
            "[tests]\nlayout = 'folder'\ndir = 'tests/Parity'\nnamespace = 'Tests\\Parity'\n",
        );
        let p = place(
            &c,
            Kind::Feature,
            "App\\Http\\Controllers\\Admin\\UnitController",
        );
        assert_eq!(
            p.file,
            repo.join("tests/Parity/UnitControllerParityTest.php")
        );
        assert_eq!(p.namespace, "Tests\\Parity");
        assert_eq!(
            place(&c, Kind::Unit, "App\\Models\\Unit").file,
            repo.join("tests/Unit/Models/UnitParityTest.php")
        );
    }

    #[test]
    fn stale_files_go() {
        let dir = tempfile::tempdir().unwrap();
        let repo = &dir.path().canonicalize().unwrap();
        let c = config(repo, "");
        let write = |rel: &str, text: &str| {
            let path = repo.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        let old = write("tests/Feature/Parity/UnitControllerParityTest.php", MARKER);
        let kept = write(
            "tests/Feature/Controllers/UnitControllerParityTest.php",
            MARKER,
        );
        let mine = write(
            "tests/Feature/Parity/Extra/MineTest.php",
            "<?php // by hand",
        );
        let unit = write("tests/Unit/Models/UnitParityTest.php", MARKER);
        assert_eq!(
            remove_stale(&c, Kind::Feature, std::slice::from_ref(&kept)),
            1
        );
        assert!(!old.exists() && kept.exists() && mine.exists() && unit.exists());
        assert_eq!(remove_stale(&c, Kind::Unit, &[]), 1);
        assert!(!unit.exists());
    }
}
