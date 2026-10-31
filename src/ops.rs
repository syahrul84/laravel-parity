//! The steps, shared by the command line and the MCP server. Each returns its outcome or an error; none exits the
//! process, so one bad call cannot take the server down.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::compare::{self, Outcome, Rules};
use crate::config::{Config, Side};
use crate::plan::{self, Spec};
use crate::report::{self, Skipped};
use crate::routes::{self, Route, RouteDiff};
use crate::{codemap, db, runner, say, testgen};

pub type Res<T> = Result<T, String>;

pub fn read<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub fn write<T: Serialize>(path: &Path, value: &T) -> Res<()> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(value).unwrap() + "\n";
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn jsonl(path: &Path) -> HashMap<String, Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|row| Some((row["id"].as_str()?.to_string(), row)))
        .collect()
}

fn tail(text: &str, chars: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(chars)).collect()
}

pub fn setup_db(config: &Config, verbose: bool) -> Res<String> {
    say(&format!(
        "Copying {} into {} and {} ...",
        config.source_db, config.old.database, config.new.database
    ));
    db::copy_source(config)?;
    for side in [&config.old, &config.new] {
        if side.setup.is_empty() {
            continue;
        }
        runner::ensure_ready(side)?;
        let names: Vec<String> = side
            .setup
            .iter()
            .map(|c| c[0].as_str().unwrap_or("?").to_string())
            .collect();
        say(&format!(
            "Preparing {} with the {} app: {}",
            side.database,
            side.label,
            names.join(", ")
        ));
        for command in &side.setup {
            // one command per run, so a failure stops before the next step builds on it
            let rows = runner::run_job(
                config,
                side,
                &json!({"mode": "artisan", "commands": [command]}),
                0,
            )?;
            for row in rows.iter().filter(|r| r.get("command").is_some()) {
                let ok = row["exit"].as_i64() == Some(0);
                let name = row["command"].as_str().unwrap_or("?");
                say(&format!(
                    "  {name}: {}",
                    if ok {
                        "ok".to_string()
                    } else {
                        format!("FAILED (exit {})", row["exit"])
                    }
                ));
                let output = row["output"].as_str().unwrap_or_default().trim();
                if !ok || verbose {
                    say(&format!(
                        "    {}",
                        tail(output, 3000).replace('\n', "\n    ")
                    ));
                }
                if !ok {
                    return Err(format!(
                        "setup of {} stopped at {name}: {}",
                        side.database,
                        tail(output, 1500)
                    ));
                }
            }
        }
    }
    say("Test databases ready.");
    Ok(format!(
        "{} and {} are ready",
        config.old.database, config.new.database
    ))
}

pub fn routes(config: &Config) -> Res<String> {
    let mut found: BTreeMap<&str, BTreeMap<String, Route>> = BTreeMap::new();
    for side in [&config.old, &config.new] {
        let mut codemap = codemap::Codemap::start(&side.repo)?;
        let routes = routes::collect(&mut codemap)?;
        say(&format!(
            "{}: {} routes ({})",
            side.label,
            routes.len(),
            side.repo.display()
        ));
        found.insert(side.label, routes);
    }
    write(&config.work.join("routes-old.json"), &found["old"])?;
    write(&config.work.join("routes-new.json"), &found["new"])?;
    let diff = routes::diff(&found["old"], &found["new"]);
    write(&config.work.join("routes-diff.json"), &diff)?;
    let line = format!(
        "old {} routes, new {}; only old: {}, only new: {}, controller or middleware changed: {}",
        found["old"].len(),
        found["new"].len(),
        diff.removed.len(),
        diff.added.len(),
        diff.changed.len()
    );
    say(&line);
    Ok(line)
}

pub fn plan(config: &Config) -> Res<String> {
    let load = |name: &str| read::<BTreeMap<String, Route>>(&config.work.join(name));
    if load("routes-old.json").is_none() || load("routes-new.json").is_none() {
        routes(config)?;
    }
    let (old, new) = (
        load("routes-old.json").unwrap_or_default(),
        load("routes-new.json").unwrap_or_default(),
    );
    let previous: Vec<Spec> = read(&config.work.join("requests.json")).unwrap_or_default();
    let mut codemap = codemap::Codemap::start(&config.new.repo)?;
    let mut lookup = plan::CodeLookup::new(&mut codemap, &config.new.repo);
    let specs = plan::build(config, &old, &new, &previous, Some(&mut lookup))?;
    write(&config.work.join("requests.json"), &specs)?;
    let unresolved = specs.iter().filter(|s| !s.unresolved.is_empty()).count();
    let by_codemap = specs
        .iter()
        .filter(|s| s.sources.values().any(|v| v.contains("codemap")))
        .count();
    let line = format!(
        "{} requests planned ({by_codemap} with a parameter found through codemap), {unresolved} with a route parameter \
         that could not be filled ({})",
        specs.len(),
        config.work.join("requests.json").display()
    );
    say(&line);
    Ok(line)
}

pub fn runnable(specs: &[Spec], only: Option<&str>) -> (Vec<Spec>, Vec<Skipped>) {
    let mut run = vec![];
    let mut skipped = vec![];
    // a whole "METHOD uri" means that one route; anything else is a part of "METHOD uri" (a prefix, a word)
    let exact = only.is_some_and(|o| specs.iter().any(|s| s.key == o));
    for spec in specs {
        if only.is_some_and(|o| {
            if exact {
                spec.key != o
            } else {
                !spec.key.contains(o)
            }
        }) {
            continue;
        }
        if let Some(why) = &spec.skip {
            skipped.push(Skipped {
                key: spec.name(),
                why: format!("skip: {}", why.as_str().unwrap_or(&why.to_string())),
            });
        } else if !spec.unresolved.is_empty() {
            if spec.case == "happy" {
                skipped.push(Skipped {
                    key: spec.key.clone(),
                    why: format!("no value for {}", spec.unresolved.join(", ")),
                });
            }
        } else {
            run.push(spec.clone());
        }
    }
    // reads first, then writes: a write that leaks past its rollback (another connection) cannot change a read
    run.sort_by(|a, b| {
        (a.method != "GET", &a.key, &a.case).cmp(&(b.method != "GET", &b.key, &b.case))
    });
    (run, skipped)
}

/// The request as the runner gets it: the pair file's default query/body under the request's own.
fn wire(config: &Config, spec: &Spec) -> Value {
    let mut query = config.default_query.clone();
    query.extend(spec.query.clone().unwrap_or_default());
    let body = match (&spec.body, spec.method.as_str()) {
        (_, "GET") | (None, _) => Value::Null,
        (Some(Value::Object(own)), _) => {
            let mut merged = config.default_body.clone();
            merged.extend(own.clone());
            for field in &spec.without {
                merged.remove(field);
            }
            Value::Object(merged)
        }
        (Some(other), _) => other.clone(),
    };
    // "{unit}" in a value stands for the id the route's {unit} parameter got; "{user}" for the signed-in user
    let mut fills: HashMap<String, Value> = spec
        .filled
        .iter()
        .map(|(k, v)| {
            (
                format!("{{{k}}}"),
                v.parse::<i64>().map(Value::from).unwrap_or(json!(v)),
            )
        })
        .collect();
    if let Some(user) = spec.guard.as_ref().and_then(|g| config.users.get(g)) {
        fills.insert("{user}".into(), user.clone());
    }
    let mut query = Value::Object(query);
    let mut body = body;
    fill_placeholders(&mut query, &fills);
    fill_placeholders(&mut body, &fills);
    json!({"id": spec.id, "method": spec.method, "uri": spec.uri, "headers": spec.headers, "guard": spec.guard,
           "query": query, "body": body})
}

/// Replace values that are exactly a placeholder ("{unit}") with what it stands for.
pub fn fill_placeholders(value: &mut Value, fills: &HashMap<String, Value>) {
    match value {
        Value::String(s) => {
            if let Some(v) = fills.get(s.as_str()) {
                *value = v.clone();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| fill_placeholders(v, fills)),
        Value::Object(map) => map.values_mut().for_each(|v| fill_placeholders(v, fills)),
        _ => {}
    }
}

/// Run every request on one app with `workers` PHP processes; a request that kills PHP is recorded and the batch
/// goes on without it.
fn run_side(
    config: &Config,
    side: &Side,
    specs: &[Spec],
    clock: &str,
    out: &Path,
    workers: usize,
) -> Res<Value> {
    let done = jsonl(out);
    let pending: Vec<&Spec> = specs.iter().filter(|s| !done.contains_key(&s.id)).collect();
    let total = specs.len();
    let queue: Mutex<VecDeque<Vec<&Spec>>> =
        Mutex::new(pending.chunks(config.batch).map(|c| c.to_vec()).collect());
    let file = Mutex::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(out)
            .map_err(|e| e.to_string())?,
    );
    let finished = Mutex::new(total - pending.len());
    let meta = Mutex::new(json!({}));
    let error = Mutex::new(None::<String>);

    std::thread::scope(|scope| {
        for slot in 0..workers {
            let (queue, file, finished, meta, error) = (&queue, &file, &finished, &meta, &error);
            scope.spawn(move || loop {
                if error.lock().unwrap().is_some() {
                    return;
                }
                let Some(batch) = queue.lock().unwrap().pop_front() else { return };
                let job = json!({
                    "mode": "replay", "clock": clock, "users": config.users, "headers": config.headers,
                    "requests": batch.iter().map(|s| wire(config, s)).collect::<Vec<_>>(),
                });
                let rows = match runner::run_job(config, side, &job, slot) {
                    Ok(rows) => rows,
                    Err(e) => {
                        *error.lock().unwrap() = Some(e);
                        return;
                    }
                };
                let mut got = HashSet::new();
                let mut crash = String::new();
                let mut restart = false;
                let mut lines = String::new();
                for row in rows {
                    if row.get("runner_restart").is_some() {
                        restart = true;
                    } else if row.get("info").is_some() {
                        *meta.lock().unwrap() = json!({"info": row["info"], "auth": row.get("auth").cloned().unwrap_or(json!({}))});
                    } else if let Some(c) = row["runner_crash"].as_str() {
                        crash = c.to_string();
                    } else if let Some(id) = row["id"].as_str() {
                        got.insert(id.to_string());
                        lines += &(row.to_string() + "\n");
                    }
                }
                let missing: Vec<&Spec> = batch.iter().filter(|s| !got.contains(&s.id)).copied().collect();
                let mut counted = batch.len() - missing.len();
                if restart {
                    // the runner stopped on purpose (its output was spoiled): the rest run again in a new process
                    if !missing.is_empty() {
                        queue.lock().unwrap().push_front(missing.clone());
                    }
                } else if let Some(victim) = missing.first() {
                    let error_row = json!({"id": victim.id, "status": "runner-error",
                                           "error": format!("PHP stopped on this request: {}", tail(&crash, 500))});
                    lines += &(error_row.to_string() + "\n");
                    counted += 1;
                    if missing.len() > 1 {
                        queue.lock().unwrap().push_front(missing[1..].to_vec());
                    }
                }
                if let Err(e) = file.lock().unwrap().write_all(lines.as_bytes()) {
                    *error.lock().unwrap() = Some(format!("{}: {e}", out.display()));
                    return;
                }
                let mut count = finished.lock().unwrap();
                *count += counted;
                say(&format!("  {}: {}/{total}", side.label, *count));
            });
        }
    });
    if let Some(e) = error.into_inner().unwrap() {
        return Err(e);
    }
    Ok(meta.into_inner().unwrap())
}

/// Drop the given requests' rows from a results file, keeping the rest.
fn forget(path: &Path, ids: &HashSet<&str>) -> Res<()> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let kept: String = text
        .lines()
        .filter(|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .is_none_or(|r| !r["id"].as_str().is_some_and(|id| ids.contains(id)))
        })
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(path, kept).map_err(|e| e.to_string())
}

/// Run requests on both apps. Without `only`, a new run (or `resume` to finish one); with `only`, just the matching
/// requests run again and replace their earlier results, so a fixed route can be checked without a full run.
pub fn run(
    config: &Config,
    only: Option<&str>,
    resume: bool,
    workers: Option<usize>,
) -> Res<String> {
    let specs: Vec<Spec> = match read(&config.work.join("requests.json")) {
        Some(s) => s,
        None => {
            plan(config)?;
            read(&config.work.join("requests.json")).unwrap_or_default()
        }
    };
    let (run, skipped) = runnable(&specs, only);
    if run.is_empty() {
        return Err(match only {
            Some(o) => format!(
                "no runnable request matches '{o}' ({} matched but have no parameter value or are skipped)",
                skipped.len()
            ),
            None => "nothing to run".into(),
        });
    }
    let line = send(
        config,
        run,
        skipped.len(),
        only.is_some(),
        resume,
        !resume,
        workers,
    )?;
    if config.learn == 0 {
        return Ok(line);
    }
    // requests the old app refused for missing fields: fill what it named and send them again
    let learned = crate::learn::improve(config, workers)?;
    if learned.starts_with("nothing") {
        return Ok(line);
    }
    say(&learned);
    report(config)
}

/// Send the given requests to both apps and write the report. `partial` keeps the other results (a rerun), `resume`
/// finishes a run that stopped, `keep` saves the current results first for "since the last run".
pub fn send(
    config: &Config,
    run: Vec<Spec>,
    skipped: usize,
    partial: bool,
    resume: bool,
    keep: bool,
    workers: Option<usize>,
) -> Res<String> {
    let meta_file = config.work.join("run.json");
    let partial = partial || resume;
    if keep {
        // "since the last run" compares with these
        crate::history::keep_previous(config);
    }
    let mut meta: Value = if partial {
        read(&meta_file).unwrap_or(json!({}))
    } else {
        json!({})
    };
    // a partial run keeps the clock of the run it adds to, so its answers stay comparable
    let clock = meta["clock"].as_str().map(String::from).unwrap_or_else(|| {
        if config.clock == "auto" {
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
        } else {
            config.clock.clone()
        }
    });
    meta["clock"] = json!(clock);
    write(&meta_file, &meta)?;
    let workers = workers
        .unwrap_or(config.workers)
        .min(run.len().div_ceil(config.batch))
        .max(1);
    say(&format!(
        "Running {} requests on both apps, {workers} PHP processes each (clock fixed at {clock}); {skipped} not run.",
        run.len()
    ));
    for side in [&config.old, &config.new] {
        runner::ensure_ready(side)?;
        let out = config.work.join(format!("results-{}.jsonl", side.label));
        if partial && !resume {
            let ids: HashSet<&str> = run.iter().map(|s| s.id.as_str()).collect();
            forget(&out, &ids)?;
        } else if !resume {
            // both sides start clean, so a half-finished earlier run never mixes with this one
            let _ = std::fs::remove_file(&out);
        }
    }
    // the two apps run side by side
    let (old_meta, new_meta) = std::thread::scope(|scope| {
        let old = scope.spawn(|| {
            run_side(
                config,
                &config.old,
                &run,
                &clock,
                &config.work.join("results-old.jsonl"),
                workers,
            )
        });
        let new = scope.spawn(|| {
            run_side(
                config,
                &config.new,
                &run,
                &clock,
                &config.work.join("results-new.jsonl"),
                workers,
            )
        });
        (old.join().unwrap(), new.join().unwrap())
    });
    meta["old"] = old_meta?;
    meta["new"] = new_meta?;
    write(&meta_file, &meta)?;
    auto_reports(config, &meta, &run);
    report(config)
}

/// The Laravel version of each app, for reports.
pub fn laravel_versions(meta: &Value) -> BTreeMap<String, String> {
    ["old", "new"]
        .iter()
        .filter_map(|s| {
            meta[*s]["info"]["laravel"]
                .as_str()
                .map(|v| (s.to_string(), v.to_string()))
        })
        .collect()
}

/// Problems the tool found in its own run: a Laravel internal it could not use, requests that killed the runner.
fn auto_reports(config: &Config, meta: &Value, run: &[Spec]) {
    let versions = laravel_versions(meta);
    for (side, kind, version) in missing_capabilities(meta) {
        crate::reports::add(crate::reports::New {
            kind: "missing-capability",
            source: "auto",
            step: "run",
            route: None,
            summary: &format!(
                "{kind} cannot be recorded on the {side} app (Laravel {version}): a Laravel internal the runner reads is missing"
            ),
            expected: None,
            actual: None,
            laravel: versions.clone(),
        });
    }
    let keys: HashMap<&str, &str> = run
        .iter()
        .map(|s| (s.id.as_str(), s.key.as_str()))
        .collect();
    for side in ["old", "new"] {
        for (id, row) in jsonl(&config.work.join(format!("results-{side}.jsonl"))) {
            if row["status"] == "runner-error" {
                crate::reports::add(crate::reports::New {
                    kind: "runner-crash",
                    source: "auto",
                    step: "run",
                    route: keys.get(id.as_str()).copied(),
                    summary: &format!(
                        "{side} app: {}",
                        row["error"].as_str().unwrap_or("runner stopped")
                    ),
                    expected: None,
                    actual: None,
                    laravel: versions.clone(),
                });
            }
        }
    }
}

pub fn report(config: &Config) -> Res<String> {
    let specs: Vec<Spec> = read(&config.work.join("requests.json")).unwrap_or_default();
    let (run, skipped) = runnable(&specs, None);
    let meta: Value = read(&config.work.join("run.json")).unwrap_or(json!({}));
    // each app's own URLs (APP_URL ...) differ by environment, not by code: hide them before comparing
    let masks = |side: &str| -> Vec<String> {
        let mut m: Vec<String> = ["url", "web_url"]
            .iter()
            .filter_map(|k| meta[side]["info"][k].as_str().map(String::from))
            .collect();
        m.extend(config.masks.iter().cloned());
        m
    };
    let (old_masks, new_masks) = (masks("old"), masks("new"));
    let old_rows: HashMap<String, Value> = jsonl(&config.work.join("results-old.jsonl"))
        .into_iter()
        .map(|(k, v)| (k, compare::mask(&v, &old_masks)))
        .collect();
    let new_rows: HashMap<String, Value> = jsonl(&config.work.join("results-new.jsonl"))
        .into_iter()
        .map(|(k, v)| (k, compare::mask(&v, &new_masks)))
        .collect();
    let not_compared = missing_capabilities(&meta);
    let kinds: Vec<String> = not_compared
        .iter()
        .map(|(_, kind, _)| kind.clone())
        .collect();
    let rules = Rules {
        ignore: &config.ignore_keys,
        unordered: config.unordered_lists,
        not_compared: &kinds,
    };
    let mut results: Vec<Outcome> = run
        .iter()
        .filter(|s| old_rows.contains_key(&s.id) || new_rows.contains_key(&s.id))
        .map(|s| compare::compare(s, old_rows.get(&s.id), new_rows.get(&s.id), &rules))
        .collect();
    results.sort_by(|a, b| {
        (compare::rank(&a.verdict), &a.key).cmp(&(compare::rank(&b.verdict), &b.key))
    });
    write(&config.work.join("results.json"), &results)?;
    let diff: RouteDiff = read(&config.work.join("routes-diff.json")).unwrap_or_default();
    let tests: Option<Value> = read(&config.work.join("tests.json"));
    let path = config.work.join("report.md");
    report::write(
        &path,
        config,
        &meta,
        &diff,
        &results,
        &skipped,
        tests.as_ref(),
    )
    .map_err(|e| e.to_string())?;
    let fixes = crate::fixlist::write_all(config)?;
    let since = crate::history::changes(config, &results).map(|changes| {
        let section = crate::history::section(&changes);
        crate::history::annotate(&path, &section);
        crate::history::annotate(&config.work.join("fixes.md"), &section);
        let count = |k: &str| changes[k].as_array().map_or(0, Vec::len);
        format!(
            " Since the last run: {} fixed, {} newly broken.",
            count("fixed"),
            count("broken")
        )
    });
    crate::history::record(config, &results, &fixes);
    let line = format!(
        "Result: {}. Report: {}. Fix list: {} items in {}.{}",
        counts_line(&results),
        path.display(),
        fixes.len(),
        config.work.join("fixes.md").display(),
        since.unwrap_or_default()
    );
    say(&line);
    Ok(line)
}

/// (side, capability, laravel version) for every Laravel internal an app could not use.
pub fn missing_capabilities(meta: &Value) -> Vec<(String, String, String)> {
    let mut missing = vec![];
    for side in ["old", "new"] {
        let info = &meta[side]["info"];
        if let Some(caps) = info["capabilities"].as_object() {
            for (name, ok) in caps {
                if ok.as_bool() == Some(false) {
                    let version = info["laravel"].as_str().unwrap_or("?").to_string();
                    missing.push((side.to_string(), name.clone(), version));
                }
            }
        }
    }
    missing
}

pub fn counts_line(results: &[Outcome]) -> String {
    let mut counts: BTreeMap<usize, (&str, usize)> = BTreeMap::new();
    for r in results {
        counts
            .entry(compare::rank(&r.verdict))
            .or_insert((r.verdict.as_str(), 0))
            .1 += 1;
    }
    counts
        .values()
        .map(|(v, n)| format!("{v} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn tests(config: &Config, verify: bool, drop_failing: bool) -> Res<String> {
    let specs: Vec<Spec> = read(&config.work.join("requests.json")).unwrap_or_default();
    let old_rows = jsonl(&config.work.join("results-old.jsonl"));
    if old_rows.is_empty() {
        return Err("run `run` first: tests are built from the old app's answers".into());
    }
    say("Asking codemap which shared routes have no test in the new repo ...");
    let mut codemap = codemap::Codemap::start(&config.new.repo)?;
    // a route counts as tested only by a test someone wrote: the tool's own generated files do not count, or a
    // second run would see its first run's tests and generate nothing
    let mut generated_files: HashMap<String, bool> = HashMap::new();
    let mut untested = BTreeSet::new();
    // one question per route, not per case (a route is sent several ways but tested in one place)
    let mut asked = BTreeSet::new();
    for spec in &specs {
        if !asked.insert(spec.key.as_str()) {
            continue;
        }
        let files = codemap.route_test_files(&spec.method, &spec.template)?;
        let written = files.iter().any(|f| {
            !*generated_files.entry(f.clone()).or_insert_with(|| {
                std::fs::read_to_string(config.new.repo.join(f))
                    .is_ok_and(|t| t.contains(testgen::MARKER))
            })
        });
        if !written {
            untested.insert(spec.key.clone());
        }
    }
    drop(codemap);
    let routes: BTreeSet<&str> = specs.iter().map(|s| s.key.as_str()).collect();
    say(&format!(
        "{} of {} shared routes have no test.",
        untested.len(),
        routes.len()
    ));
    let tables = db::tables(config, &config.new)?;
    let mut generated = testgen::generate(
        config,
        &specs,
        &old_rows,
        &untested,
        &tables,
        &HashSet::new(),
    );
    say(&format!(
        "Generated {} tests in {} files; {} routes could not get one.",
        generated.made.len(),
        generated.files.len(),
        generated.cannot.len()
    ));

    let mut outcome = HashMap::new();
    if verify && !generated.files.is_empty() {
        runner::ensure_ready(&config.new)?;
        say("Running the generated tests with the new app's PHP ...");
        outcome = testgen::run_phpunit(config, &generated.files);
        let failing: HashSet<(String, String)> = outcome
            .iter()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        say(&format!(
            "{} pass, {} fail.",
            outcome.len() - failing.len(),
            failing.len()
        ));
        if drop_failing && !failing.is_empty() {
            generated = testgen::generate(config, &specs, &old_rows, &untested, &tables, &failing);
            say(&format!(
                "Removed the {} failing tests from the files; they are listed in tests.md.",
                failing.len()
            ));
        }
    }

    let key_of: HashMap<(String, String), String> = generated
        .made
        .iter()
        .map(|m| ((m.class.clone(), m.method.clone()), m.key.clone()))
        .collect();
    let mut failed: Vec<(&(String, String), &String)> = outcome
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|m| (k, m)))
        .collect();
    failed.sort();
    let mut lines = vec!["# Generated tests".to_string(), String::new()];
    let mut failing_json = vec![];
    if !failed.is_empty() {
        lines.push("## Failing (a real difference, or a guess the generator got wrong)".into());
        lines.push(String::new());
        for ((class, method), message) in &failed {
            let key = key_of
                .get(&(class.clone(), method.clone()))
                .map(String::as_str)
                .unwrap_or("removed");
            lines.push(format!("- {class}::{method} ({key}): {message}"));
            failing_json
                .push(json!({"class": class, "method": method, "route": key, "message": message}));
        }
        lines.push(String::new());
    }
    let mut cannot = generated.cannot.clone();
    cannot.sort();
    if !cannot.is_empty() {
        lines.push("## Could not be generated (test these by hand)".into());
        lines.push(String::new());
        lines.extend(cannot.iter().map(|(k, why)| format!("- {k}: {why}")));
    }
    let _ = std::fs::write(config.work.join("tests.md"), lines.join("\n") + "\n");
    let ran = !outcome.is_empty();
    let summary = json!({
        "untested": untested.len(),
        "generated": generated.made.len(),
        "files": generated.files.len(),
        "cannot": cannot.len(),
        "dir": config.tests.dir(),
        "passed": if ran { json!(outcome.len() - failed.len()) } else { json!("?") },
        "failed": if ran { json!(failed.len()) } else { json!("?") },
    });
    write(&config.work.join("tests.json"), &summary)?;
    write(
        &config.work.join("tests-detail.json"),
        &json!({"failing": failing_json, "cannot": cannot.iter().map(|(k, w)| json!({"route": k, "why": w})).collect::<Vec<_>>()}),
    )?;
    say(&format!(
        "Details: {}",
        config.work.join("tests.md").display()
    ));
    report(config)?;
    Ok(format!(
        "{} untested routes; {} tests generated in {}, {} pass, {} fail; {} routes could not get a test",
        untested.len(),
        generated.made.len(),
        config.tests.dir(),
        summary["passed"],
        summary["failed"],
        cannot.len()
    ))
}

pub fn check(config: &Config) -> Res<Value> {
    let mut found = json!({});
    for side in [&config.old, &config.new] {
        runner::ensure_ready(side)?;
        let job = json!({"mode": "replay", "users": config.users, "headers": config.headers, "requests": []});
        for row in runner::run_job(config, side, &job, 0)? {
            if row.get("info").is_some() {
                say(&format!("{}: {}", side.label, row["info"]));
                if let Some(auth) = row["auth"].as_object() {
                    for (guard, note) in auth {
                        say(&format!("  {guard}: {}", note.as_str().unwrap_or_default()));
                    }
                }
                let missing: Vec<String> = row["info"]["capabilities"]
                    .as_object()
                    .map(|c| {
                        c.iter()
                            .filter(|(_, ok)| ok.as_bool() == Some(false))
                            .map(|(k, _)| k.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                if !missing.is_empty() {
                    say(&format!(
                        "  NOT compared on this app (a Laravel internal the tool reads is missing; update laravel-parity): {}",
                        missing.join(", ")
                    ));
                }
                found[side.label] = json!({"info": row["info"], "sign_in_problems": row["auth"], "not_compared": missing});
            }
        }
    }
    Ok(found)
}

pub fn all(config: &Config, workers: Option<usize>) -> Res<String> {
    setup_db(config, false)?;
    routes(config)?;
    crate::code::check(config)?;
    crate::schema::check(config)?;
    if config.web_old.is_some() {
        crate::web::check(config)?;
    }
    plan(config)?;
    run(config, None, false, workers)?;
    crate::console::check(config)?;
    // after the run: the channel names come from what the requests and commands broadcast
    crate::channels::check(config)?;
    tests(config, true, false)
}
