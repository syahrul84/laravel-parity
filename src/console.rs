//! What runs outside a request: the app's own artisan commands, its queued jobs and its schedule. Both apps list
//! them; the shared ones run on both apps (one process each, in a rolled-back transaction, with mail, queue,
//! notifications, broadcasts, HTTP and storage faked, and a time limit) and are compared like requests.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::compare::{self, Outcome, Rules};
use crate::config::{Config, Side};
use crate::fixlist::Fix;
use crate::ops::{Res, read, write};
use crate::plan::{Spec, request_id};
use crate::{glob, runner, say};

/// Commands that never end or serve something: never run, whatever the pair file says.
const NEVER: [&str; 8] = [
    "*:work",
    "*:listen",
    "*serve*",
    "*websocket*",
    "*daemon*",
    "*:watch",
    "*reverb:start*",
    "horizon*",
];

fn inventory(config: &Config, side: &Side) -> Res<Value> {
    runner::ensure_ready(side)?;
    let rows = runner::run_job(config, side, &json!({"mode": "console"}), 0)?;
    rows.into_iter()
        .find_map(|r| r.get("console").cloned())
        .ok_or_else(|| format!("the {} app did not list its commands", side.label))
}

/// A scheduled task as the same text on both apps: no PHP binary path, no output redirect.
pub fn task_key(task: &str) -> String {
    static PHP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"^['"]?[^'" ]*php[^'" ]*['"]?\s+['"]?artisan['"]?\s+"#).unwrap()
    });
    static REDIRECT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"\s*>>?\s*['"]?[^'" ]+['"]?\s*2>&1\s*$"#).unwrap());
    let task = PHP.replace(task.trim(), "");
    REDIRECT.replace(&task, "").trim().to_string()
}

fn by_task(list: &Value) -> BTreeMap<String, Value> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter(|e| e.get("error").is_none())
        .map(|e| (task_key(e["task"].as_str().unwrap_or_default()), e.clone()))
        .collect()
}

fn names(list: &Value, key: &str) -> BTreeSet<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e[key].as_str().map(String::from))
        .collect()
}

/// Exit codes and output as a request-like row, so the request comparison applies.
fn as_row(row: &Value) -> Value {
    static DURATION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\d+(\.\d+)?\s?(ms|s|sec|seconds|secs)\b").unwrap());
    let output = row["output"].as_str().unwrap_or_default();
    let mut out = row.clone();
    match &row["status"] {
        Value::Number(n) if n.as_i64() == Some(0) => out["status"] = json!(200),
        Value::Number(n) => {
            let tail: String = output
                .trim()
                .lines()
                .rev()
                .take(3)
                .collect::<Vec<_>>()
                .join(" | ");
            out["status"] = json!(500);
            out["error"] = json!(format!("exit {n}: {tail}"));
        }
        _ => {}
    }
    let normal = DURATION.replace_all(output, "<time>");
    out["kind"] = json!("text");
    out["content_type"] = json!("text/plain");
    out["size"] = json!(normal.len());
    out["sha1"] = json!(format!("{:x}", Sha256::digest(normal.as_bytes())));
    out
}

/// Run the items on one app, `workers` processes at a time, one item per process.
fn run_side(config: &Config, side: &Side, items: &[Value], clock: &str) -> Vec<Value> {
    let queue: Mutex<VecDeque<&Value>> = Mutex::new(items.iter().collect());
    let found: Mutex<Vec<Value>> = Mutex::new(vec![]);
    let total = items.len();
    std::thread::scope(|scope| {
        for slot in 0..config.workers.min(items.len()).max(1) {
            let (queue, found) = (&queue, &found);
            scope.spawn(move || loop {
                let Some(item) = queue.lock().unwrap().pop_front() else { return };
                let job = json!({"mode": "console-run", "clock": clock, "timeout": config.console.timeout,
                                 "users": config.users, "items": [item]});
                // the PHP side stops itself at the limit when it can (pcntl); the tool a little later in any case
                let rows = runner::run_job_timed(config, side, &job, 100 + slot, Some(config.console.timeout + 15));
                let id = item["id"].clone();
                let row = match rows {
                    Ok(rows) => rows
                        .iter()
                        .find(|r| r["id"] == id)
                        .cloned()
                        .unwrap_or_else(|| {
                            if rows.iter().any(|r| r.get("runner_timeout").is_some()) {
                                json!({"id": id, "status": "timeout", "error": format!("still running after {}s", config.console.timeout)})
                            } else {
                                let crash = rows.iter().find_map(|r| r["runner_crash"].as_str()).unwrap_or("no answer");
                                json!({"id": id, "status": "runner-error", "error": format!("PHP stopped: {crash}")})
                            }
                        }),
                    Err(e) => json!({"id": id, "status": "runner-error", "error": e}),
                };
                let mut list = found.lock().unwrap();
                list.push(row);
                say(&format!("  {}: {}/{total}", side.label, list.len()));
            });
        }
    });
    found.into_inner().unwrap()
}

pub fn check(config: &Config) -> Res<Value> {
    say("Listing both apps' commands, jobs and schedule ...");
    let (old, new) = (
        inventory(config, &config.old)?,
        inventory(config, &config.new)?,
    );

    let (old_commands, new_commands) = (
        names(&old["commands"], "name"),
        names(&new["commands"], "name"),
    );
    let (old_jobs, new_jobs) = (names(&old["jobs"], "class"), names(&new["jobs"], "class"));
    let (old_tasks, new_tasks) = (by_task(&old["schedule"]), by_task(&new["schedule"]));
    let mut schedule_changed = vec![];
    for (task, a) in &old_tasks {
        if let Some(b) = new_tasks.get(task) {
            for field in [
                "expression",
                "timezone",
                "without_overlapping",
                "on_one_server",
            ] {
                if a[field] != b[field] {
                    schedule_changed.push(
                        json!({"task": task, "field": field, "old": a[field], "new": b[field]}),
                    );
                }
            }
        }
    }
    let schedule_errors: Vec<Value> = [("old", &old), ("new", &new)]
        .iter()
        .flat_map(|(side, inv)| {
            inv["schedule"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(move |e| {
                    e["error"]
                        .as_str()
                        .map(|x| json!({"app": side, "error": x}))
                })
        })
        .collect();

    // what runs on both apps
    let mut items = vec![];
    let mut not_run: Vec<Value> = vec![];
    if config.console.run {
        for command in new["commands"].as_array().into_iter().flatten() {
            let name = command["name"].as_str().unwrap_or_default();
            if !old_commands.contains(name) {
                continue;
            }
            if NEVER
                .iter()
                .chain(
                    config
                        .console
                        .skip
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .iter(),
                )
                .any(|p| glob::matches(name, p))
            {
                continue;
            }
            let arguments = config.console.commands.get(name).cloned();
            let required: Vec<&str> = command["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let missing: Vec<&&str> = required
                .iter()
                .filter(|r| !arguments.as_ref().is_some_and(|a| a.get(**r).is_some()))
                .collect();
            if !missing.is_empty() {
                not_run.push(json!({"name": format!("command {name}"), "why": format!("needs the argument(s) {}; give them in [console.commands]", missing.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(", "))}));
                continue;
            }
            items.push(json!({"id": request_id(&format!("command {name}")), "kind": "command", "name": name,
                              "arguments": arguments.unwrap_or(json!({}))}));
        }
        for job in new["jobs"].as_array().into_iter().flatten() {
            let class = job["class"].as_str().unwrap_or_default();
            if !old_jobs.contains(class)
                || config.console.skip.iter().any(|p| glob::matches(class, p))
            {
                continue;
            }
            items.push(json!({"id": request_id(&format!("job {class}")), "kind": "job", "class": class,
                              "args": config.console.jobs.get(class).cloned().unwrap_or(json!({}))}));
        }
    }

    let clock = if config.clock == "auto" {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        config.clock.clone()
    };
    let mut outcomes: Vec<Outcome> = vec![];
    if !items.is_empty() {
        say(&format!(
            "Running {} commands and jobs on both apps (at most {}s each) ...",
            items.len(),
            config.console.timeout
        ));
        let (old_rows, new_rows) = std::thread::scope(|scope| {
            let a = scope.spawn(|| run_side(config, &config.old, &items, &clock));
            let b = scope.spawn(|| run_side(config, &config.new, &items, &clock));
            (a.join().unwrap(), b.join().unwrap())
        });
        let lines = |rows: &[Value]| {
            rows.iter()
                .map(|r| r.to_string() + "\n")
                .collect::<String>()
        };
        let _ = std::fs::write(config.work.join("console-old.jsonl"), lines(&old_rows));
        let _ = std::fs::write(config.work.join("console-new.jsonl"), lines(&new_rows));
        let index = |rows: Vec<Value>| -> HashMap<String, Value> {
            rows.into_iter()
                .filter_map(|r| Some((r["id"].as_str()?.to_string(), r)))
                .collect()
        };
        let (old_rows, new_rows) = (index(old_rows), index(new_rows));
        let rules = Rules {
            ignore: &config.ignore_keys,
            unordered: config.unordered_lists,
            not_compared: &[],
        };
        for item in &items {
            let id = item["id"].as_str().unwrap_or_default();
            let key = match item["kind"].as_str() {
                Some("job") => format!("job {}", item["class"].as_str().unwrap_or_default()),
                _ => format!("command {}", item["name"].as_str().unwrap_or_default()),
            };
            let (a, b) = (old_rows.get(id), new_rows.get(id));
            if let Some(why) = [a, b]
                .iter()
                .flatten()
                .find(|r| r["status"] == "not-run")
                .and_then(|r| r["error"].as_str())
            {
                not_run.push(json!({"name": key, "why": why}));
                continue;
            }
            let spec = Spec {
                id: id.to_string(),
                key: key.clone(),
                case: "happy".into(),
                method: "CONSOLE".into(),
                template: key.clone(),
                uri: key.clone(),
                ..Default::default()
            };
            let mut outcome = compare::compare(
                &spec,
                a.map(as_row).as_ref(),
                b.map(as_row).as_ref(),
                &rules,
            );
            if let Some(file) = b
                .and_then(|r| r["file"].as_str())
                .filter(|_| outcome.verdict == "new-broken")
            {
                outcome.notes.push(format!("at {file}"));
            }
            outcomes.push(outcome);
        }
        outcomes.sort_by(|x, y| {
            (compare::rank(&x.verdict), &x.key).cmp(&(compare::rank(&y.verdict), &y.key))
        });
    }

    let result = json!({
        "commands": {"old": old_commands.len(), "new": new_commands.len(),
                     "removed": old_commands.difference(&new_commands).collect::<Vec<_>>(),
                     "added": new_commands.difference(&old_commands).collect::<Vec<_>>()},
        "jobs": {"old": old_jobs.len(), "new": new_jobs.len(),
                 "removed": old_jobs.difference(&new_jobs).collect::<Vec<_>>(),
                 "added": new_jobs.difference(&old_jobs).collect::<Vec<_>>()},
        "schedule": {"old": old_tasks.len(), "new": new_tasks.len(),
                     "missing": old_tasks.keys().filter(|t| !new_tasks.contains_key(*t)).collect::<Vec<_>>(),
                     "added": new_tasks.keys().filter(|t| !old_tasks.contains_key(*t)).collect::<Vec<_>>(),
                     "changed": schedule_changed, "errors": schedule_errors},
        "ran": outcomes.len(),
        "counts": crate::ops::counts_line(&outcomes),
        "outcomes": outcomes,
        "not_run": not_run,
    });
    let line = format!(
        "Commands and jobs: {} compared on both apps ({}); {} not run; schedule: {} tasks missing on the new app, {} changed",
        outcomes.len(),
        if outcomes.is_empty() {
            "nothing compared".to_string()
        } else {
            result["counts"].as_str().unwrap_or_default().to_string()
        },
        result["not_run"].as_array().map_or(0, Vec::len),
        result["schedule"]["missing"].as_array().map_or(0, Vec::len),
        result["schedule"]["changed"].as_array().map_or(0, Vec::len),
    );
    say(&line);
    let mut result = result;
    result["summary"] = json!(line);
    write(&config.work.join("console.json"), &result)?;
    // the fix list picks this up (when there are results to build it from)
    let _ = crate::fixlist::write_all(config);
    Ok(result)
}

pub fn last(config: &Config) -> Option<Value> {
    read(&config.work.join("console.json"))
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|x| {
            x.as_str()
                .map(String::from)
                .unwrap_or_else(|| x.to_string())
        })
        .collect()
}

/// The console check as fix-list items (area Jobs & commands).
pub fn fixes(config: &Config) -> Vec<Fix> {
    let Some(found) = last(config) else {
        return vec![];
    };
    let mut out = vec![];
    let mut add = |priority: u8, title: String, hint: &str, details: Vec<String>| {
        if !details.is_empty() {
            out.push(Fix {
                priority,
                area: "Jobs & commands".into(),
                title,
                location: None,
                hint: Some(hint.into()),
                routes: vec![],
                details,
            });
        }
    };
    let missing = strings(&found["schedule"]["missing"]);
    add(
        1,
        format!(
            "{} scheduled tasks the old app ran are not scheduled on the new app",
            missing.len()
        ),
        "Laravel 11+ moved the schedule from app/Console/Kernel.php to routes/console.php (or bootstrap/app.php withSchedule): move the missing entries over.",
        missing,
    );
    let changed: Vec<String> = found["schedule"]["changed"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            format!(
                "{}: {} {} -> {}",
                c["task"].as_str().unwrap_or_default(),
                c["field"].as_str().unwrap_or_default(),
                c["old"],
                c["new"]
            )
        })
        .collect();
    add(
        2,
        format!(
            "{} scheduled tasks run at another time or with other options",
            changed.len()
        ),
        "Compare the frequency, timezone, withoutOverlapping and onOneServer of each entry.",
        changed,
    );
    let errors: Vec<String> = found["schedule"]["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            format!(
                "{} app: {}",
                e["app"].as_str().unwrap_or_default(),
                e["error"].as_str().unwrap_or_default()
            )
        })
        .collect();
    add(
        2,
        "The schedule could not be read".into(),
        "Run `php artisan schedule:list` to see the error.",
        errors,
    );

    let outcomes: Vec<Outcome> =
        serde_json::from_value(found["outcomes"].clone()).unwrap_or_default();
    let with = |verdicts: &[&str]| -> Vec<String> {
        outcomes
            .iter()
            .filter(|o| verdicts.contains(&o.verdict.as_str()))
            .map(|o| {
                format!(
                    "{}: {}",
                    o.key,
                    o.notes.join("; ").chars().take(400).collect::<String>()
                )
            })
            .collect()
    };
    let broken = with(&["new-broken"]);
    add(
        1,
        format!(
            "{} commands or jobs fail on the new app where the old one ran them",
            broken.len()
        ),
        "Run the command (or dispatch the job) on the new app to see the error; the note gives the file:line.",
        broken,
    );
    let differ = with(&["status", "effects", "body"]);
    add(
        3,
        format!(
            "{} commands or jobs do different work on the new app",
            differ.len()
        ),
        "Rows written, mail, notifications, jobs or output differ: check whether the change is intended.",
        differ,
    );
    let removed_jobs = strings(&found["jobs"]["removed"]);
    add(
        2,
        format!(
            "{} job classes are gone in the new code",
            removed_jobs.len()
        ),
        "Jobs already waiting in the production queue under the old class name fail when a worker picks them up after the release: drain the queue first, or keep an alias class.",
        removed_jobs,
    );
    let removed_commands = strings(&found["commands"]["removed"]);
    add(
        3,
        format!(
            "{} artisan commands are gone in the new code",
            removed_commands.len()
        ),
        "Cron entries, deploy scripts or people that call them by name will fail.",
        removed_commands,
    );
    let both = with(&["both-fail"]);
    add(
        4,
        format!(
            "{} commands or jobs fail on both apps (not caused by the upgrade, so not judged)",
            both.len()
        ),
        "Usually missing data or a required option: give arguments in the pair file's [console.commands] / [console.jobs].",
        both,
    );
    let not_run: Vec<String> = found["not_run"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|n| {
            format!(
                "{}: {}",
                n["name"].as_str().unwrap_or_default(),
                n["why"].as_str().unwrap_or_default()
            )
        })
        .collect();
    add(
        4,
        format!("{} commands or jobs were not run", not_run.len()),
        "Give their arguments in the pair file: [console.commands] \"name\" = { arg = \"value\" }, [console.jobs] \"App\\\\Jobs\\\\X\" = { param = id }.",
        not_run,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks() {
        assert_eq!(
            task_key("'/usr/local/bin/php' 'artisan' report:send --daily > '/dev/null' 2>&1"),
            "report:send --daily"
        );
        assert_eq!(task_key("/usr/bin/php8.2 artisan inspire"), "inspire");
        assert_eq!(task_key("App\\Jobs\\Cleanup"), "App\\Jobs\\Cleanup");
    }
}
