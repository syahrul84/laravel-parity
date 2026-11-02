//! The report a person reads: what differs between old and new, worst first.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

use serde_json::Value;

use crate::compare::{Outcome, VERDICTS};
use crate::config::Config;
use crate::routes::RouteDiff;

pub struct Skipped {
    pub key: String,
    pub why: String,
}

pub fn write(
    path: &Path,
    config: &Config,
    meta: &Value,
    route_diff: &RouteDiff,
    results: &[Outcome],
    skipped: &[Skipped],
    tests: Option<&Value>,
) -> std::io::Result<()> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in results {
        *counts.entry(r.verdict.as_str()).or_default() += 1;
    }
    let mut out = String::new();
    let _ = writeln!(out, "# Parity report: {}\n", config.name);
    let _ = writeln!(
        out,
        "Made {}.\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    );
    let _ = writeln!(out, "| | Old | New |\n| --- | --- | --- |");
    for key in ["php", "laravel", "mysql", "database"] {
        let cell = |side: &str| meta[side]["info"][key].as_str().unwrap_or("?").to_string();
        let _ = writeln!(out, "| {key} | {} | {} |", cell("old"), cell("new"));
    }
    for (side, kind, version) in crate::ops::missing_capabilities(meta) {
        let _ = writeln!(
            out,
            "\n> **{kind} NOT compared**: the {side} app (Laravel {version}) lacks a Laravel internal the tool reads. \
             Update laravel-parity (`laravel-parity update`); until then {kind} differences are not reported."
        );
    }
    for side in ["old", "new"] {
        if let Some(notes) = meta[side]["auth"].as_object() {
            for (guard, note) in notes {
                let _ = writeln!(
                    out,
                    "\n> {side} app, guard {guard}: {}",
                    note.as_str().unwrap_or_default()
                );
            }
        }
    }

    let _ = writeln!(out, "\n## Summary\n\n| Result | Requests |\n| --- | ---: |");
    for (name, label) in VERDICTS {
        if let Some(n) = counts.get(name) {
            let _ = writeln!(out, "| {label} | {n} |");
        }
    }
    let _ = writeln!(
        out,
        "| Not run (parameter not found, or skipped) | {} |",
        skipped.len()
    );
    let _ = writeln!(
        out,
        "\nRoutes only in the old app: **{}**, only in the new app: **{}**, controller or middleware changed: **{}**.",
        route_diff.removed.len(),
        route_diff.added.len(),
        route_diff.changed.len()
    );
    if let Some(t) = tests {
        // routes, test cases and methods are counted apart: they are different things
        let ran = |c: &Value| match c["made"].as_u64() {
            Some(made) => format!(
                "{made} made, {} pass, {} fail{}",
                c["passed"],
                c["failed"],
                match c["not_run"].as_u64() {
                    Some(n) if n > 0 => format!(", {n} did not run"),
                    _ => String::new(),
                }
            ),
            None => "not run".into(),
        };
        let r = &t["routes"];
        let _ = writeln!(
            out,
            "\nPHPUnit, in `{}`: {} of {} shared routes had no test someone wrote. Feature test cases: {}; {} routes got \
             generated tests, {} with a passing case; {} routes could not get one. Unit tests (one per method): {}; {} \
             methods could not get one (see tests.md).",
            t["dir"].as_str().unwrap_or_default(),
            t["untested"],
            r["shared"],
            ran(&t["feature_cases"]),
            r["with_generated"],
            r["passing"],
            t["cannot_routes"],
            ran(&t["unit_tests"]),
            t["cannot_methods"]
        );
        if let (Some(shared), Some(before), Some(after)) = (
            r["shared"].as_u64().filter(|n| *n > 0),
            r["tested_before"].as_u64(),
            r["tested_after"].as_u64(),
        ) {
            let share = |n: u64| (n as f64 * 1000.0 / shared as f64).round() / 10.0;
            let _ = writeln!(
                out,
                "Routes with a test: {before} of {shared} ({}%) by tests someone wrote; {after} ({}%) with the passing \
                 generated ones.",
                share(before),
                share(after)
            );
        }
        if let Some(line) = t["factories"].as_str() {
            let _ = writeln!(out, "{line}");
        }
        if let Some(line) = crate::coverage::line(&t["coverage"]) {
            let _ = writeln!(out, "{line}");
        }
    }

    for (name, label) in VERDICTS {
        if name == "same" {
            continue;
        }
        let mut group: Vec<&Outcome> = results.iter().filter(|r| r.verdict == name).collect();
        if group.is_empty() {
            continue;
        }
        group.sort_by(|a, b| (&a.key, &a.case).cmp(&(&b.key, &b.case)));
        let _ = writeln!(out, "\n## {label} ({})\n", group.len());
        for r in group {
            let _ = writeln!(
                out,
                "- **{}** `{}` ({} → {})",
                r.name(),
                r.uri,
                r.old_status,
                r.new_status
            );
            for note in r.notes.iter().take(12) {
                let _ = writeln!(out, "  - {note}");
            }
        }
    }

    let _ = writeln!(out, "\n## Route changes\n");
    if !route_diff.removed.is_empty() {
        let _ = writeln!(out, "### Only in the old app\n");
        for k in &route_diff.removed {
            let _ = writeln!(out, "- {k}");
        }
        out.push('\n');
    }
    if !route_diff.added.is_empty() {
        let _ = writeln!(out, "### Only in the new app\n");
        for k in &route_diff.added {
            let _ = writeln!(out, "- {k}");
        }
        out.push('\n');
    }
    if !route_diff.changed.is_empty() {
        // thousands of routes usually share a handful of changes (a middleware added everywhere): group them
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for c in &route_diff.changed {
            let first = &c.changes[0];
            if first.starts_with("action") {
                groups
                    .entry("controller changed".into())
                    .or_default()
                    .push(format!("{}: {first}", c.route));
            } else {
                groups
                    .entry(c.changes.join("; "))
                    .or_default()
                    .push(c.route.clone());
            }
        }
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by_key(|(_, members)| std::cmp::Reverse(members.len()));
        let _ = writeln!(out, "### Changed (grouped by change)\n");
        for (change, members) in groups {
            let examples: Vec<&str> = members.iter().take(3).map(String::as_str).collect();
            let _ = writeln!(
                out,
                "- **{change}**: {} routes, e.g. {}",
                members.len(),
                examples.join(", ")
            );
            if change == "controller changed" {
                for m in &members {
                    let _ = writeln!(out, "  - {m}");
                }
            }
        }
    }

    if !skipped.is_empty() {
        let _ = writeln!(
            out,
            "\n## Not run\n\nFill these in the pair file's `[params]` or in requests.json (`values`), then run again.\n"
        );
        let mut sorted: Vec<&Skipped> = skipped.iter().collect();
        sorted.sort_by(|a, b| a.key.cmp(&b.key));
        for s in sorted {
            let _ = writeln!(out, "- {}: {}", s.key, s.why);
        }
    }
    std::fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rendered_report() {
        let dir =
            std::env::temp_dir().join(format!("laravel-parity-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pair = dir.join("shop.toml");
        std::fs::write(
            &pair,
            "name = \"shop\"\n[old]\nrepo = \"/nonexistent/shop-old\"\n[new]\nrepo = \"/nonexistent/shop\"\n\
             [database]\nsource = \"shop\"\n",
        )
        .unwrap();
        let config = crate::config::load(&pair).unwrap();
        let meta = json!({
            "old": {"info": {"php": "7.4", "laravel": "5.5", "capabilities": {"mail": false}},
                    "auth": {"api": "signed in as user 1"}},
            "new": {"info": {"php": "8.3", "laravel": "11.0", "mysql": "8.0"}},
        });
        let diff: RouteDiff = serde_json::from_value(json!({
            "removed": ["GET api/legacy"], "added": ["GET api/health"],
            "changed": [
                {"route": "GET api/a", "changes": ["middleware: +throttle"]},
                {"route": "GET api/b", "changes": ["middleware: +throttle"]},
                {"route": "GET api/c", "changes": ["action: OrderController@show -> OrderController@view"]},
            ]})).unwrap();
        let outcome = |key: &str, case: &str, verdict: &str, old: i64, new: i64, notes: Value| {
            serde_json::from_value::<Outcome>(json!({"id": key, "key": key, "case": case,
                "uri": key.split(' ').nth(1).unwrap(), "verdict": verdict, "old_status": old,
                "new_status": new, "notes": notes}))
            .unwrap()
        };
        let results = [
            outcome("GET api/shops", "happy", "same", 200, 200, json!([])),
            outcome(
                "GET api/orders",
                "guest",
                "status",
                401,
                200,
                json!(["auth dropped"]),
            ),
            outcome(
                "GET api/cart",
                "happy",
                "body",
                200,
                200,
                json!(["$.total differs"]),
            ),
            outcome("GET api/area", "happy", "body", 200, 200, json!([])),
        ];
        let skipped = [
            Skipped {
                key: "GET api/z/{z}".into(),
                why: "no value for z".into(),
            },
            Skipped {
                key: "GET api/a/{a}".into(),
                why: "no value for a".into(),
            },
        ];
        let tests = json!({"untested": 3, "dir": "tests/Feature/Parity", "cannot_routes": 1, "cannot_methods": 4,
                           "feature_cases": {"made": 2, "passed": 1, "failed": 1, "not_run": 0},
                           "unit_tests": {"made": 5, "passed": 4, "failed": 0, "not_run": 1},
                           "routes": {"shared": 10, "tested_before": 7, "tested_after": 8, "with_generated": 2,
                                      "passing": 1},
                           "factories": "Factories: 2 factories written and checked."});
        let path = dir.join("report.md");
        write(
            &path,
            &config,
            &meta,
            &diff,
            &results,
            &skipped,
            Some(&tests),
        )
        .unwrap();
        let md = std::fs::read_to_string(&path).unwrap();

        assert!(md.starts_with("# Parity report: shop\n\nMade "));
        assert!(md.contains("| php | 7.4 | 8.3 |\n| laravel | 5.5 | 11.0 |\n| mysql | ? | 8.0 |"));
        assert!(md.contains("> **mail NOT compared**: the old app (Laravel 5.5)"));
        assert!(md.contains("> old app, guard api: signed in as user 1"));
        assert!(md.contains(
            "| Different status code | 1 |\n| Different response body | 2 |\n| Same | 1 |"
        ));
        assert!(md.contains("| Not run (parameter not found, or skipped) | 2 |"));
        assert!(
            md.contains("only in the new app: **1**, controller or middleware changed: **3**.")
        );
        assert!(md.contains(
            "PHPUnit, in `tests/Feature/Parity`: 3 of 10 shared routes had no test someone wrote. Feature test cases: 2 made, 1 pass, 1 fail; 2 routes got generated tests, 1 with a passing case; 1 routes could not get one. Unit tests (one per method): 5 made, 4 pass, 0 fail, 1 did not run; 4 methods could not get one (see tests.md)."
        ));
        assert!(md.contains("Routes with a test: 7 of 10 (70%) by tests someone wrote; 8 (80%) with the passing generated ones."));
        assert!(md.contains("Factories: 2 factories written and checked."));
        // each verdict but "same" lists its requests, sorted by route
        assert!(!md.contains("## Same"));
        assert!(md.contains("## Different status code (1)\n\n- **GET api/orders [guest]** `api/orders` (401 → 200)\n  - auth dropped\n"));
        let area = md.find("- **GET api/area**").unwrap();
        assert!(area < md.find("- **GET api/cart**").unwrap());
        assert!(md.contains("### Only in the old app\n\n- GET api/legacy\n"));
        assert!(md.contains("### Only in the new app\n\n- GET api/health\n"));
        // the change most routes share comes first; controller changes list every route
        let throttle = md
            .find("- **middleware: +throttle**: 2 routes, e.g. GET api/a, GET api/b")
            .unwrap();
        let controller = md
            .find("- **controller changed**: 1 routes, e.g. GET api/c: action: ")
            .unwrap();
        assert!(throttle < controller);
        assert!(md.contains("  - GET api/c: action: OrderController@show -> OrderController@view"));
        assert!(
            md.find("- GET api/a/{a}: no value for a").unwrap()
                < md.find("- GET api/z/{z}: no value for z").unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_report() {
        let dir = std::env::temp_dir().join(format!(
            "laravel-parity-report-empty-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pair = dir.join("shop.toml");
        std::fs::write(&pair, "[old]\nrepo = \"/nonexistent/a\"\n[new]\nrepo = \"/nonexistent/b\"\n[database]\nsource = \"shop\"\n").unwrap();
        let config = crate::config::load(&pair).unwrap();
        let path = dir.join("report.md");
        write(
            &path,
            &config,
            &json!({}),
            &RouteDiff::default(),
            &[],
            &[],
            None,
        )
        .unwrap();
        let md = std::fs::read_to_string(&path).unwrap();
        assert!(md.starts_with("# Parity report: shop\n"));
        assert!(md.contains("| database | ? | ? |"));
        assert!(md.contains("| Not run (parameter not found, or skipped) | 0 |"));
        assert!(md.contains("Routes only in the old app: **0**"));
        assert!(!md.contains("PHPUnit") && !md.contains("## Not run") && !md.contains("###"));
        assert!(md.ends_with("## Route changes\n\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
