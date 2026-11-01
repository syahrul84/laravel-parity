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

    fn fix(priority: u8, title: &str, routes: &[&str], details: &[&str]) -> Fix {
        Fix {
            priority,
            area: "API".into(),
            title: title.into(),
            location: None,
            hint: None,
            routes: routes.iter().map(|r| r.to_string()).collect(),
            details: details.iter().map(|d| d.to_string()).collect(),
        }
    }

    /// A fresh folder holding pairs/shop.toml; its runs go to <dir>/runs/shop.
    fn pair(name: &str) -> (PathBuf, Config) {
        let dir =
            std::env::temp_dir().join(format!("laravel-parity-gate-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pairs")).unwrap();
        let path = dir.join("pairs/shop.toml");
        let toml = format!(
            "name = \"shop<&>\"\nwork = '{0}/runs/shop'\n[old]\nrepo = '{0}/old'\n[new]\nrepo = '{0}/new'\n[database]\nsource = \"shop\"\n",
            dir.display()
        );
        std::fs::write(&path, toml).unwrap();
        let config = crate::config::load(&path).unwrap();
        (dir, config)
    }

    #[test]
    fn entries_cover_every_shape() {
        let fixes = vec![
            fix(2, "Mail changed", &[], &[]),
            fix(2, "Mail changed", &[], &[]),
            fix(4, "Not checked", &[], &["job 12"]),
            fix(
                1,
                "4 routes fail",
                &["GET api/orders/{order}"],
                &["ignored 1"],
            ),
        ];
        let all = entries(&fixes, 4);
        assert_eq!(all.len(), 3);
        // ordered by priority first
        let list: Vec<_> = all.iter().cloned().collect();
        assert_eq!(
            list,
            vec![
                (1, "[API] # routes fail: GET api/orders/{order}".to_string()),
                (2, "[API] Mail changed".to_string()),
                (4, "[API] Not checked: job #".to_string()),
            ]
        );
        assert_eq!(entries(&fixes, 2).len(), 2);
        assert!(entries(&fixes, 0).is_empty());
        assert!(entries(&[], 4).is_empty());
        assert_eq!(xml("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
    }

    #[test]
    fn baseline_then_gate() {
        let (dir, config) = pair("ci");
        // every ci() call appends to the GitHub job summary: keep it in this folder (on GitHub Actions too)
        let file = dir.join("summary.md");
        let previous = std::env::var_os("GITHUB_STEP_SUMMARY");
        // SAFETY: only gate::ci reads GITHUB_STEP_SUMMARY, and only this test calls it
        unsafe { std::env::set_var("GITHUB_STEP_SUMMARY", &file) };
        assert_eq!(
            baseline(&config, None).unwrap_err(),
            "no fix list yet: run the checks first"
        );
        assert!(ci(&config, true, false, 1, None).is_err());

        write(
            &config.work.join("fixes.json"),
            &vec![
                fix(1, "3 routes fail", &["GET api/a", "GET api/b"], &[]),
                fix(3, "Slower", &[], &["GET api/c: 10 -> 90 queries"]),
            ],
        )
        .unwrap();
        let said = baseline(&config, None).unwrap();
        assert!(said.starts_with("3 findings accepted in "));
        assert!(said.ends_with("baseline.json"));
        let saved: Value = read(&config.work.join("baseline.json")).unwrap();
        assert_eq!(saved["accepted"].as_array().unwrap().len(), 3);

        // counts and timings move between runs: still the same findings
        write(
            &config.work.join("fixes.json"),
            &vec![
                fix(1, "5 routes fail", &["GET api/a", "GET api/b"], &[]),
                fix(3, "Slower", &[], &["GET api/c: 11 -> 95 queries"]),
            ],
        )
        .unwrap();
        let pass = ci(&config, true, false, 3, None).unwrap();
        assert_eq!(
            pass,
            "PASS: no new findings at priority 3 or worse (3 findings in all, 3 accepted)"
        );

        // the same cause reaching one more route is new; a new priority-3 item only fails a gate at 3
        write(
            &config.work.join("fixes.json"),
            &vec![
                fix(
                    1,
                    "6 routes fail",
                    &["GET api/a", "GET api/b", "GET api/<x>&"],
                    &[],
                ),
                fix(3, "Slower", &[], &["GET api/d: 1 -> 2 queries"]),
            ],
        )
        .unwrap();
        assert_eq!(
            ci(&config, true, false, 1, None).unwrap_err(),
            "FAIL: 1 new findings at priority 1 or worse"
        );
        assert_eq!(
            ci(&config, true, false, 3, None).unwrap_err(),
            "FAIL: 2 new findings at priority 3 or worse"
        );
        let junit = std::fs::read_to_string(config.work.join("junit.xml")).unwrap();
        assert!(junit.contains(
            "<testsuite name=\"laravel-parity shop&lt;&amp;&gt;\" tests=\"4\" failures=\"2\">"
        ));
        assert!(junit.contains(
            "<testcase classname=\"laravel-parity.priority-1\" name=\"[API] # routes fail: GET api/&lt;x&gt;&amp;\"><failure message=\"priority 1\">"
        ));
        assert!(junit.contains(
            "name=\"[API] # routes fail: GET api/a\"><skipped message=\"accepted in the baseline\"/></testcase>"
        ));
        assert!(junit.contains(
            "name=\"[API] Slower: GET api/d: # -&gt; # queries\"><failure message=\"priority 3\">"
        ));
        // gate at 1: the new priority-3 item is neither failed nor skipped
        ci(&config, true, false, 1, None).unwrap_err();
        let junit = std::fs::read_to_string(config.work.join("junit.xml")).unwrap();
        assert!(junit.contains("name=\"[API] Slower: GET api/d: # -&gt; # queries\"></testcase>"));

        // a baseline in another file: the default one is not read
        let other = dir.join("accepted.json");
        assert!(
            baseline(&config, Some(&other))
                .unwrap()
                .starts_with("4 findings accepted")
        );
        assert!(ci(&config, true, false, 4, Some(&other)).is_ok());
        let _ = std::fs::remove_file(config.work.join("baseline.json"));
        assert_eq!(
            ci(&config, true, false, 4, None).unwrap_err(),
            "FAIL: 4 new findings at priority 4 or worse"
        );

        // the job summary lists the first 50 new findings
        let fixes: Vec<Fix> = (0..52)
            .map(|i| fix(1, "Broken", &[&format!("GET api/r{i:02}")], &[]))
            .collect();
        write(&config.work.join("fixes.json"), &fixes).unwrap();
        std::fs::write(&file, "").unwrap();
        let result = ci(&config, true, false, 2, None);
        match previous {
            // SAFETY: as above
            Some(v) => unsafe { std::env::set_var("GITHUB_STEP_SUMMARY", v) },
            None => unsafe { std::env::remove_var("GITHUB_STEP_SUMMARY") },
        }
        assert_eq!(
            result.unwrap_err(),
            "FAIL: 52 new findings at priority 2 or worse"
        );
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with(
            "## laravel-parity: shop<&>\n\nFAIL: 52 new findings at priority 2 or worse\n\n"
        ));
        assert!(text.contains("- priority 1: [API] Broken: GET api/r49\n"));
        assert!(!text.contains("GET api/r50"));
        assert!(text.contains("- … 2 more in "));
        assert!(text.trim_end().ends_with("junit.xml"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
