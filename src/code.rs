//! `code`: problems in the upgraded code itself, found without running it. Behaviour checks only catch what the
//! test data reaches; these catch the rest:
//! - codemap `doctor` on both repos: routes to controller methods that do not exist, code that does not parse,
//!   migrations that create a table twice; only what the old repo did not have counts as caused by the upgrade
//! - upgrade traps, picked by the version jump (old -> new Laravel, Carbon, PHP from composer.lock / composer.json):
//!   `$dates` (Laravel 10), signed `diffIn*` (Carbon 3), removed helpers and facades, PHP 8.x removals and
//!   deprecations; each with file:line and the usual fix
//! - every PHP file parsed by the new app's own PHP
//! - PHPStan / Larastan, when the new repo has it

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, Side};
use crate::ops::{Res, read, write};
use crate::say;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Versions {
    pub laravel: Option<String>,
    pub carbon: Option<String>,
    pub php: Option<String>,
    pub helpers_package: bool,
}

/// (major, minor) of a version or constraint: "v13.38.0" -> (13, 38), "^8.1" -> (8, 1).
fn major_minor(v: &str) -> Option<(u32, u32)> {
    static NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)(?:\.(\d+))?").unwrap());
    let c = NUM.captures(v)?;
    Some((
        c[1].parse().ok()?,
        c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0),
    ))
}

pub fn versions(repo: &Path, ran_php: Option<&str>) -> Versions {
    let lock: Value = std::fs::read_to_string(repo.join("composer.lock"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let composer: Value = std::fs::read_to_string(repo.join("composer.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let package = |name: &str| {
        ["packages", "packages-dev"]
            .iter()
            .flat_map(|k| lock[*k].as_array().cloned().unwrap_or_default())
            .find(|p| p["name"] == name)
            .and_then(|p| p["version"].as_str().map(String::from))
    };
    Versions {
        laravel: package("laravel/framework"),
        carbon: package("nesbot/carbon"),
        // the PHP the app really ran on, else what composer.json asks for
        php: ran_php
            .map(String::from)
            .or_else(|| composer["require"]["php"].as_str().map(String::from)),
        helpers_package: package("laravel/helpers").is_some(),
    }
}

/// Whether the upgrade crossed `at` (old below it, new at or above); without an old version, whether new is there.
fn crossed(old: &Option<String>, new: &Option<String>, at: (u32, u32)) -> bool {
    let Some(n) = new.as_deref().and_then(major_minor) else {
        return false;
    };
    if n < at {
        return false;
    }
    old.as_deref().and_then(major_minor).is_none_or(|o| o < at)
}

pub struct Rule {
    pub id: &'static str,
    pub title: &'static str,
    pub priority: u8,
    pub fix: &'static str,
    /// Which version jump makes it a problem: (package, version) with package laravel, carbon or php.
    pub when: (&'static str, (u32, u32)),
    pub pattern: &'static str,
    /// Only in these folders (default: app, routes, database, config).
    pub dirs: &'static [&'static str],
}

pub const RULES: &[Rule] = &[
    Rule {
        id: "dates-property",
        title: "Models still use `protected $dates`, which Laravel 10 removed: those attributes are no longer dates",
        priority: 2,
        fix: "Move each field to `$casts` as 'datetime' (or 'date'), e.g. `protected $casts = ['paid_at' => 'datetime'];`, then remove `$dates`. Code calling ->format() on them throws until then.",
        when: ("laravel", (10, 0)),
        pattern: r"protected\s+\$dates\s*=",
        dirs: &["app"],
    },
    Rule {
        id: "carbon3-diff",
        title: "`diffIn*()` calls: Carbon 3 returns a signed float where Carbon 2 returned a positive integer",
        priority: 2,
        fix: "Where the old code relied on a positive whole number (`$end->diffInDays($start) + 1`), write `(int) abs($a->diffInDays($b))`. With `false` (signed on purpose) the result is now fractional too (2.5 days): compare with `<`/`>` or wrap it in (int) where the code tests `== n`. Calls already wrapped in intval/(int) with `true` are left out.",
        when: ("carbon", (3, 0)),
        pattern: r"->diffIn(Days|Weekdays|Weekends|Weeks|Months|Quarters|Years|Hours|Minutes|Seconds|RealDays|RealHours|RealMinutes|RealSeconds)\(",
        dirs: &["app", "routes", "database", "config"],
    },
    Rule {
        id: "removed-helpers",
        title: "String / array helpers removed in Laravel 6 are still called (fatal: undefined function)",
        priority: 1,
        fix: "Use the Str:: / Arr:: methods (str_slug -> Str::slug, array_get -> Arr::get, ...), or require laravel/helpers.",
        when: ("laravel", (6, 0)),
        pattern: r"\b(array_get|array_set|array_has|array_first|array_last|array_pluck|array_only|array_except|array_wrap|array_where|array_sort|array_forget|array_prepend|array_pull|array_random|str_slug|str_random|str_limit|str_finish|str_start|str_is|str_plural|str_singular|str_before|str_after|starts_with|ends_with|camel_case|snake_case|studly_case|title_case|kebab_case)\(",
        dirs: &["app", "routes", "database", "config"],
    },
    Rule {
        id: "input-facade",
        title: "The Input facade (removed in Laravel 6) is still used",
        priority: 1,
        fix: "Use `request()->input()` or the Request object.",
        when: ("laravel", (6, 0)),
        pattern: r"\bInput::(get|all|only|except|has|file)\(",
        dirs: &["app", "routes"],
    },
    Rule {
        id: "dispatch-now",
        title: "`dispatchNow()` (removed in Laravel 10) is still called",
        priority: 1,
        fix: "Use `dispatch_sync()` / `Bus::dispatchSync()` / `Job::dispatchSync()`.",
        when: ("laravel", (10, 0)),
        pattern: r"\bdispatch_now\(|->dispatchNow\(|::dispatchNow\(",
        dirs: &["app", "routes"],
    },
    Rule {
        id: "doctrine-dbal",
        title: "Doctrine DBAL schema calls, which Laravel 11 removed",
        priority: 1,
        fix: "Use Laravel's own schema methods (Schema::getColumns(), getIndexes(), getTables()); `->change()` works without DBAL in Laravel 11.",
        when: ("laravel", (11, 0)),
        pattern: r"getDoctrine(SchemaManager|Column|Connection|Driver)\(",
        dirs: &["app", "database"],
    },
    Rule {
        id: "float-precision",
        title: "Migrations give double/float a total and places, which Laravel 11 no longer takes",
        priority: 3,
        fix: "Laravel 11: `$table->double('x')` or `$table->float('x', precision: 53)`; use `decimal('x', 8, 2)` for fixed places.",
        when: ("laravel", (11, 0)),
        pattern: r"->(double|float)\(\s*['\x22][^'\x22]+['\x22]\s*,\s*\d+\s*,\s*\d+",
        dirs: &["database"],
    },
    Rule {
        id: "php8-removed",
        title: "Functions removed in PHP 8 are still called (fatal: undefined function)",
        priority: 1,
        fix: "each() -> foreach; create_function() -> a closure; money_format() -> NumberFormatter; image2wbmp() -> imagewbmp().",
        when: ("php", (8, 0)),
        pattern: r"(?:^|[^\w>:$])(each|create_function|money_format|image2wbmp|ezmlm_hash|restore_include_path)\(",
        dirs: &["app", "routes", "database", "config"],
    },
    Rule {
        id: "string-interpolation",
        title: "`${var}` inside strings, deprecated in PHP 8.2",
        priority: 3,
        fix: "Write `{$var}` instead of `${var}`.",
        when: ("php", (8, 2)),
        pattern: r#""[^"\n]*\$\{\w"#,
        dirs: &["app", "routes", "database", "config"],
    },
    Rule {
        id: "utf8-encode",
        title: "utf8_encode() / utf8_decode(), deprecated in PHP 8.2",
        priority: 3,
        fix: "Use mb_convert_encoding($s, 'UTF-8', 'ISO-8859-1') (and back).",
        when: ("php", (8, 2)),
        pattern: r"\butf8_(en|de)code\(",
        dirs: &["app", "routes", "database", "config"],
    },
    Rule {
        id: "implicit-nullable",
        title: "Parameters typed without ? but defaulting to null, deprecated in PHP 8.4",
        priority: 3,
        fix: "Write `?Type $x = null` (or `Type|null $x = null`).",
        when: ("php", (8, 4)),
        pattern: r"function\s+\w*\s*\([^)]*?[(,]\s*(?:[A-Za-z_\\][\w\\]*)\s+&?\$\w+\s*=\s*null\b",
        dirs: &["app", "routes", "database", "config"],
    },
];

fn php_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if !matches!(name, "vendor" | "node_modules" | "cache" | ".git") {
                php_files(&path, out);
            }
        } else if name.ends_with(".php") {
            out.push(path);
        }
    }
}

/// A line that is a comment (the hit is in commented-out code).
fn commented(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t.starts_with('*') || t.starts_with('#') || t.starts_with("/*")
}

fn applies(rule: &Rule, old: &Versions, new: &Versions) -> bool {
    let (package, at) = rule.when;
    if rule.id == "removed-helpers" && new.helpers_package {
        return false;
    }
    match package {
        "laravel" => crossed(&old.laravel, &new.laravel, at),
        "carbon" => {
            crossed(&old.carbon, &new.carbon, at)
                || (new.carbon.is_none() && crossed(&old.laravel, &new.laravel, (11, 0)))
        }
        _ => crossed(&old.php, &new.php, at),
    }
}

/// Hits of the rules that apply to this upgrade, in the new repo: rule id -> ["file:line  code"].
fn scan_rules(repo: &Path, old: &Versions, new: &Versions) -> Vec<Value> {
    // the implicit-nullable pattern must not count `?Type $x = null` or `Type|null`
    static NULLABLE_OK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[(,]\s*\?|\|\s*null\b|\bnull\s*\||\bmixed\s").unwrap());
    let nullable_ok = &*NULLABLE_OK;
    static CARBON_SAFE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(intval\(|\(int\)|abs\(|round\(|floor\(|ceil\().*diffIn\w+\([^()]*(\([^()]*\))?[^()]*,\s*true\s*\)").unwrap()
    });
    let carbon_safe = &*CARBON_SAFE;
    let rules: Vec<(&Rule, Regex)> = RULES
        .iter()
        .filter(|r| applies(r, old, new))
        .map(|r| (r, Regex::new(r.pattern).unwrap()))
        .collect();
    let mut found = vec![];
    for (rule, pattern) in &rules {
        let mut hits = vec![];
        for dir in rule.dirs {
            let mut files = vec![];
            php_files(&repo.join(dir), &mut files);
            files.sort();
            for file in files {
                let Ok(text) = std::fs::read_to_string(&file) else {
                    continue;
                };
                if !pattern.is_match(&text) {
                    continue;
                }
                let lines: Vec<&str> = text.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    let line = *line;
                    if commented(line) || !pattern.is_match(line) {
                        continue;
                    }
                    if rule.id == "implicit-nullable" && nullable_ok.is_match(line) {
                        continue;
                    }
                    // already made absolute and whole (`intval($a->diffInDays($b, true))`): Carbon 3 safe; a call
                    // split over lines (`intval($start` / `->diffInDays($end, true))`) is read with the two before
                    if rule.id == "carbon3-diff" {
                        let joined = if line.trim_start().starts_with("->") {
                            let start = i.saturating_sub(2);
                            lines[start..=i]
                                .iter()
                                .map(|l| l.trim())
                                .collect::<Vec<_>>()
                                .join("")
                        } else {
                            line.to_string()
                        };
                        if carbon_safe.is_match(&joined) {
                            continue;
                        }
                    }
                    let rel = file
                        .strip_prefix(repo)
                        .unwrap_or(&file)
                        .to_string_lossy()
                        .replace('\\', "/");
                    hits.push(format!(
                        "{rel}:{}  {}",
                        i + 1,
                        line.trim().chars().take(140).collect::<String>()
                    ));
                }
            }
        }
        if !hits.is_empty() {
            found.push(json!({"id": rule.id, "title": rule.title, "priority": rule.priority, "fix": rule.fix, "hits": hits}));
        }
    }
    found
}

/// Models whose `$dates` list was commented out (often by the upgrade) while its fields never reached `$casts`:
/// those attributes silently stopped being dates. created_at / updated_at / deleted_at are cast anyway.
fn scan_lost_dates(repo: &Path) -> Option<Value> {
    static DATES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)//\s*protected\s+\$dates\s*=\s*\[(.*?)//\s*\];").unwrap()
    });
    static CASTS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)\$casts\s*=\s*\[(.*?)\];").unwrap());
    static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"['"](\w+)['"]"#).unwrap());
    static CAST_KEY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"['"](\w+)['"]\s*=>"#).unwrap());
    let mut files = vec![];
    php_files(&repo.join("app"), &mut files);
    files.sort();
    let mut hits = vec![];
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Some(dates) = DATES.captures(&text) else {
            continue;
        };
        let cast: BTreeSet<String> = CASTS
            .captures(&text)
            .map(|c| {
                CAST_KEY
                    .captures_iter(&c[1])
                    .map(|k| k[1].to_string())
                    .collect()
            })
            .unwrap_or_default();
        let lost: Vec<String> = QUOTED
            .captures_iter(&dates[1])
            .map(|c| c[1].to_string())
            .filter(|f| {
                !matches!(f.as_str(), "created_at" | "updated_at" | "deleted_at")
                    && !cast.contains(f)
            })
            .collect();
        if lost.is_empty() {
            continue;
        }
        let line = text[..dates.get(0).unwrap().start()].lines().count() + 1;
        let rel = file
            .strip_prefix(repo)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        hits.push(format!(
            "{rel}:{line}  no longer dates: {}",
            lost.join(", ")
        ));
    }
    (!hits.is_empty()).then(|| {
        json!({
            "id": "dates-lost",
            "title": "Models whose `$dates` list was commented out without moving the fields to `$casts`: these attributes are no longer dates",
            "priority": 2,
            "fix": "Add each field to `$casts` as 'datetime' (or 'date'). Until then they come back as strings: ->format(), ->diffIn*() and date comparisons on them break or compare text.",
            "hits": hits,
        })
    })
}

/// codemap doctor's findings for one repo: missing code (routes to methods that do not exist) and warnings that
/// point at real problems (not codemap's own reading limits).
fn doctor(repo: &Path) -> Res<(Vec<Value>, Vec<Value>)> {
    let out = Command::new(crate::codemap::binary())
        .args(["doctor", "--json", "--all-warnings", "--project"])
        .arg(repo)
        .output()
        .map_err(|e| format!("codemap doctor: {e}"))?;
    let report: Value = serde_json::from_slice(&out.stdout)
        .map_err(|_| format!("codemap doctor gave no JSON for {}", repo.display()))?;
    let missing = report["routes"]["missing_code"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let warnings = report["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| {
            let m = w["message"].as_str().unwrap_or("");
            let adapter = w["adapter"].as_str().unwrap_or("");
            matches!(adapter, "php" | "laravel" | "laravel-data")
                && (m.contains("could not parse")
                    || m.contains("created twice")
                    || m.contains("does not exist")
                    || m.contains("has no method"))
        })
        .collect();
    Ok((missing, warnings))
}

fn key(item: &Value, fields: &[&str]) -> String {
    fields
        .iter()
        .map(|f| item[*f].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>()
        .join("|")
}

/// The new app's PHP parses every file of the app.
fn lint(config: &Config, side: &Side) -> Res<Value> {
    let rows = crate::runner::run_job(config, side, &json!({"mode": "lint"}), 0)?;
    rows.into_iter()
        .find_map(|r| r.get("lint").cloned())
        .ok_or("the syntax check gave no answer".into())
}

/// PHPStan / Larastan, when the new repo has it installed and configured.
fn phpstan(side: &Side) -> Option<Value> {
    let repo = &side.repo;
    let has = repo.join("vendor/bin/phpstan").exists()
        && ["phpstan.neon", "phpstan.neon.dist", "phpstan.dist.neon"]
            .iter()
            .any(|f| repo.join(f).exists());
    if !has {
        return None;
    }
    say("Running PHPStan (the new repo has it) ...");
    let out = crate::runner::run_in_app(
        side,
        &[
            "php",
            "vendor/bin/phpstan",
            "analyse",
            "--error-format=json",
            "--no-progress",
            "--memory-limit=2G",
        ],
    )
    .ok()?;
    let report: Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut messages: BTreeMap<String, Vec<String>> = BTreeMap::new();
    static NOISE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\w+|'[^']*'|\d+").unwrap());
    for (file, data) in report["files"].as_object().into_iter().flatten() {
        let rel = file
            .strip_prefix(&format!("{}/", side.app_path))
            .unwrap_or(file);
        for m in data["messages"].as_array().into_iter().flatten() {
            let text = m["message"].as_str().unwrap_or("");
            messages
                .entry(NOISE.replace_all(text, "…").into_owned())
                .or_default()
                .push(format!("{rel}:{}  {text}", m["line"]));
        }
    }
    Some(json!({"errors": report["totals"]["file_errors"], "groups": messages}))
}

pub fn check(config: &Config) -> Res<Value> {
    let meta: Value = read(&config.work.join("run.json")).unwrap_or(json!({}));
    let old_v = versions(&config.old.repo, meta["old"]["info"]["php"].as_str());
    let new_v = versions(&config.new.repo, meta["new"]["info"]["php"].as_str());
    say(&format!(
        "Code check: Laravel {} -> {}, Carbon {} -> {}, PHP {} -> {}",
        old_v.laravel.as_deref().unwrap_or("?"),
        new_v.laravel.as_deref().unwrap_or("?"),
        old_v.carbon.as_deref().unwrap_or("?"),
        new_v.carbon.as_deref().unwrap_or("?"),
        old_v.php.as_deref().unwrap_or("?"),
        new_v.php.as_deref().unwrap_or("?"),
    ));

    say("  codemap doctor on both repos ...");
    let (old_missing, old_warnings) = doctor(&config.old.repo).unwrap_or_default();
    let (new_missing, new_warnings) = doctor(&config.new.repo)?;
    // the same route and the same problem, wherever the route file moved
    let was: BTreeSet<String> = old_missing
        .iter()
        .map(|m| key(m, &["route", "detail"]))
        .collect();
    let was_warned: BTreeSet<String> = old_warnings
        .iter()
        .map(|w| key(w, &["path", "message"]))
        .collect();
    let (missing_new, missing_before): (Vec<Value>, Vec<Value>) = new_missing
        .into_iter()
        .partition(|m| !was.contains(&key(m, &["route", "detail"])));
    let (warnings_new, warnings_before): (Vec<Value>, Vec<Value>) = new_warnings
        .into_iter()
        .partition(|w| !was_warned.contains(&key(w, &["path", "message"])));

    say("  upgrade-trap rules ...");
    let mut rules = scan_rules(&config.new.repo, &old_v, &new_v);
    if crossed(&old_v.laravel, &new_v.laravel, (10, 0)) {
        rules.extend(scan_lost_dates(&config.new.repo));
    }

    say("  syntax of every file with the new PHP ...");
    let lint = lint(config, &config.new).unwrap_or_else(|e| json!({"error": e}));
    let phpstan = phpstan(&config.new);

    let result = json!({
        "versions": {"old": old_v, "new": new_v},
        "missing_code": {"new": missing_new, "before": missing_before},
        "warnings": {"new": warnings_new, "before": warnings_before},
        "rules": rules,
        "lint": lint,
        "phpstan": phpstan,
    });
    write(&config.work.join("code.json"), &result)?;
    let hits: usize = rules
        .iter()
        .map(|r| r["hits"].as_array().map_or(0, Vec::len))
        .sum();
    let line = format!(
        "Code: {} routes to missing methods ({} new), {} upgrade-trap hits in {} rules, {} syntax errors in {} files{}",
        result["missing_code"]["new"].as_array().map_or(0, Vec::len)
            + result["missing_code"]["before"]
                .as_array()
                .map_or(0, Vec::len),
        result["missing_code"]["new"].as_array().map_or(0, Vec::len),
        hits,
        rules.len(),
        result["lint"]["errors"].as_array().map_or(0, Vec::len),
        result["lint"]["files"],
        if phpstan.is_some() {
            format!(", PHPStan {} errors", result["phpstan"]["errors"])
        } else {
            String::new()
        }
    );
    say(&line);
    Ok(result)
}

/// The last code check, for the fix list.
pub fn last(config: &Config) -> Option<Value> {
    read(&config.work.join("code.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_jumps() {
        assert!(crossed(
            &Some("v9.52.21".into()),
            &Some("v13.38.0".into()),
            (10, 0)
        ));
        assert!(!crossed(
            &Some("v10.1.0".into()),
            &Some("v13.38.0".into()),
            (10, 0)
        ));
        assert!(crossed(
            &Some("^8.1".into()),
            &Some("8.5.12".into()),
            (8, 4)
        ));
        assert!(!crossed(
            &Some("8.4.1".into()),
            &Some("8.5.12".into()),
            (8, 4)
        ));
    }

    #[test]
    fn rule_patterns() {
        let find = |id: &str, line: &str| {
            Regex::new(RULES.iter().find(|r| r.id == id).unwrap().pattern)
                .unwrap()
                .is_match(line)
        };
        assert!(find(
            "dates-property",
            "    protected $dates = ['paid_at'];"
        ));
        assert!(find("carbon3-diff", "$n = $end->diffInMonths($start) + 1;"));
        assert!(Regex::new(r"(intval\(|\(int\)|abs\(|round\(|floor\(|ceil\().*diffIn\w+\([^()]*(\([^()]*\))?[^()]*,\s*true\s*\)").unwrap()
            .is_match("$d = intval($endOfMonth->diffInDays($startOfMonth, true)) + 1;"));
        assert!(find("removed-helpers", "$x = array_get($a, 'b');"));
        assert!(!find("removed-helpers", "$x = Arr::get($a, 'b');"));
        assert!(find("php8-removed", "while (list($k, $v) = each($arr)) {"));
        assert!(!find("php8-removed", "$items->each(function () {});"));
        assert!(find("string-interpolation", r#"$s = "Hello ${name}";"#));
        assert!(find(
            "implicit-nullable",
            "public function show(Request $request, string $id = null)"
        ));
        assert!(find("float-precision", "$table->double('amount', 15, 2);"));
    }
}
