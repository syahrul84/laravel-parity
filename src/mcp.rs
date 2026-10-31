//! `laravel-parity serve`: the tool as an MCP server (JSON-RPC over stdio), so an AI client can run it, poll long
//! steps, and ask about the results one route at a time.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::compare::Outcome;
use crate::config::{self, Config};
use crate::ops::{self, Res, jsonl, read};
use crate::plan::Spec;
use crate::{guide, init, log};

struct Job {
    name: String,
    pair: String,
    started: Instant,
    finished: Option<Instant>,
    outcome: Option<Result<String, String>>,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);
/// The "new version" line goes into one answer per server run.
static TOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn tools() -> Value {
    let pair = json!({"type": "string", "description": "Path of the pair file (TOML), e.g. pairs/shop.toml, or just its name; list_pairs shows them"});
    let wait = json!({"type": "integer", "description": "Seconds to wait for the result before returning a job to poll with status (default 50)"});
    let schema = |props: Value, required: &[&str]| json!({"type": "object", "properties": props, "required": required});
    json!([
        {"name": "guide", "description": "The step-by-step workflow and what each verdict means. Read it once if unsure how to proceed.",
         "inputSchema": schema(json!({}), &[])},
        {"name": "list_pairs", "description": "The pair files the tool knows (pairs/*.toml): which old and new repo each compares, and whether it has results.",
         "inputSchema": schema(json!({}), &[])},
        {"name": "init_pair", "description": "Write a first pair file for an old and a new repo: finds the Docker container that mounts each repo (or uses a local PHP), the database and the Passport guards. Returns the TODO items to fill in (usually the admin user id).",
         "inputSchema": schema(json!({"old_repo": {"type": "string"}, "new_repo": {"type": "string"},
             "name": {"type": "string", "description": "Short name; default: the new repo's folder name"},
             "force": {"type": "boolean", "description": "Replace an existing pair file"}}), &["old_repo", "new_repo"])},
        {"name": "check", "description": "Boot both apps on their test databases: PHP, Laravel and MySQL versions, and any sign-in problem. Run before setup_db's results are trusted.",
         "inputSchema": schema(json!({"pair": pair}), &["pair"])},
        {"name": "setup_db", "description": "Copy the old app's database into the two *_parity test databases and run each side's setup (the new side's migrations). Takes about a minute. Background job.",
         "inputSchema": schema(json!({"pair": pair, "wait_seconds": wait}), &["pair"])},
        {"name": "plan", "description": "List both apps' routes (codemap) and turn the shared ones into requests, filling route parameters with real ids. Takes a minute or two. Background job.",
         "inputSchema": schema(json!({"pair": pair, "wait_seconds": wait}), &["pair"])},
        {"name": "run", "description": "Run every planned request on both apps and compare. 5-30 minutes. Background job; poll status.",
         "inputSchema": schema(json!({"pair": pair, "workers": {"type": "integer", "description": "PHP processes per app"}, "resume": {"type": "boolean", "description": "Keep earlier results and run only what is missing"}, "wait_seconds": wait}), &["pair"])},
        {"name": "rerun", "description": "Run again one route (its exact 'METHOD uri') or every request whose 'METHOD uri' contains the text (a URL prefix) and replace their results. Use after fixing code.",
         "inputSchema": schema(json!({"pair": pair, "only": {"type": "string"}, "wait_seconds": wait}), &["pair", "only"])},
        {"name": "status", "description": "The running or last job: state, elapsed time, recent progress lines, and its result or error.",
         "inputSchema": schema(json!({"lines": {"type": "integer", "description": "Progress lines to show (default 15)"}}), &[])},
        {"name": "summary", "description": "Counts per verdict, both apps' versions, sign-in problems, route changes, test generation totals, and the routes where the new app fails.",
         "inputSchema": schema(json!({"pair": pair}), &["pair"])},
        {"name": "results", "description": "Compared routes, worst first, filtered by verdict (new-broken, status, effects, body, minor, new-fixed, both-fail, same) and/or text in 'METHOD uri' or the notes.",
         "inputSchema": schema(json!({"pair": pair, "verdict": {"type": "string"}, "contains": {"type": "string"},
             "limit": {"type": "integer", "description": "Default 30"}, "offset": {"type": "integer"}}), &["pair"])},
        {"name": "show", "description": "One route in full: the request sent, the old and the new answer (status, JSON, exception, log lines, side effects) and the differences. Give 'METHOD uri' or a unique part of it.",
         "inputSchema": schema(json!({"pair": pair, "route": {"type": "string"}, "max_chars": {"type": "integer", "description": "Per answer body, default 4000"}}), &["pair", "route"])},
        {"name": "set_request", "description": "Change what a planned request sends (saved in requests.json, kept by later plans): a JSON body, query, headers, route parameter values, or skip it with a reason. Then rerun it.",
         "inputSchema": schema(json!({"pair": pair, "route": {"type": "string", "description": "'METHOD uri' (template), or a unique part"},
             "body": {"type": "object"}, "query": {"type": "object"}, "headers": {"type": "object"},
             "values": {"type": "object", "description": "Route parameter -> value, e.g. {\"lease\": \"42\"}"},
             "skip": {"type": "string", "description": "Reason to stop running this request; empty string to run it again"}}), &["pair", "route"])},
        {"name": "generate_tests", "description": "Generate PHPUnit tests in the new repo for shared routes that have none (codemap decides), from the old app's answers, and run them. Several minutes. Background job.",
         "inputSchema": schema(json!({"pair": pair, "verify": {"type": "boolean", "description": "Run the tests (default true)"},
             "drop_failing": {"type": "boolean", "description": "Remove the generated tests that fail"}, "wait_seconds": wait}), &["pair"])},
        {"name": "report_problem", "description": "Tell the tool's makers about a problem with laravel-parity itself (not with the app): a verdict that is wrong, an answer that was confusing or missing something, a fix that did not show up on rerun, a step that failed. Kept on this machine; the person can send it to GitHub later with `laravel-parity reports --send`. Do not include customer data: give the route pattern and what you expected versus saw.",
         "inputSchema": schema(json!({"kind": {"type": "string", "enum": ["wrong-verdict", "confusing-answer", "missing-feature", "fix-did-not-work", "step-failed", "other"]},
             "summary": {"type": "string", "description": "One or two sentences"}, "route": {"type": "string", "description": "'METHOD uri' pattern, if about one route"},
             "expected": {"type": "string"}, "actual": {"type": "string"}, "pair": pair}), &["kind", "summary"])},
        {"name": "fix_list", "description": "What to fix after the upgrade, worst first: API crashes grouped by cause with the file:line in the new code and the likely cause, routes now open without sign-in or no longer reachable, validation that changed, web app calls the upgrade breaks (with the web file:line), lost response fields, side effects. Also written to runs/<name>/fixes.md for people.",
         "inputSchema": schema(json!({"pair": pair, "priority": {"type": "integer", "description": "Only this priority: 1 fix before release, 2 fix soon, 3 check, 4 know about"},
             "area": {"type": "string", "description": "API, Access, Validation, UI, Code, Database, Jobs & commands, Broadcasting, Response, Side effects, Speed, Not checked"}, "limit": {"type": "integer", "description": "Default 20"}}), &["pair"])},
        {"name": "code_check", "description": "Problems in the upgraded code found without running it: routes to controller methods that do not exist (codemap doctor, new vs already there), upgrade traps chosen by the version jump ($dates removed in Laravel 10, Carbon 3 signed diffIn*, removed helpers, PHP 8.x removals and deprecations) with file:line and the fix, files the new PHP cannot parse, and PHPStan when installed. Results go into fix_list (area Code). Background job.",
         "inputSchema": schema(json!({"pair": pair, "wait_seconds": wait}), &["pair"])},
        {"name": "web_check", "description": "Read the web app (Vue 2, Vue 3, Nuxt, React; [web] in the pair file) with codemap twice: old web against the old API, shipping web against the new API. Returns the web calls the upgrade breaks (route gone, wrong method, required field not sent, response field gone) with their file:line. Background job.",
         "inputSchema": schema(json!({"pair": pair, "wait_seconds": wait}), &["pair"])},
        {"name": "test_gaps", "description": "Routes whose generated tests cover few scenarios, thinnest first, each with what you need to add more: the controller method's file:line, the cases already sent and what the old app answered, the request body, and the validation rules found. Read the controller, then add scenarios with add_case. Use after generate_tests when coverage looks thin.",
         "inputSchema": schema(json!({"pair": pair, "route": {"type": "string", "description": "Only this route ('METHOD uri' or a unique part)"},
             "max_cases": {"type": "integer", "description": "Routes with at most this many tested cases (default 2)"}, "limit": {"type": "integer", "description": "Default 10"}}), &["pair"])},
        {"name": "add_case", "description": "Add a test scenario to a route: a name and what to send differently from its happy case (body fields merged over the happy body, fields to leave out, query, route parameter values, signed in or not). The tool sends it to BOTH apps on rerun; the OLD app's answer becomes the expected result, and generate_tests adds it as a data-provider row. Use \"{param}\" (a route parameter name, e.g. \"{unit}\") or \"{user}\" as a value to mean that row's id / the signed-in user; a value tied to a specific database row will not exist in the test database. Good scenarios: boundary values (max length, 0, negative), each value of an in: rule, a filter / sort / page the controller reads, another project's id, a wrong type, a duplicate for a unique rule. Then rerun the route and generate_tests.",
         "inputSchema": schema(json!({"pair": pair, "route": {"type": "string", "description": "'METHOD uri' (template) or a unique part"},
             "name": {"type": "string", "description": "Short scenario name, e.g. 'name-too-long'; stored as ai-<name>"},
             "body": {"type": "object", "description": "Fields merged over the happy case's body"},
             "without": {"type": "array", "items": {"type": "string"}, "description": "Fields to leave out"},
             "query": {"type": "object"}, "values": {"type": "object", "description": "Route parameter -> value"},
             "signed_in": {"type": "boolean", "description": "Default: as the happy case"},
             "note": {"type": "string", "description": "What the scenario checks"}}), &["pair", "route", "name"])},
        {"name": "schema_check", "description": "Compare the database itself, read from information_schema: (1) the old app's real database against its copy after the new app's migrations ran (what the release does to production: dropped tables or columns, narrowed types, lost unique keys and indexes, NOT NULL added); (2) the old database against an empty one built by the new app's migrations alone (what a new install, CI or a RefreshDatabase test gets; after moving from Propel or hand-made SQL to Laravel migrations this shows what they do not describe). Results go into fix_list (area Database). Background job.",
         "inputSchema": schema(json!({"pair": pair, "wait_seconds": wait}), &["pair"])},
        {"name": "console_check", "description": "The app's own artisan commands, queued jobs (app/Jobs) and schedule on both apps: scheduled tasks missing or changed on the new app (Laravel 11 moved the schedule to routes/console.php), job classes gone (queued jobs fail after release), and every shared command and job RUN on both apps (rolled back, mail/queue/notifications/HTTP faked, time-limited) and compared: exit code, output, rows written, mail, jobs. Commands needing arguments and jobs needing ids take them from the pair file's [console]. Results go into fix_list (area Jobs & commands). Background job.",
         "inputSchema": schema(json!({"pair": pair, "wait_seconds": wait}), &["pair"])},
        {"name": "channels_check", "description": "Broadcasting after the upgrade (Laravel Echo with Pusher/Redis -> Reverb): asks both apps who may join each private and presence channel (routes/channels.php, the way /broadcasting/auth does) for the signed-in users, using the channel names the requests broadcast on during run; whether /broadcasting/auth still exists and its middleware; the broadcast driver and missing Reverb settings. Run after run. Results go into fix_list (area Broadcasting). Background job.",
         "inputSchema": schema(json!({"pair": pair, "wait_seconds": wait}), &["pair"])},
        {"name": "import_log", "description": "Add real requests from a web server access log (nginx/Apache combined, a file path on this machine) as extra cases log-<n>: successful GETs matched to planned routes, with the ids, filters, sorting and paging real users sent (query values that look like secrets are dropped). Then run (or rerun the routes) to compare them.",
         "inputSchema": schema(json!({"pair": pair, "log": {"type": "string", "description": "Path of the access log"},
             "per_route": {"type": "integer", "description": "At most this many different requests per route (default 5)"}}), &["pair", "log"])},
        {"name": "overview", "description": "Every pair file at once: last run, how many routes answer the same, fix items per priority, the change since the previous run, and the next step; closest to release first. For a team upgrading many apps.",
         "inputSchema": schema(json!({}), &[])},
        {"name": "test_results", "description": "Generated-test totals, the failing tests with their messages, and the routes that could not get a test and why.",
         "inputSchema": schema(json!({"pair": pair, "limit": {"type": "integer", "description": "Default 40 per list"}}), &["pair"])},
    ])
}

fn pair_path(args: &Value) -> Res<PathBuf> {
    let raw = args["pair"].as_str().ok_or("pair is required")?;
    let path = config::expand(raw);
    if path.is_absolute() || path.exists() {
        return Ok(path);
    }
    let in_tool = init::tool_root().join(&path);
    if in_tool.exists() {
        return Ok(in_tool);
    }
    let named = init::tool_root().join("pairs").join(format!("{raw}.toml"));
    if named.exists() {
        Ok(named)
    } else {
        Err(format!(
            "pair file {raw} not found (list_pairs shows the known ones)"
        ))
    }
}

fn load(args: &Value) -> Res<Config> {
    let config = config::load(&pair_path(args)?)?;
    let _ = std::fs::create_dir_all(&config.work);
    Ok(config)
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap()
}

/// Start a background step; wait up to wait_seconds for it to finish.
fn start(
    name: &str,
    args: &Value,
    work: impl FnOnce(Config) -> Res<String> + Send + 'static,
) -> Res<String> {
    let config = load(args)?;
    {
        let mut job = JOB.lock().unwrap();
        if let Some(j) = job.as_ref().filter(|j| j.outcome.is_none()) {
            return Err(format!(
                "{} on {} is still running ({}s); poll status, one job at a time",
                j.name,
                j.pair,
                j.started.elapsed().as_secs()
            ));
        }
        log::clear();
        *job = Some(Job {
            name: name.into(),
            pair: config.name.clone(),
            started: Instant::now(),
            finished: None,
            outcome: None,
        });
    }
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(config)))
            .unwrap_or_else(|_| {
                crate::reports::add(crate::reports::New {
                    kind: "panic",
                    source: "auto",
                    step: "job",
                    route: None,
                    summary: "a background job panicked",
                    expected: None,
                    actual: None,
                    laravel: Default::default(),
                });
                Err("the job panicked; it was recorded as a problem report".into())
            });
        let mut job = JOB.lock().unwrap();
        if let Some(j) = job.as_mut() {
            j.finished = Some(Instant::now());
            j.outcome = Some(result);
        }
    });
    let wait = Duration::from_secs(args["wait_seconds"].as_u64().unwrap_or(50));
    let begun = Instant::now();
    while begun.elapsed() < wait {
        std::thread::sleep(Duration::from_millis(300));
        if let Some(Some(outcome)) = JOB.lock().unwrap().as_ref().map(|j| j.outcome.clone()) {
            return outcome.map(|r| format!("{name} finished: {r}"));
        }
    }
    Ok(format!(
        "{name} is running in the background ({}s so far). Poll status for progress and the result.",
        begun.elapsed().as_secs()
    ))
}

fn status(args: &Value) -> String {
    let lines = args["lines"].as_u64().unwrap_or(15) as usize;
    let job = JOB.lock().unwrap();
    let Some(j) = job.as_ref() else {
        return "No job has run since the server started.".into();
    };
    let elapsed = j
        .finished
        .unwrap_or_else(Instant::now)
        .duration_since(j.started)
        .as_secs();
    let state = match &j.outcome {
        None => "running".to_string(),
        Some(Ok(r)) => format!("done: {r}"),
        Some(Err(e)) => format!("FAILED: {e}"),
    };
    format!(
        "{} on {}: {state} ({elapsed}s)\n\nRecent progress:\n{}",
        j.name,
        j.pair,
        log::tail(lines).join("\n")
    )
}

fn results_of(config: &Config) -> Res<Vec<Outcome>> {
    read(&config.work.join("results.json")).ok_or_else(|| "no results yet: run first".to_string())
}

fn summary(config: &Config) -> Res<String> {
    let results = results_of(config)?;
    let meta: Value = read(&config.work.join("run.json")).unwrap_or(json!({}));
    let diff: Value = read(&config.work.join("routes-diff.json")).unwrap_or(json!({}));
    let specs: Vec<Spec> = read(&config.work.join("requests.json")).unwrap_or_default();
    let not_run = specs
        .iter()
        .filter(|s| s.skip.is_some() || !s.unresolved.is_empty())
        .count();
    let broken: Vec<Value> = results
        .iter()
        .filter(|r| r.verdict == "new-broken")
        .map(|r| json!({"route": r.name(), "uri": r.uri, "status": format!("{} -> {}", r.old_status, r.new_status), "why": r.notes.first()}))
        .collect();
    let len = |v: &Value| v.as_array().map_or(0, Vec::len);
    Ok(pretty(&json!({
        "counts": ops::counts_line(&results),
        "not_run": not_run,
        "old": meta["old"]["info"], "new": meta["new"]["info"],
        "sign_in_problems": {"old": meta["old"]["auth"], "new": meta["new"]["auth"]},
        "clock": meta["clock"],
        "routes": {"only_old": len(&diff["removed"]), "only_new": len(&diff["added"]), "changed": len(&diff["changed"])},
        "tests": read::<Value>(&config.work.join("tests.json")),
        "since_last_run": read::<Value>(&config.work.join("changes.json")).map(|c| json!({
            "fixed": c["fixed"].as_array().map_or(0, Vec::len), "newly_broken": c["broken"], "before": c["before"]})),
        "database": crate::schema::last(config).map(|v| v["summary"].clone()),
        "commands_and_jobs": crate::console::last(config).map(|v| v["summary"].clone()),
        "channels": crate::channels::last(config).map(|v| v["summary"].clone()),
        "new_broken": broken,
        "report": config.work.join("report.md"),
        "next": "results with a verdict lists the rest; show gives one route in full",
    })))
}

fn results(config: &Config, args: &Value) -> Res<String> {
    let verdict = args["verdict"].as_str();
    let contains = args["contains"].as_str().map(str::to_lowercase);
    let limit = args["limit"].as_u64().unwrap_or(30) as usize;
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let all = results_of(config)?;
    let matching: Vec<&Outcome> = all
        .iter()
        .filter(|r| verdict.is_none_or(|v| r.verdict == v))
        .filter(|r| {
            contains.as_ref().is_none_or(|c| {
                r.key.to_lowercase().contains(c)
                    || r.notes.iter().any(|n| n.to_lowercase().contains(c))
            })
        })
        .collect();
    let page: Vec<Value> = matching
        .iter()
        .skip(offset)
        .take(limit)
        .map(|r| json!({"route": r.name(), "uri": r.uri, "verdict": r.verdict, "status": format!("{} -> {}", r.old_status, r.new_status), "notes": r.notes.iter().take(4).collect::<Vec<_>>()}))
        .collect();
    Ok(pretty(
        &json!({"total": matching.len(), "offset": offset, "shown": page.len(), "results": page}),
    ))
}

fn find_spec<'a>(specs: &'a [Spec], route: &str) -> Res<&'a Spec> {
    // "GET api/x [guest]" names a case; "GET api/x" alone means its happy case
    let (route, case) = match route
        .trim()
        .strip_suffix(']')
        .and_then(|r| r.rsplit_once(" ["))
    {
        Some((r, c)) => (r.trim(), c),
        None => (route.trim(), "happy"),
    };
    if let Some(s) = specs
        .iter()
        .find(|s| s.id == route || (s.key == route && s.case == case))
    {
        return Ok(s);
    }
    let matches: Vec<&Spec> = specs
        .iter()
        .filter(|s| s.case == case && (s.key.contains(route) || s.uri.contains(route)))
        .collect();
    match matches.len() {
        0 => Err(format!("no planned request matches '{route}'")),
        1 => Ok(matches[0]),
        n => Err(format!(
            "'{route}' matches {n} requests; be more specific, e.g. {}",
            matches
                .iter()
                .take(5)
                .map(|s| s.key.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        )),
    }
}

fn trimmed(row: Option<&Value>, max: usize) -> Value {
    let Some(row) = row else { return Value::Null };
    let mut row = row.clone();
    // a debug-mode exception: its message and first frames say where it happened
    if let Some(trace) = row["json"]["trace"].as_array().cloned() {
        let frames: Vec<Value> = trace
            .iter()
            .take(8)
            .map(|f| {
                json!(format!(
                    "{}:{} {}{}{}",
                    f["file"].as_str().unwrap_or("?"),
                    f["line"],
                    f["class"].as_str().unwrap_or(""),
                    f["type"].as_str().unwrap_or(""),
                    f["function"].as_str().unwrap_or("")
                ))
            })
            .collect();
        row["json"]["trace"] = json!(frames);
        return row;
    }
    // the same warning logged a thousand times is one line
    if let Some(logs) = row["effects"]["logs"].as_array().cloned() {
        let mut unique: Vec<String> = vec![];
        for line in logs.iter().filter_map(Value::as_str) {
            let line: String = line.chars().take(300).collect();
            if !unique.contains(&line) {
                unique.push(line);
            }
        }
        let total = unique.len();
        unique.truncate(10);
        if total > 10 {
            unique.push(format!("... {} more different lines", total - 10));
        }
        row["effects"]["logs"] = json!(unique);
    }
    if let Some(writes) = row["effects"]["writes"].as_array().cloned()
        && writes.len() > 20
    {
        row["effects"]["writes"] = json!(
            writes
                .into_iter()
                .take(20)
                .chain([json!("... more writes")])
                .collect::<Vec<_>>()
        );
    }
    for key in ["json", "text"] {
        if let Some(v) = row.get(key) {
            let text = if v.is_string() {
                v.as_str().unwrap().to_string()
            } else {
                v.to_string()
            };
            if text.chars().count() > max {
                let mut cut: String = text.chars().take(max).collect();
                cut += &format!("... ({} chars in all)", text.chars().count());
                row[key] = json!(cut);
            }
        }
    }
    row
}

fn show(config: &Config, args: &Value) -> Res<String> {
    let specs: Vec<Spec> =
        read(&config.work.join("requests.json")).ok_or("no plan yet: run plan first")?;
    let spec = find_spec(&specs, args["route"].as_str().ok_or("route is required")?)?;
    let max = args["max_chars"].as_u64().unwrap_or(4000) as usize;
    let old = jsonl(&config.work.join("results-old.jsonl")).remove(&spec.id);
    let new = jsonl(&config.work.join("results-new.jsonl")).remove(&spec.id);
    let outcome = results_of(config)
        .ok()
        .and_then(|r| r.into_iter().find(|o| o.id == spec.id));
    Ok(pretty(&json!({
        "route": spec.key, "case": spec.case, "uri": spec.uri, "guard": spec.guard,
        "other_cases": specs.iter().filter(|s| s.key == spec.key && s.id != spec.id).map(|s| format!("{} [{}]", s.key, s.case)).collect::<Vec<_>>(),
        "action": {"old": spec.old_action, "new": spec.action},
        "request": {"query": spec.query, "body": spec.body, "headers": spec.headers, "parameters": spec.filled, "found_by": spec.sources,
                    "defaults_added": {"query": config.default_query, "body": config.default_body}},
        "unresolved": spec.unresolved,
        "verdict": outcome.as_ref().map(|o| o.verdict.clone()),
        "differences": outcome.map(|o| o.notes),
        "old": trimmed(old.as_ref(), max),
        "new": trimmed(new.as_ref(), max),
    })))
}

fn set_request(config: &Config, args: &Value) -> Res<String> {
    let path = config.work.join("requests.json");
    let mut specs: Vec<Spec> = read(&path).ok_or("no plan yet: run plan first")?;
    let key = find_spec(&specs, args["route"].as_str().ok_or("route is required")?)?
        .key
        .clone();
    let spec = specs.iter_mut().find(|s| s.key == key).unwrap();
    let object = |v: &Value| v.as_object().cloned();
    if let Some(body) = args.get("body").filter(|v| !v.is_null()) {
        spec.body = Some(body.clone());
    }
    if let Some(query) = args.get("query").and_then(object) {
        spec.query = Some(query);
    }
    if let Some(headers) = args.get("headers").and_then(object) {
        spec.headers = Some(headers);
    }
    if let Some(values) = args.get("values").and_then(object) {
        for (k, v) in values {
            let v = v
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| v.to_string());
            spec.values.insert(k.clone(), v.clone());
            spec.filled.insert(k.clone(), v);
            spec.unresolved.retain(|u| !u.starts_with(&format!("{k} ")));
        }
        let filled = spec.filled.clone();
        let uri = crate::plan::PARAM.replace_all(&spec.template, |c: &regex::Captures| {
            filled.get(&c[1]).cloned().unwrap_or_default()
        });
        spec.uri = uri.trim_end_matches('/').to_string();
    }
    if let Some(skip) = args["skip"].as_str() {
        spec.skip = if skip.is_empty() {
            None
        } else {
            Some(json!(skip))
        };
    }
    let saved = json!({"route": spec.key, "uri": spec.uri, "body": spec.body, "query": spec.query, "headers": spec.headers,
                       "values": spec.values, "skip": spec.skip, "still_unresolved": spec.unresolved});
    ops::write(&path, &specs)?;
    Ok(format!(
        "Saved. Run it with rerun only=\"{key}\".\n{}",
        pretty(&saved)
    ))
}

fn test_results(config: &Config, args: &Value) -> Res<String> {
    let limit = args["limit"].as_u64().unwrap_or(40) as usize;
    let totals: Value = read(&config.work.join("tests.json"))
        .ok_or("no tests generated yet: run generate_tests")?;
    let detail: Value = read(&config.work.join("tests-detail.json")).unwrap_or(json!({}));
    let take = |v: &Value| {
        v.as_array()
            .map(|a| a.iter().take(limit).cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    Ok(pretty(
        &json!({"totals": totals, "failing": take(&detail["failing"]), "cannot": take(&detail["cannot"]),
                      "details": config.work.join("tests.md")}),
    ))
}

/// Routes with few tested scenarios, and what an AI needs to add more.
fn test_gaps(config: &Config, args: &Value) -> Res<String> {
    let specs: Vec<Spec> =
        read(&config.work.join("requests.json")).ok_or("no plan yet: run plan first")?;
    let old_rows = jsonl(&config.work.join("results-old.jsonl"));
    if old_rows.is_empty() {
        return Err("no results yet: run first".into());
    }
    let outcomes: std::collections::HashMap<String, Outcome> = results_of(config)
        .unwrap_or_default()
        .into_iter()
        .map(|o| (o.id.clone(), o))
        .collect();
    let max_cases = args["max_cases"].as_u64().unwrap_or(2) as usize;
    let limit = args["limit"].as_u64().unwrap_or(10) as usize;
    let only = match args["route"].as_str() {
        Some(r) => Some(find_spec(&specs, r)?.key.clone()),
        None => None,
    };
    let testable = |id: &str| {
        old_rows
            .get(id)
            .and_then(|r| r["status"].as_i64())
            .is_some_and(|s| [200, 201, 202, 204, 400, 401, 403, 404, 409, 422].contains(&s))
    };
    let mut routes: std::collections::BTreeMap<&str, Vec<&Spec>> =
        std::collections::BTreeMap::new();
    for spec in specs
        .iter()
        .filter(|s| s.unresolved.is_empty() && only.as_deref().is_none_or(|o| s.key == o))
    {
        routes.entry(spec.key.as_str()).or_default().push(spec);
    }
    // thinnest first; writes before reads (more ways to go wrong)
    let mut thin: Vec<(&str, Vec<&Spec>, usize)> = routes
        .into_iter()
        .map(|(k, cases)| {
            let n = cases.iter().filter(|s| testable(&s.id)).count();
            (k, cases, n)
        })
        .filter(|(_, _, n)| only.is_some() || *n <= max_cases)
        .collect();
    thin.sort_by_key(|(k, _, n)| (*n, k.starts_with("GET ")));
    let total = thin.len();
    let mut codemap = crate::codemap::Codemap::start(&config.new.repo)?;
    let mut lookup = crate::plan::CodeLookup::new(&mut codemap, &config.new.repo);
    let mut shown = vec![];
    for (key, cases, n) in thin.into_iter().take(limit) {
        let happy = cases
            .iter()
            .find(|s| s.case == "happy")
            .copied()
            .unwrap_or(cases[0]);
        let location = happy
            .action
            .as_deref()
            .and_then(|a| a.split_once('@'))
            .and_then(|(c, m)| {
                lookup.symbol(&format!("{c}::{m}")).map(|s| {
                    format!(
                        "{}:{}-{}",
                        s["file"].as_str().unwrap_or("?"),
                        s["line"],
                        s["end_line"]
                    )
                })
            });
        let rules: Vec<String> = if happy.method != "GET" {
            crate::rules::for_action(&mut lookup, happy.action.as_deref())
                .unwrap_or_default()
                .iter()
                .map(|f| format!("{}: {}", f.name, f.rules.join("|")))
                .collect()
        } else {
            vec![]
        };
        let sent: Vec<Value> = cases
            .iter()
            .map(|s| {
                json!({"case": s.case, "old_status": old_rows.get(&s.id).map(|r| r["status"].clone()),
                       "verdict": outcomes.get(&s.id).map(|o| o.verdict.clone()), "tested": testable(&s.id)})
            })
            .collect();
        shown.push(json!({
            "route": key, "tested_cases": n, "controller": happy.action, "code": location,
            "guard": happy.guard, "happy_body": happy.body, "parameters": happy.filled.keys().collect::<Vec<_>>(),
            "validation_rules": rules, "cases": sent,
        }));
    }
    Ok(pretty(&json!({
        "routes_with_few_cases": total, "shown": shown.len(), "routes": shown,
        "next": "read the controller (code), then add_case for each scenario worth checking; rerun the route; generate_tests",
    })))
}

/// A scenario an AI (or a person) adds to a route; kept by every later plan.
fn add_case(config: &Config, args: &Value) -> Res<String> {
    let path = config.work.join("requests.json");
    let mut specs: Vec<Spec> = read(&path).ok_or("no plan yet: run plan first")?;
    let happy = find_spec(&specs, args["route"].as_str().ok_or("route is required")?)?.clone();
    if !happy.unresolved.is_empty() {
        return Err(format!(
            "{} has no value for {}: give it with set_request values first",
            happy.key,
            happy.unresolved.join(", ")
        ));
    }
    let raw = args["name"].as_str().ok_or("name is required")?;
    let name: String = raw
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if name.is_empty() {
        return Err("name must have letters or digits".into());
    }
    let case_name = if name.starts_with("ai-") {
        name
    } else {
        format!("ai-{name}")
    };
    let mut case = crate::plan::variant(&happy, &case_name);
    case.custom = true;
    if happy.method != "GET" {
        let mut body = happy
            .body
            .clone()
            .and_then(|b| b.as_object().cloned())
            .unwrap_or_default();
        if let Some(extra) = args["body"].as_object() {
            body.extend(extra.clone());
        }
        let without: Vec<String> = args["without"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        for field in &without {
            body.remove(field);
        }
        case.without = without;
        case.body = Some(Value::Object(body));
    }
    if let Some(query) = args["query"].as_object() {
        case.query = Some(query.clone());
    }
    if let Some(values) = args["values"].as_object() {
        for (k, v) in values {
            let v = v
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| v.to_string());
            case.values.insert(k.clone(), v.clone());
            case.filled.insert(k.clone(), v);
        }
        let filled = case.filled.clone();
        let uri = crate::plan::PARAM.replace_all(&case.template, |c: &regex::Captures| {
            filled.get(&c[1]).cloned().unwrap_or_default()
        });
        case.uri = crate::plan::SLASHES
            .replace_all(&uri, "/")
            .trim_end_matches('/')
            .to_string();
    }
    if args["signed_in"].as_bool() == Some(false) {
        case.guard = None;
    }
    case.note = args["note"].as_str().map(|n| json!(n));
    let shown = json!({"route": case.key, "case": case.case, "uri": case.uri, "signed_in": case.guard.is_some(),
                       "body": case.body, "without": case.without, "query": case.query});
    specs.retain(|s| !(s.key == case.key && s.case == case.case));
    specs.push(case.clone());
    ops::write(&path, &specs)?;
    Ok(format!(
        "Added {} [{}]. Next: rerun only=\"{}\" (sends it to both apps; the old app's answer becomes the expected one), then generate_tests.\n{}",
        case.key,
        case.case,
        case.key,
        pretty(&shown)
    ))
}

fn list_pairs() -> String {
    let dir = init::tool_root().join("pairs");
    let mut found = vec![];
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml") {
            match config::load(&path) {
                Ok(c) => {
                    let results: Option<Vec<Outcome>> = read(&c.work.join("results.json"));
                    found.push(
                        json!({"pair": path, "name": c.name, "old": c.old.repo, "new": c.new.repo,
                                      "results": results.map(|r| ops::counts_line(&r))}),
                    );
                }
                Err(e) => found.push(json!({"pair": path, "error": e})),
            }
        }
    }
    pretty(
        &json!({"folder": dir, "pairs": found, "new_pair": "init_pair with the old and new repo paths"}),
    )
}

fn call(name: &str, args: &Value) -> Res<String> {
    match name {
        "guide" => Ok(guide::GUIDE.to_string()),
        "list_pairs" => Ok(list_pairs()),
        "init_pair" => {
            let (path, todo) = init::init(
                Path::new(&config::expand(
                    args["old_repo"].as_str().ok_or("old_repo is required")?,
                )),
                Path::new(&config::expand(
                    args["new_repo"].as_str().ok_or("new_repo is required")?,
                )),
                args["name"].as_str(),
                None,
                args["force"].as_bool().unwrap_or(false),
            )?;
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            Ok(format!(
                "Wrote {}.\nTODO:\n- {}\n\n{text}",
                path.display(),
                if todo.is_empty() {
                    "nothing".into()
                } else {
                    todo.join("\n- ")
                }
            ))
        }
        "check" => ops::check(&load(args)?).map(|v| pretty(&v)),
        "setup_db" => start("setup_db", args, |c| ops::setup_db(&c, false)),
        "plan" => start("plan", args, |c| {
            ops::routes(&c)?;
            ops::plan(&c)
        }),
        "run" => {
            let (workers, resume) = (
                args["workers"].as_u64().map(|w| w as usize),
                args["resume"].as_bool().unwrap_or(false),
            );
            start("run", args, move |c| ops::run(&c, None, resume, workers))
        }
        "rerun" => {
            let only = args["only"].as_str().ok_or("only is required")?.to_string();
            start("rerun", args, move |c| {
                ops::run(&c, Some(&only), false, None)
            })
        }
        "generate_tests" => {
            let (verify, drop) = (
                args["verify"].as_bool().unwrap_or(true),
                args["drop_failing"].as_bool().unwrap_or(false),
            );
            start("generate_tests", args, move |c| {
                ops::tests(&c, verify, drop)
            })
        }
        "status" => Ok(status(args)),
        "code_check" => start("code_check", args, |c| {
            crate::code::check(&c)?;
            let fixes = crate::fixlist::write_all(&c)
                .map(|f| f.iter().filter(|x| x.area == "Code").count())
                .unwrap_or(0);
            Ok(format!(
                "{fixes} code items in the fix list (fix_list area Code)"
            ))
        }),
        "web_check" => start("web_check", args, |c| {
            crate::web::check(&c).map(|v| {
            format!("{} web calls broken by the upgrade, {} fixed by it; details in fix_list (area UI) and web.json",
                v["broken_by_upgrade"].as_array().map_or(0, Vec::len), v["fixed_by_upgrade"])
        })
        }),
        "fix_list" => {
            let config = load(args)?;
            let fixes: Vec<crate::fixlist::Fix> = read(&config.work.join("fixes.json"))
                .ok_or("no fix list yet: run (or report) first")?;
            let limit = args["limit"].as_u64().unwrap_or(20) as usize;
            let chosen: Vec<&crate::fixlist::Fix> = fixes
                .iter()
                .filter(|f| {
                    args["priority"]
                        .as_u64()
                        .is_none_or(|p| f.priority as u64 == p)
                })
                .filter(|f| {
                    args["area"]
                        .as_str()
                        .is_none_or(|a| f.area.eq_ignore_ascii_case(a))
                })
                .collect();
            let shown: Vec<Value> = chosen
                .iter()
                .take(limit)
                .map(|f| {
                    let mut v = serde_json::to_value(f).unwrap();
                    if let Some(r) = v["routes"].as_array().cloned() {
                        v["routes"] = json!(r.into_iter().take(10).collect::<Vec<_>>());
                        v["routes_total"] = json!(f.routes.len());
                    }
                    if let Some(d) = v["details"].as_array().cloned() {
                        v["details"] = json!(d.into_iter().take(10).collect::<Vec<_>>());
                        v["details_total"] = json!(f.details.len());
                    }
                    v
                })
                .collect();
            Ok(pretty(
                &json!({"total": chosen.len(), "shown": shown.len(), "fixes": shown, "file": config.work.join("fixes.md"),
                "next": "show a route for its old and new answer; after a fix, rerun it"}),
            ))
        }
        "schema_check" => start("schema_check", args, |c| {
            crate::schema::check(&c).map(|v| v["summary"].as_str().unwrap_or_default().to_string())
        }),
        "console_check" => start("console_check", args, |c| {
            let v = crate::console::check(&c)?;
            Ok(format!(
                "{} Details in fix_list (area Jobs & commands).",
                v["summary"].as_str().unwrap_or_default()
            ))
        }),
        "channels_check" => start("channels_check", args, |c| {
            let v = crate::channels::check(&c)?;
            Ok(format!(
                "{} Details in fix_list (area Broadcasting).",
                v["summary"].as_str().unwrap_or_default()
            ))
        }),
        "import_log" => crate::logs::import(
            &load(args)?,
            &config::expand(args["log"].as_str().ok_or("log is required")?),
            args["per_route"].as_u64().unwrap_or(5) as usize,
        ),
        "overview" => crate::history::overview(),
        "summary" => summary(&load(args)?),
        "results" => results(&load(args)?, args),
        "show" => show(&load(args)?, args),
        "set_request" => set_request(&load(args)?, args),
        "test_results" => test_results(&load(args)?, args),
        "test_gaps" => test_gaps(&load(args)?, args),
        "add_case" => add_case(&load(args)?, args),
        "report_problem" => {
            let laravel = args["pair"]
                .as_str()
                .and_then(|_| load(args).ok())
                .and_then(|c| read::<Value>(&c.work.join("run.json")))
                .map(|m| ops::laravel_versions(&m))
                .unwrap_or_default();
            let id = crate::reports::add(crate::reports::New {
                kind: args["kind"].as_str().unwrap_or("other"),
                source: "ai",
                step: "mcp",
                route: args["route"].as_str(),
                summary: args["summary"].as_str().ok_or("summary is required")?,
                expected: args["expected"].as_str(),
                actual: args["actual"].as_str(),
                laravel,
            });
            Ok(format!(
                "Recorded as {id}. It stays on this machine; the person can review and send it with `laravel-parity reports --send {id}`."
            ))
        }
        other => Err(format!("unknown tool {other}")),
    }
}

fn answer(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn serve() {
    log::server_mode();
    crate::update::clean_old_copy();
    crate::update::check_in_background();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
        }; // a notification needs no answer
        let params = message.get("params").cloned().unwrap_or(json!({}));
        let reply = match message["method"].as_str().unwrap_or_default() {
            "initialize" => answer(
                &id,
                json!({
                    "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "laravel-parity", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": guide::MCP_INSTRUCTIONS,
                }),
            ),
            "ping" => answer(&id, json!({})),
            "tools/list" => answer(&id, json!({"tools": tools()})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(Value::Object(Map::new()));
                let result = std::panic::catch_unwind(|| call(name, &args)).unwrap_or_else(|_| {
                    crate::reports::add(crate::reports::New {
                        kind: "panic",
                        source: "auto",
                        step: name,
                        route: None,
                        summary: &format!("the MCP tool {name} panicked"),
                        expected: None,
                        actual: None,
                        laravel: Default::default(),
                    });
                    Err("the tool panicked; it was recorded as a problem report".into())
                });
                let (mut text, error) = match result {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                if let Some(newer) = crate::update::known_newer()
                    && !TOLD.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    text += &format!(
                        "\n\n(laravel-parity {newer} is available; this server is {}. Tell the person: run `laravel-parity update`.)",
                        crate::update::VERSION
                    );
                }
                answer(
                    &id,
                    json!({"content": [{"type": "text", "text": text}], "isError": error}),
                )
            }
            other => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {other}")}})
            }
        };
        let _ = writeln!(stdout, "{reply}");
        let _ = stdout.flush();
    }
}
