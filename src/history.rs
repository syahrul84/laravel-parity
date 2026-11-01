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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh tool folder holding pairs/<name>.toml; its runs go to <dir>/runs/<name>.
    fn pair(dir: &std::path::Path, name: &str) -> Config {
        let pairs = dir.join("pairs");
        std::fs::create_dir_all(&pairs).unwrap();
        let path = pairs.join(format!("{name}.toml"));
        let toml = format!(
            "name = \"{name}\"\n[old]\nrepo = '{0}/old'\n[new]\nrepo = '{0}/new'\n[database]\nsource = \"shop\"\n",
            dir.display()
        );
        std::fs::write(&path, toml).unwrap();
        config::load(&path).unwrap()
    }

    fn fresh(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "laravel-parity-history-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn outcome(id: &str, key: &str, verdict: &str) -> Outcome {
        serde_json::from_value(json!({"id": id, "key": key, "uri": key, "verdict": verdict,
                                      "old_status": 200, "new_status": 200, "notes": []}))
        .unwrap()
    }

    fn fix(priority: u8) -> Fix {
        Fix {
            priority,
            area: "API".into(),
            title: "routes fail".into(),
            location: None,
            hint: None,
            routes: vec![],
            details: vec![],
        }
    }

    #[test]
    fn since_last_run() {
        let dir = fresh("changes");
        let config = pair(&dir, "shop");
        // no previous results: nothing to compare with
        keep_previous(&config);
        assert!(changes(&config, &[outcome("1", "GET api/a", "same")]).is_none());

        let before = vec![
            outcome("1", "GET api/a", "status"),
            outcome("2", "GET api/b", "same"),
            outcome("3", "POST api/c", "body"),
            outcome("4", "GET api/d", "same"),
        ];
        write(&config.work.join("results.json"), &before).unwrap();
        keep_previous(&config);
        let mut guest = outcome("2", "GET api/b", "status");
        guest.case = "guest".into();
        let now = vec![
            outcome("1", "GET api/a", "same"),
            guest,
            outcome("3", "POST api/c", "effects"),
            outcome("4", "GET api/d", "minor"),
            outcome("5", "GET api/e", "new-broken"),
        ];
        let found = changes(&config, &now).unwrap();
        assert_eq!(found["fixed"], json!(["GET api/a (status -> same)"]));
        assert_eq!(
            found["broken"],
            json!(["GET api/b [guest] (same -> status)"])
        );
        assert_eq!(found["changed"], json!(["POST api/c (body -> effects)"]));
        assert_eq!(found["before"], json!(crate::ops::counts_line(&before)));
        assert_eq!(found["now"], json!(crate::ops::counts_line(&now)));
        let saved: Value = read(&config.work.join("changes.json")).unwrap();
        assert_eq!(saved, found);

        let text = section(&found);
        assert!(text.starts_with(
            "## Since the last run\n\n1 fixed, 1 newly broken, 1 differ in another way.\n\n"
        ));
        let at = |s: &str| text.find(s).unwrap();
        assert!(at("**Newly broken**") < at("**Fixed**"));
        assert!(at("**Fixed**") < at("**Differ in another way**"));
        assert!(text.contains("- GET api/a (status -> same)\n"));

        assert_eq!(
            section(&json!({"fixed": [], "broken": [], "changed": []})),
            ""
        );
        let many: Vec<String> = (0..32).map(|i| format!("GET api/{i}")).collect();
        let long = section(&json!({"fixed": many}));
        assert!(long.contains("32 fixed, 0 newly broken"));
        assert!(long.contains("- GET api/29\n"));
        assert!(!long.contains("- GET api/30\n"));
        assert!(long.contains("- … 2 more in changes.json"));
        assert!(!long.contains("**Newly broken**"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn annotate_under_first_heading() {
        let dir = fresh("annotate");
        let path = dir.join("report.md");
        std::fs::write(&path, "# Report\n\nbody\n").unwrap();
        annotate(&path, "");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# Report\n\nbody\n"
        );
        annotate(&path, "## Since\n\n");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# Report\n\n## Since\n\nbody\n"
        );
        std::fs::write(&path, "# Only a heading").unwrap();
        annotate(&path, "S");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Only a headingS");
        // a missing file stays missing
        annotate(&dir.join("none.md"), "S");
        assert!(!dir.join("none.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_one_line_per_run() {
        let dir = fresh("record");
        let config = pair(&dir, "shop");
        let results = vec![
            outcome("1", "GET api/a", "status"),
            outcome("2", "GET api/b", "status"),
            outcome("3", "GET api/c", "same"),
        ];
        // no run.json: no clock, no line
        record(&config, &results, &[]);
        assert!(history(&config).is_empty());

        write(
            &config.work.join("run.json"),
            &json!({"clock": "2026-10-30 09:00:00"}),
        )
        .unwrap();
        record(&config, &results, &[fix(1), fix(0), fix(3), fix(9)]);
        let lines = history(&config);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["run"], "2026-10-30 09:00:00");
        assert_eq!(lines[0]["verdicts"], json!({"same": 1, "status": 2}));
        // priority 0 counts as 1, 9 as 4
        assert_eq!(lines[0]["fixes"], json!([2, 0, 1, 1]));
        assert_eq!(bad_count(&lines[0]), 2);

        // a later report of the same run replaces its line
        record(&config, &results[2..], &[]);
        let lines = history(&config);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["verdicts"], json!({"same": 1}));
        assert_eq!(bad_count(&lines[0]), 0);

        write(
            &config.work.join("run.json"),
            &json!({"clock": "2026-10-31 09:00:00"}),
        )
        .unwrap();
        record(&config, &results, &[]);
        let lines = history(&config);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["run"], "2026-10-31 09:00:00");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overview_closest_to_release_first() {
        let root = fresh("overview");
        let previous = std::env::var_os("LARAVEL_PARITY_HOME");
        // SAFETY: no other test in this crate reads LARAVEL_PARITY_HOME while this one runs
        unsafe { std::env::set_var("LARAVEL_PARITY_HOME", &root) };
        let empty = overview();

        // alpha: two priority-1 items, first run
        let alpha = pair(&root, "alpha");
        write(
            &alpha.work.join("results.json"),
            &vec![
                outcome("1", "GET api/a", "status"),
                outcome("2", "GET api/b", "same"),
            ],
        )
        .unwrap();
        write(
            &alpha.work.join("fixes.json"),
            &vec![fix(1), fix(1), fix(3)],
        )
        .unwrap();
        // beta: one priority-2 item, two fewer differences than the run before
        let beta = pair(&root, "beta");
        write(
            &beta.work.join("results.json"),
            &vec![
                outcome("1", "GET api/a", "body"),
                outcome("2", "GET api/b", "same"),
                outcome("3", "GET api/c", "minor"),
            ],
        )
        .unwrap();
        write(&beta.work.join("fixes.json"), &vec![fix(2)]).unwrap();
        std::fs::write(
            beta.work.join("history.jsonl"),
            "{\"run\":\"1\",\"verdicts\":{\"status\":2,\"body\":1}}\n{\"run\":\"2\",\"verdicts\":{\"body\":1,\"same\":2}}\n",
        )
        .unwrap();
        // gamma: never run; broken: not a pair file; example: the template
        pair(&root, "gamma");
        std::fs::write(root.join("pairs/broken.toml"), "name = ").unwrap();
        std::fs::write(root.join("pairs/example.toml"), "name = ").unwrap();
        std::fs::write(root.join("pairs/notes.txt"), "x").unwrap();
        let out = overview();

        match previous {
            // SAFETY: as above
            Some(v) => unsafe { std::env::set_var("LARAVEL_PARITY_HOME", v) },
            None => unsafe { std::env::remove_var("LARAVEL_PARITY_HOME") },
        }

        assert!(empty.unwrap_err().starts_with("no pair files in"));
        let out = out.unwrap();
        assert!(out.starts_with("# Upgrades\n"));
        let at = |s: &str| {
            out.find(s)
                .unwrap_or_else(|| panic!("{s} missing in {out}"))
        };
        assert!(at("| beta |") < at("| alpha |"));
        assert!(at("| alpha |") < at("| gamma |"));
        assert!(at("| gamma |") < at("broken.toml | pair file error"));
        assert!(!out.contains("example"));
        assert!(out.contains(
            "| 2 of 3 | 1 | 0 / 1 / 0 | 2 fewer differences | 1 priority-2 items left |"
        ));
        assert!(out.contains(
            "| 1 of 2 | 1 | 2 / 0 / 1 | first run | fix the 2 priority-1 items in runs/alpha/fixes.md |"
        ));
        assert!(
            out.contains("| gamma | not run yet | | | | | `laravel-parity all pairs/gamma.toml` |")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("runs/overview.md")).unwrap(),
            out
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
