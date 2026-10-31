//! CI: fail a build when the upgrade breaks something that was not broken (or accepted) before. `baseline` accepts
//! today's findings; `ci` runs the checks and fails on any finding at or above the chosen priority that the baseline
//! does not have. Writes JUnit XML for CI test reports and, on GitHub Actions, a job summary.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::config::Config;
use crate::fixlist::Fix;
use crate::ops::{Res, read, write};
use crate::say;

/// One finding per route or detail, so a cause that reaches one more route is a new finding. Numbers in titles and
/// details are blurred (counts, timings, ids change between runs).
pub fn entries(fixes: &[Fix], max_priority: u8) -> BTreeSet<(u8, String)> {
    static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
    let blur = |t: &str| DIGITS.replace_all(t, "#").into_owned();
    let mut out = BTreeSet::new();
    for fix in fixes.iter().filter(|f| f.priority <= max_priority) {
        let head = format!("[{}] {}", fix.area, blur(&fix.title));
        if !fix.routes.is_empty() {
            out.extend(
                fix.routes
                    .iter()
                    .map(|r| (fix.priority, format!("{head}: {r}"))),
            );
        } else if !fix.details.is_empty() {
            out.extend(
                fix.details
                    .iter()
                    .map(|d| (fix.priority, format!("{head}: {}", blur(d)))),
            );
        } else {
            out.insert((fix.priority, head));
        }
    }
    out
}

fn baseline_path(config: &Config, file: Option<&Path>) -> PathBuf {
    file.map(Path::to_path_buf)
        .unwrap_or_else(|| config.work.join("baseline.json"))
}

fn current(config: &Config) -> Res<Vec<Fix>> {
    read(&config.work.join("fixes.json"))
        .ok_or_else(|| "no fix list yet: run the checks first".to_string())
}

/// Accept every current finding: later `ci` runs fail only on new ones.
pub fn baseline(config: &Config, file: Option<&Path>) -> Res<String> {
    let found = entries(&current(config)?, 4);
    let path = baseline_path(config, file);
    write(
        &path,
        &json!({"accepted": found.iter().map(|(_, e)| e).collect::<Vec<_>>(),
                         "at": chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()}),
    )?;
    Ok(format!(
        "{} findings accepted in {}",
        found.len(),
        path.display()
    ))
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The checks (unless `no_run`), then the verdict: Err when a finding at priority `fail_on` or worse is new.
pub fn ci(
    config: &Config,
    no_run: bool,
    setup: bool,
    fail_on: u8,
    file: Option<&Path>,
) -> Res<String> {
    if !no_run {
        if setup {
            crate::ops::setup_db(config, false)?;
        }
        crate::ops::routes(config)?;
        crate::code::check(config)?;
        crate::schema::check(config)?;
        if config.web_old.is_some() {
            crate::web::check(config)?;
        }
        crate::ops::plan(config)?;
        crate::ops::run(config, None, false, None)?;
        crate::console::check(config)?;
        crate::channels::check(config)?;
        crate::ops::report(config)?;
    }
    let fixes = current(config)?;
    let accepted: BTreeSet<String> = read::<Value>(&baseline_path(config, file))
        .and_then(|b| b["accepted"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let all = entries(&fixes, 4);
    let new: Vec<&(u8, String)> = all
        .iter()
        .filter(|(p, e)| *p <= fail_on && !accepted.contains(e))
        .collect();

    // JUnit: one test per finding; new ones at the gate's priority fail, accepted ones are skipped
    let mut cases = String::new();
    for (priority, entry) in &all {
        let name = xml(entry);
        let body = if *priority <= fail_on && !accepted.contains(entry) {
            format!("<failure message=\"priority {priority}\">{name}</failure>")
        } else if accepted.contains(entry) {
            "<skipped message=\"accepted in the baseline\"/>".to_string()
        } else {
            String::new()
        };
        cases.push_str(&format!("    <testcase classname=\"laravel-parity.priority-{priority}\" name=\"{name}\">{body}</testcase>\n"));
    }
    let junit = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n  <testsuite name=\"laravel-parity {}\" tests=\"{}\" failures=\"{}\">\n{cases}  </testsuite>\n</testsuites>\n",
        xml(&config.name),
        all.len(),
        new.len()
    );
    let junit_path = config.work.join("junit.xml");
    let _ = std::fs::write(&junit_path, junit);

    let verdict = if new.is_empty() {
        format!(
            "PASS: no new findings at priority {fail_on} or worse ({} findings in all, {} accepted)",
            all.len(),
            accepted.len()
        )
    } else {
        format!(
            "FAIL: {} new findings at priority {fail_on} or worse",
            new.len()
        )
    };
    if let Ok(summary) = std::env::var("GITHUB_STEP_SUMMARY") {
        let mut text = format!("## laravel-parity: {}\n\n{verdict}\n\n", config.name);
        for (p, e) in new.iter().take(50) {
            text.push_str(&format!("- priority {p}: {e}\n"));
        }
        if new.len() > 50 {
            text.push_str(&format!(
                "- … {} more in {}\n",
                new.len() - 50,
                junit_path.display()
            ));
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(summary)
            .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()));
    }
    say(&verdict);
    for (p, e) in new.iter().take(30) {
        say(&format!("  priority {p}: {e}"));
    }
    say(&format!("JUnit report: {}", junit_path.display()));
    if new.is_empty() {
        Ok(verdict)
    } else {
        Err(verdict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_are_per_route_and_blurred() {
        let fixes = vec![
            Fix {
                priority: 1,
                area: "API".into(),
                title: "3 routes fail".into(),
                location: None,
                hint: None,
                routes: vec!["GET api/a".into(), "GET api/b".into()],
                details: vec![],
            },
            Fix {
                priority: 3,
                area: "Speed".into(),
                title: "2 routes slower".into(),
                location: None,
                hint: None,
                routes: vec![],
                details: vec!["GET api/c: 10 -> 90 queries".into()],
            },
        ];
        let found = entries(&fixes, 4);
        assert!(found.contains(&(1, "[API] # routes fail: GET api/a".into())));
        assert!(found.contains(&(
            3,
            "[Speed] # routes slower: GET api/c: # -> # queries".into()
        )));
        assert_eq!(entries(&fixes, 1).len(), 2);
    }
}
