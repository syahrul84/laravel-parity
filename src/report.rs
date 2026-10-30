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
        let _ = writeln!(
            out,
            "\nPHPUnit: {} shared routes had no test; {} tests generated in `{}`, {} pass, {} fail, {} could not be \
             generated (see tests.md).",
            t["untested"],
            t["generated"],
            t["dir"].as_str().unwrap_or_default(),
            t["passed"],
            t["failed"],
            t["cannot"]
        );
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
