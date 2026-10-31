//! Progress over time: what changed since the last run (fixed, newly broken), one history line per run, and an
//! overview of every pair file, so a team upgrading many apps sees which is closest to release.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Write as _;

use serde_json::{Value, json};

use crate::compare::Outcome;
use crate::config::{self, Config};
use crate::fixlist::Fix;
use crate::ops::{Res, read, write};
use crate::{init, say};

/// Verdicts that mean "the upgrade changed something here".
pub const BAD: [&str; 4] = ["new-broken", "status", "effects", "body"];

/// Before a run replaces the results: keep the last ones to compare with.
pub fn keep_previous(config: &Config) {
    let now = config.work.join("results.json");
    if now.exists() {
        let _ = std::fs::copy(&now, config.work.join("results-previous.json"));
    }
}

/// What changed since the previous results: fixed, newly broken, and differing differently.
pub fn changes(config: &Config, results: &[Outcome]) -> Option<Value> {
    let previous: Vec<Outcome> = read(&config.work.join("results-previous.json"))?;
    let before: HashMap<&str, &Outcome> = previous.iter().map(|o| (o.id.as_str(), o)).collect();
    let bad = |v: &str| BAD.contains(&v);
    let (mut fixed, mut broken, mut changed) = (vec![], vec![], vec![]);
    for now in results {
        let Some(was) = before.get(now.id.as_str()) else {
            continue;
        };
        let line = format!("{} ({} -> {})", now.name(), was.verdict, now.verdict);
        match (bad(&was.verdict), bad(&now.verdict)) {
            (true, false) => fixed.push(line),
            (false, true) => broken.push(line),
            (true, true) if was.verdict != now.verdict => changed.push(line),
            _ => {}
        }
    }
    let value = json!({"fixed": fixed, "broken": broken, "changed": changed,
                       "before": crate::ops::counts_line(&previous), "now": crate::ops::counts_line(results)});
    let _ = write(&config.work.join("changes.json"), &value);
    Some(value)
}

/// The "since the last run" section for report.md and fixes.md (empty when nothing changed).
pub fn section(changes: &Value) -> String {
    let list = |k: &str| changes[k].as_array().cloned().unwrap_or_default();
    let (fixed, broken, changed) = (list("fixed"), list("broken"), list("changed"));
    if fixed.is_empty() && broken.is_empty() && changed.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "## Since the last run\n\n{} fixed, {} newly broken, {} differ in another way.\n\n",
        fixed.len(),
        broken.len(),
        changed.len()
    );
    for (label, items) in [
        ("Newly broken", &broken),
        ("Fixed", &fixed),
        ("Differ in another way", &changed),
    ] {
        if items.is_empty() {
            continue;
        }
        let _ = writeln!(out, "**{label}**\n");
        for item in items.iter().take(30) {
            let _ = writeln!(out, "- {}", item.as_str().unwrap_or_default());
        }
        if items.len() > 30 {
            let _ = writeln!(out, "- … {} more in changes.json", items.len() - 30);
        }
        out.push('\n');
    }
    out
}

/// Put the section under the first heading of a markdown file.
pub fn annotate(path: &std::path::Path, section: &str) {
    if section.is_empty() {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let (head, rest) = match text.find("\n\n") {
        Some(i) => text.split_at(i + 2),
        None => (text.as_str(), ""),
    };
    let _ = std::fs::write(path, format!("{head}{section}{rest}"));
}

/// One line per run in history.jsonl (a later report of the same run replaces its line).
pub fn record(config: &Config, results: &[Outcome], fixes: &[Fix]) {
    let meta: Value = read(&config.work.join("run.json")).unwrap_or(json!({}));
    let Some(run) = meta["clock"].as_str() else {
        return;
    };
    let mut verdicts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in results {
        *verdicts.entry(r.verdict.as_str()).or_default() += 1;
    }
    let mut priorities = [0usize; 4];
    for f in fixes {
        priorities[(f.priority.clamp(1, 4) - 1) as usize] += 1;
    }
    let line = json!({"run": run, "at": chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
                      "verdicts": verdicts, "fixes": priorities});
    let path = config.work.join("history.jsonl");
    let kept: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| serde_json::from_str::<Value>(l).is_ok_and(|v| v["run"] != json!(run)))
        .map(String::from)
        .collect();
    if let Ok(mut file) = std::fs::File::create(&path) {
        for l in kept {
            let _ = writeln!(file, "{l}");
        }
        let _ = writeln!(file, "{line}");
    }
}

fn history(config: &Config) -> Vec<Value> {
    std::fs::read_to_string(config.work.join("history.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn bad_count(entry: &Value) -> i64 {
    BAD.iter()
        .map(|v| entry["verdicts"][*v].as_i64().unwrap_or(0))
        .sum()
}

/// Every pair file: where each upgrade stands, closest to release first. Also written to runs/overview.md.
pub fn overview() -> Res<String> {
    let root = init::tool_root();
    let mut rows: Vec<(u64, u64, String)> = vec![];
    for entry in std::fs::read_dir(root.join("pairs"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = entry.path();
        // example.toml is the template, not an upgrade
        if path.extension().is_none_or(|e| e != "toml")
            || path.file_stem().is_some_and(|n| n == "example")
        {
            continue;
        }
        let config = match config::load(&path) {
            Ok(c) => c,
            Err(e) => {
                rows.push((
                    u64::MAX,
                    0,
                    format!("| {} | pair file error: {e} | | | | | |", path.display()),
                ));
                continue;
            }
        };
        let results: Option<Vec<Outcome>> = read(&config.work.join("results.json"));
        let Some(results) = results else {
            rows.push((
                u64::MAX - 1,
                0,
                format!(
                    "| {} | not run yet | | | | | `laravel-parity all pairs/{}.toml` |",
                    config.name, config.name
                ),
            ));
            continue;
        };
        let fixes: Vec<Fix> = read(&config.work.join("fixes.json")).unwrap_or_default();
        let p = |n: u8| fixes.iter().filter(|f| f.priority == n).count() as u64;
        let when = std::fs::metadata(config.work.join("results.json"))
            .and_then(|m| m.modified())
            .map(|t| {
                chrono::DateTime::<chrono::Local>::from(t)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        let same = results
            .iter()
            .filter(|r| r.verdict == "same" || r.verdict == "minor")
            .count();
        let differ = results
            .iter()
            .filter(|r| BAD.contains(&r.verdict.as_str()))
            .count();
        let past = history(&config);
        let trend = match past.len() {
            0 | 1 => "first run".to_string(),
            n => {
                let delta = bad_count(&past[n - 1]) - bad_count(&past[n - 2]);
                match delta {
                    0 => "no change".into(),
                    d if d < 0 => format!("{} fewer differences", -d),
                    d => format!("{d} more differences"),
                }
            }
        };
        let next = if p(1) > 0 {
            format!(
                "fix the {} priority-1 items in runs/{}/fixes.md",
                p(1),
                config.name
            )
        } else if p(2) > 0 {
            format!("{} priority-2 items left", p(2))
        } else {
            "nothing blocking: check the priority-3 items".into()
        };
        rows.push((
            p(1),
            p(2),
            format!(
                "| {} | {when} | {same} of {} | {differ} | {} / {} / {} | {trend} | {next} |",
                config.name,
                results.len(),
                p(1),
                p(2),
                p(3)
            ),
        ));
    }
    if rows.is_empty() {
        return Err(format!("no pair files in {}", root.join("pairs").display()));
    }
    rows.sort();
    let mut out = String::from(
        "# Upgrades\n\nClosest to release first. Fix items: priority 1 (fix before release) / 2 (fix soon) / 3 (check).\n\n\
         | Pair | Last run | Same | Differ | Fix items 1 / 2 / 3 | Since last run | Next |\n| --- | --- | ---: | ---: | --- | --- | --- |\n",
    );
    for (_, _, row) in rows {
        out.push_str(&row);
        out.push('\n');
    }
    let path = root.join("runs").join("overview.md");
    let _ = std::fs::create_dir_all(root.join("runs"));
    let _ = std::fs::write(&path, &out);
    say(&out);
    Ok(out)
}
