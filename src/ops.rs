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
use crate::{codemap, db, runner, say, testfile, testgen};

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
                                           "error": format!("PHP stopped on this request: {crash}")});
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
            // the app's own code (dd, exit, a fatal error in app/ or a view) is the route's answer, not a problem
            // with the tool
            if row["status"] == "runner-error" && row["cause"] != "app" {
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
    report_with(config, true)
}

/// Write report.md and the fix list. `speak`: say the result line and record the run in the history (a report made
/// for another step, such as `tests`, does neither: its comparison results are the last run's).
pub fn report_with(config: &Config, speak: bool) -> Res<String> {
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
    if speak {
        crate::history::record(config, &results, &fixes);
    }
    let line = format!(
        "Result: {}. Report: {}. Fix list: {} items in {}.{}",
        counts_line(&results),
        path.display(),
        fixes.len(),
        config.work.join("fixes.md").display(),
        since.unwrap_or_default()
    );
    if speak {
        say(&line);
    }
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
    // a route counts as tested only by a test someone wrote: the tool's own files and its blocks in the app's own test
    // files do not count, or a second run would see its first run's tests and generate nothing
    let mut texts: HashMap<String, Option<(String, bool)>> = HashMap::new();
    let mut untested = BTreeSet::new();
    // one question per route, not per case (a route is sent several ways but tested in one place)
    let mut asked = BTreeSet::new();
    for spec in &specs {
        if !asked.insert(spec.key.as_str()) {
            continue;
        }
        let files = codemap.route_test_files(&spec.method, &spec.template)?;
        // the route's path up to its first parameter, as a test written by hand would have it
        let path = spec
            .template
            .split('{')
            .next()
            .unwrap_or_default()
            .trim_matches('/')
            .to_string();
        let written = files.iter().any(|f| {
            let entry = texts.entry(f.clone()).or_insert_with(|| {
                let text = std::fs::read_to_string(config.new.repo.join(f)).ok()?;
                if testfile::owned(&text) {
                    return None;
                }
                let own = testfile::strip(&text);
                let had_blocks = own.len() != text.len();
                Some((own, had_blocks))
            });
            match entry {
                None => false,
                // codemap may have found the route in the generated block: it counts only when the file's own
                // part names it
                Some((own, true)) => !path.is_empty() && own.contains(&path),
                Some((_, false)) => true,
            }
        });
        if !written {
            untested.insert(spec.key.clone());
        }
    }
    drop(codemap);
    let routes: BTreeSet<&str> = specs.iter().map(|s| s.key.as_str()).collect();
    say(&format!(
        "{} of {} shared routes have no test someone wrote.",
        untested.len(),
        routes.len()
    ));
    // factories for the models that have none, so the tests can make the rows they need (checked on the new app)
    let mut factories_line = None;
    if verify && config.tests.factories() {
        runner::ensure_ready(&config.new)?;
        if let Err(e) = crate::factories::write_missing(config) {
            say(&format!("Factories: not written: {e}"));
        }
        factories_line = crate::factories::last(config).and_then(|f| crate::factories::line(&f));
        if let Some(line) = &factories_line {
            say(line);
        }
    }
    let tables = db::tables(config, &config.new)?;
    let mut generated = testgen::generate(
        config,
        &specs,
        &old_rows,
        &untested,
        &tables,
        &HashSet::new(),
    );
    let route_keys = |list: &[testgen::Made]| -> BTreeSet<String> {
        list.iter().map(|m| m.key.clone()).collect()
    };
    say(&format!(
        "Feature tests: {} test cases for {} routes, in {} files; {} routes could not get one.",
        generated.made.len(),
        route_keys(&generated.made).len(),
        generated.files.len(),
        generated.cannot.len()
    ));
    // unit tests: the app's own classes, method by method, as the old app answered
    if crate::units::last(config).is_none() {
        say(
            "Running the app's classes method by method on both apps (once; `laravel-parity units` runs it again) ...",
        );
        say(&crate::units::check(config, None)?);
    }
    let mut unit = crate::units::generate(config, &HashSet::new());
    say(&format!(
        "Unit tests: {} tests for {} methods, in {} files; {} methods could not get one.",
        unit.made.len(),
        unit.made.len(),
        unit.files.len(),
        unit.cannot.len()
    ));

    let mut outcome = HashMap::new();
    let mut coverage = Value::Null;
    // what ran, counted before --drop-failing takes the failing tests out
    let mut counted: Option<(Counts, Counts)> = None;
    let mut removed = 0;
    if verify && !(generated.files.is_empty() && unit.files.is_empty()) {
        runner::ensure_ready(&config.new)?;
        // coverage: the app's own tests (measured once, reused until they or the code change), then the generated
        // ones while they run anyway
        let mut cover = None;
        let mut own = None;
        if config.tests.coverage() {
            match crate::coverage::driver(&config.new) {
                None => {
                    coverage = json!({"note": crate::coverage::missing_note()});
                    say(&format!(
                        "Code coverage {}.",
                        crate::coverage::missing_note()
                    ));
                }
                Some(driver) => match crate::coverage::script(&config.new) {
                    Ok(script) => {
                        own = Some(crate::coverage::own(
                            config,
                            driver,
                            testgen::phpunit_attributes(&config.new.repo),
                        ));
                        cover = Some((driver, script));
                    }
                    Err(e) => coverage = json!({"note": format!("not measured: {e}")}),
                },
            }
        }
        say("Running the generated tests with the new app's PHP ...");
        let (ran, generated_cov) = testgen::run_phpunit(
            config,
            &[generated.files.clone(), unit.files.clone()].concat(),
            cover.as_ref().map(|(d, s)| (*d, s.as_str())),
        );
        outcome = ran;
        if let Some((driver, script)) = &cover {
            let own = own.unwrap_or(crate::coverage::Own {
                file: None,
                reused: false,
                note: None,
            });
            let ai = crate::coverage::ai_tests(config, *driver, script);
            let mut extra: Vec<&std::path::Path> =
                generated_cov.iter().map(|p| p.as_path()).collect();
            if let Some(file) = &ai.file {
                extra.push(file);
            }
            coverage = match crate::coverage::summary(config, script, own.file.as_deref(), &extra) {
                Ok(mut summary) => {
                    summary["driver"] = json!(driver.name());
                    summary["own_reused"] = json!(own.reused);
                    summary["ai_tests"] = json!({"passed": ai.passed, "failed": ai.failed});
                    summary["target"] = json!(config.tests.target());
                    if let Some(note) = &own.note {
                        summary["own_note"] = json!(note);
                    }
                    summary
                }
                Err(e) => json!({"note": format!("not measured: {e}")}),
            };
            runner::remove(&config.new, script);
            if let Some(line) = crate::coverage::line(&coverage) {
                say(&line);
            }
            if let Some(note) = &own.note {
                say(&format!("Coverage: {note}"));
            }
        }
        let (feature, units) = (
            Counts::of(&generated.made, &outcome),
            Counts::of(&unit.made, &outcome),
        );
        say(&format!(
            "Feature test cases: {}. Unit tests: {}.",
            feature.text(),
            units.text()
        ));
        let failing: HashSet<(String, String)> = outcome
            .iter()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        counted = Some((feature, units));
        if drop_failing && !failing.is_empty() {
            generated = testgen::generate(config, &specs, &old_rows, &untested, &tables, &failing);
            unit = crate::units::generate(config, &failing);
            removed = failing.len();
            say(&format!(
                "Removed the {removed} failing tests from the files; they are listed in tests.md."
            ));
        }
    }

    let stale = crate::layout::remove_stale(config, crate::layout::Kind::Feature, &generated.files)
        + crate::layout::remove_stale(config, crate::layout::Kind::Unit, &unit.files);
    if stale > 0 {
        say(&format!(
            "Took the generated tests out of {stale} files this run did not write again (moved, or their route has a test now)."
        ));
    }
    let key_of: HashMap<(String, String), String> = generated
        .made
        .iter()
        .chain(unit.made.iter())
        .map(|m| ((m.class.clone(), m.method.clone()), m.key.clone()))
        .collect();
    let mut failed: Vec<(&(String, String), &String)> = outcome
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|m| (k, m)))
        .collect();
    failed.sort();

    // routes: a route counts as tested after this run only when one of its generated cases passed
    let tested_before = routes.len() - untested.len();
    let mut passing_routes = BTreeSet::new();
    let mut ran_routes = BTreeSet::new();
    for m in &generated.made {
        match outcome.get(&(m.class.clone(), m.method.clone())) {
            Some(None) => {
                passing_routes.insert(m.key.clone());
                ran_routes.insert(m.key.clone());
            }
            Some(Some(_)) => {
                ran_routes.insert(m.key.clone());
            }
            None => {}
        }
    }
    let with_tests = route_keys(&generated.made);
    let cannot_routes: BTreeSet<&str> = generated.cannot.iter().map(|(k, _)| k.as_str()).collect();
    let all_failing = ran_routes.difference(&passing_routes).count();
    let not_run = with_tests.difference(&ran_routes).count();
    let ran = !outcome.is_empty();
    let tested_after = tested_before + passing_routes.len();
    let share = |n: usize| {
        if routes.is_empty() {
            0.0
        } else {
            (n as f64 * 1000.0 / routes.len() as f64).round() / 10.0
        }
    };
    let routes_line = if ran {
        format!(
            "Routes: {} shared; {tested_before} ({}%) had a test someone wrote; {} got generated tests: {} with a passing case, {all_failing} with every case failing{}; {} could not get one. Routes with a test someone wrote or a passing generated one: {tested_after} ({}%).",
            routes.len(),
            share(tested_before),
            with_tests.len(),
            passing_routes.len(),
            if not_run > 0 {
                format!(", {not_run} that did not run")
            } else {
                String::new()
            },
            cannot_routes.len(),
            share(tested_after)
        )
    } else {
        format!(
            "Routes: {} shared; {tested_before} ({}%) had a test someone wrote; {} got generated tests (not run); {} could not get one.",
            routes.len(),
            share(tested_before),
            with_tests.len(),
            cannot_routes.len()
        )
    };
    let methods_line = match &counted {
        Some((_, units)) => format!(
            "Methods of app/ classes: {} got a unit test ({}); {} could not get one.",
            units.made,
            units.text(),
            unit.cannot.len()
        ),
        None => format!(
            "Methods of app/ classes: {} got a unit test (not run); {} could not get one.",
            unit.made.len(),
            unit.cannot.len()
        ),
    };

    let mut lines = vec!["# Generated tests".to_string(), String::new()];
    lines.push(routes_line.clone());
    lines.push(String::new());
    lines.push(methods_line.clone());
    if let Some((feature, _)) = &counted {
        lines.push(String::new());
        lines.push(format!(
            "Feature test cases (one per way a route was sent): {}.",
            feature.text()
        ));
    }
    if removed > 0 {
        lines.push(String::new());
        lines.push(format!("{removed} failing tests were taken out of the files (--drop-failing); they are listed below."));
    }
    if let Some(line) = &factories_line {
        lines.push(String::new());
        lines.push(line.clone());
    }
    if let Some(line) = crate::coverage::line(&coverage) {
        lines.push(String::new());
        lines.push(line);
        let gains = crate::coverage::gains(&coverage, 10);
        if !gains.is_empty() {
            lines.push(String::new());
            lines.push("| Area of app/ | Lines | Own tests | With the generated ones |".into());
            lines.push("|---|---:|---:|---:|".into());
            for area in &gains {
                let pct = |key: &str| {
                    crate::coverage::percent(&json!({"lines": area["lines"], "covered": area[key]}))
                        .map(|p| format!("{p}%"))
                        .unwrap_or_else(|| "?".into())
                };
                lines.push(format!(
                    "| {} | {} | {} | {} |",
                    area["area"].as_str().unwrap_or_default(),
                    area["lines"],
                    pct("own"),
                    pct("with")
                ));
            }
        }
        if let Some(note) = coverage["own_note"].as_str() {
            lines.push(String::new());
            lines.push(format!("The app's own tests: {note}."));
        }
    }
    lines.push(String::new());
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
    if let Some(found) = crate::factories::last(config) {
        for (key, title) in [
            (
                "failed",
                "## Factories the tool could not make work (not kept)",
            ),
            (
                "broken",
                "## The app's own factories that cannot make a row",
            ),
        ] {
            let list = found[key].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                continue;
            }
            lines.push(title.into());
            lines.push(String::new());
            for f in &list {
                lines.push(format!(
                    "- {}: {}",
                    f["model"].as_str().unwrap_or_default(),
                    f["error"].as_str().unwrap_or_default()
                ));
            }
            lines.push(String::new());
        }
    }
    let mut cannot = generated.cannot.clone();
    cannot.extend(unit.cannot.iter().cloned());
    cannot.sort();
    if !generated.cannot.is_empty() {
        lines.push(format!(
            "## Routes that could not get a test ({})",
            generated.cannot.len()
        ));
        lines.push(String::new());
        let mut list = generated.cannot.clone();
        list.sort();
        lines.extend(list.iter().map(|(k, why)| format!("- {k}: {why}")));
        lines.push(String::new());
    }
    if !unit.cannot.is_empty() {
        lines.push(format!(
            "## Methods that could not get a test ({})",
            unit.cannot.len()
        ));
        lines.push(String::new());
        let mut list = unit.cannot.clone();
        list.sort();
        lines.extend(list.iter().map(|(k, why)| format!("- {k}: {why}")));
    }
    let _ = std::fs::write(config.work.join("tests.md"), lines.join("\n") + "\n");
    let counts_json = |c: &Option<Counts>| match c {
        Some(c) => {
            json!({"made": c.made, "passed": c.passed, "failed": c.failed, "not_run": c.not_run})
        }
        None => Value::Null,
    };
    let summary = json!({
        "untested": untested.len(),
        "generated": generated.made.len() + unit.made.len(),
        "files": generated.files.len() + unit.files.len(),
        "unit": {"generated": unit.made.len(), "files": unit.files.len(), "cannot": unit.cannot.len()},
        "cannot": cannot.len(),
        "cannot_routes": generated.cannot.len(),
        "cannot_methods": unit.cannot.len(),
        "dir": config.tests.place_text(),
        "passed": if ran { json!(outcome.len() - failed.len()) } else { json!("?") },
        "failed": if ran { json!(failed.len()) } else { json!("?") },
        "removed": removed,
        "feature_cases": counts_json(&counted.as_ref().map(|c| c.0.clone())),
        "unit_tests": counts_json(&counted.as_ref().map(|c| c.1.clone())),
        "routes": {"shared": routes.len(), "tested_before": tested_before, "tested_after": tested_after,
                   "with_generated": with_tests.len(), "passing": passing_routes.len(),
                   "all_failing": all_failing, "not_run": not_run, "cannot": cannot_routes.len()},
        "factories": factories_line,
        "coverage": coverage,
    });
    write(&config.work.join("tests.json"), &summary)?;
    write(
        &config.work.join("tests-detail.json"),
        &json!({"failing": failing_json,
                "cannot": generated.cannot.iter().map(|(k, w)| json!({"route": k, "why": w}))
                    .chain(unit.cannot.iter().map(|(k, w)| json!({"method": k, "why": w})))
                    .collect::<Vec<_>>()}),
    )?;
    say(&format!(
        "Details: {}",
        config.work.join("tests.md").display()
    ));
    // report.md shows these numbers; its own result line is the last comparison run's, so it is not repeated here
    report_with(config, false)?;
    let mut out = vec![routes_line, methods_line];
    out.extend(factories_line);
    out.extend(crate::coverage::line(&coverage));
    Ok(out.join(" "))
}

/// Generated tests of one kind and how they ran.
#[derive(Clone)]
struct Counts {
    made: usize,
    passed: usize,
    failed: usize,
    not_run: usize,
}

impl Counts {
    fn of(made: &[testgen::Made], outcome: &HashMap<(String, String), Option<String>>) -> Self {
        let mut counts = Counts {
            made: made.len(),
            passed: 0,
            failed: 0,
            not_run: 0,
        };
        for m in made {
            match outcome.get(&(m.class.clone(), m.method.clone())) {
                Some(None) => counts.passed += 1,
                Some(Some(_)) => counts.failed += 1,
                None => counts.not_run += 1,
            }
        }
        counts
    }

    fn text(&self) -> String {
        let mut text = format!(
            "{} made, {} pass, {} fail",
            self.made, self.passed, self.failed
        );
        if self.not_run > 0 {
            text.push_str(&format!(
                ", {} did not run (PHP stopped or the class did not load)",
                self.not_run
            ));
        }
        text
    }
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

/// What keeps `all` from going on after `check`: an app that gave no answer, a guard whose user does not exist (a
/// token that could not be made is not one: the guard still signs in).
fn start_problems(config: &Config, found: &Value) -> Vec<String> {
    let mut problems = vec![];
    for side in [&config.old, &config.new] {
        let Some(answer) = found.get(side.label) else {
            problems.push(format!("the {} app did not start", side.label));
            continue;
        };
        for (guard, note) in answer["sign_in_problems"].as_object().into_iter().flatten() {
            let note = note.as_str().unwrap_or_default();
            if note.starts_with("no user ") {
                problems.push(format!("{} app, guard {guard}: {note}", side.label));
            }
        }
    }
    problems
}

pub fn all(config: &Config, workers: Option<usize>) -> Res<String> {
    setup_db(config, false)?;
    // both apps must start and sign in before anything is built on them (the guide's first step)
    let found = check(config).map_err(|e| {
        format!("stopped after check: {e}\nFix that, run `laravel-parity check` until both apps start, then `all` again.")
    })?;
    let problems = start_problems(config, &found);
    if !problems.is_empty() {
        return Err(format!(
            "stopped after check: {}\nFix that, run `laravel-parity check` until both apps start and sign in, then `all` again.",
            problems.join("; ")
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_stops_on_an_app_that_cannot_start_or_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let file = repo.join("shop.toml");
        std::fs::write(
            &file,
            format!(
                "[old]\nrepo = '{0}'\nphp = 'php'\n[new]\nrepo = '{0}'\nphp = 'php'\n[database]\nsource = 'shop'\n",
                repo.display()
            ),
        )
        .unwrap();
        let config = crate::config::load(&file).unwrap();
        let fine = json!({"old": {"sign_in_problems": {"api": "token not made (x); signed in through the guard only"}},
                          "new": {"sign_in_problems": {}}});
        assert!(start_problems(&config, &fine).is_empty());
        let broken = json!({"old": {"sign_in_problems": {"admin": "no user 7 for guard admin"}}});
        assert_eq!(
            start_problems(&config, &broken),
            [
                "old app, guard admin: no user 7 for guard admin",
                "the new app did not start"
            ]
        );
    }
}
