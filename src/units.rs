//! The app's own classes, method by method: every public method of the models, services, exports, notifications
//! ... in app/ runs on both apps (newest row for a model, constructor and method arguments made the way a unit test
//! would), rolled back with everything leaving the app faked. What differs goes in the fix list; what the old app
//! answered becomes a unit test next to the app's own (tests/Unit/<folder under app>/<Class>Test.php).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::config::{Config, Side};
use crate::fixlist::Fix;
use crate::layout::{self, Kind};
use crate::ops::{Res, read, write};
use crate::plan::request_id;
use crate::testfile;
use crate::testgen::{self, GROUP, MARKER, php_string, php_value};
use crate::{runner, say};

/// Folders under app/ the probe leaves out: controllers have feature tests, providers only boot the app.
const SKIP: [&str; 2] = ["Http/Controllers", "Providers"];
/// Seconds one method may run.
const TIMEOUT: u64 = 30;
/// Methods per PHP process.
const BATCH: usize = 200;

pub fn last(config: &Config) -> Option<Value> {
    read(&config.work.join("units.json"))
}

fn inventory(config: &Config, side: &Side) -> Res<Vec<Value>> {
    let rows = runner::run_job(config, side, &json!({"mode": "units", "skip": SKIP}), 0)?;
    rows.iter()
        .find_map(|r| r["units"].as_array().cloned())
        .ok_or_else(|| format!("the {} app did not list its classes", side.label))
}

/// The methods both apps have, as probe items (a model also gets "@config": its table, keys, fillable, casts).
fn items(old: &[Value], new: &[Value], only: Option<&str>) -> Vec<Value> {
    let old_methods: HashMap<&str, HashSet<&str>> = old
        .iter()
        .map(|u| {
            (
                u["class"].as_str().unwrap_or_default(),
                u["methods"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m["name"].as_str())
                    .collect(),
            )
        })
        .collect();
    let mut out = vec![];
    for unit in new {
        let class = unit["class"].as_str().unwrap_or_default();
        let Some(shared) = old_methods.get(class) else {
            continue;
        };
        if only.is_some_and(|o| !class.contains(o)) {
            continue;
        }
        let model = unit["kind"] == "model";
        let mut add = |method: &str, line: &Value| {
            let key = format!("{class}::{method}");
            out.push(
                json!({"id": request_id(&key), "key": key, "class": class, "method": method,
                            "kind": unit["kind"], "file": unit["file"], "line": line}),
            );
        };
        if model {
            add("@config", &Value::Null);
        }
        for method in unit["methods"].as_array().into_iter().flatten() {
            let name = method["name"].as_str().unwrap_or_default();
            if !shared.contains(name)
                || (model
                    && (name.starts_with("boot")
                        || name.starts_with("initialize")
                        || name == "newFactory"
                        || (name.starts_with("set") && name.ends_with("Attribute"))))
            {
                continue;
            }
            add(name, &method["line"]);
        }
    }
    out
}

/// Probe the items on one app, BATCH at a time per process, `workers` processes; a process that stops (a method
/// that printed straight out, a timeout, a crash) is started again for the rest.
pub fn run_side(config: &Config, side: &Side, items: &[Value], clock: &str) -> Vec<Value> {
    let batches: Vec<Vec<Value>> = items.chunks(BATCH).map(|c| c.to_vec()).collect();
    let queue: Mutex<VecDeque<Vec<Value>>> = Mutex::new(batches.into());
    let found: Mutex<Vec<Value>> = Mutex::new(vec![]);
    let total = items.len();
    std::thread::scope(|scope| {
        for slot in 0..config.workers.max(1) {
            let (queue, found) = (&queue, &found);
            scope.spawn(move || loop {
                let Some(mut batch) = queue.lock().unwrap().pop_front() else {
                    return;
                };
                while !batch.is_empty() {
                    let probe: Vec<Value> = batch
                        .iter()
                        .map(|i| {
                            let mut item = json!({"id": i["id"], "class": i["class"], "method": i["method"]});
                            if i.get("code").is_some() {
                                item["code"] = i["code"].clone();
                            }
                            item
                        })
                        .collect();
                    let job = json!({"mode": "probe", "clock": clock, "timeout": TIMEOUT, "users": config.users,
                                     "items": probe});
                    let limit = TIMEOUT * batch.len() as u64 + 60;
                    let rows = runner::run_job_timed(config, side, &job, 200 + slot, Some(limit))
                        .unwrap_or_else(|e| vec![json!({"runner_crash": e})]);
                    let answered: HashMap<String, Value> = rows
                        .iter()
                        .filter_map(|r| r["id"].as_str().map(|id| (id.to_string(), r.clone())))
                        .collect();
                    let stopped = rows.iter().find_map(|r| {
                        r["runner_crash"]
                            .as_str()
                            .map(String::from)
                            .or_else(|| r.get("runner_timeout").map(|_| "timeout".to_string()))
                    });
                    let mut rest = vec![];
                    let mut first_missing = true;
                    for item in batch {
                        let id = item["id"].as_str().unwrap_or_default().to_string();
                        if let Some(row) = answered.get(&id) {
                            found.lock().unwrap().push(row.clone());
                        } else if first_missing && stopped.is_some() {
                            // the method the process died on
                            first_missing = false;
                            let why = stopped.clone().unwrap_or_default();
                            found.lock().unwrap().push(json!({"id": id, "status": "runner-error",
                                "error": format!("PHP stopped: {}", why.chars().take(300).collect::<String>())}));
                        } else {
                            rest.push(item);
                        }
                    }
                    batch = rest;
                    let done = found.lock().unwrap().len();
                    say(&format!("  {}: {done}/{total}", side.label));
                }
            });
        }
    });
    found.into_inner().unwrap()
}

fn ok(row: &Value) -> bool {
    row["status"] == "ok"
}

fn failed(row: &Value) -> bool {
    matches!(
        row["status"].as_str(),
        Some("threw" | "timeout" | "runner-error")
    )
}

/// What a method did besides answering, without the log lines (deprecation warnings differ by PHP version).
fn effects(row: &Value) -> Value {
    let mut e = row["effects"].clone();
    if let Some(o) = e.as_object_mut() {
        o.remove("logs");
    }
    e
}

/// How the new app's answer compares with the old one's.
pub fn verdict(item: &Value, old: &Value, new: &Value) -> &'static str {
    if old["status"] == "not-run" || new["status"] == "not-run" || old.is_null() || new.is_null() {
        return "not-run";
    }
    match (ok(old), ok(new)) {
        (true, true) => {
            if item["method"] == "@config" {
                return if old["value"] == new["value"] {
                    "same"
                } else {
                    "config"
                };
            }
            if old["type"] != new["type"] {
                "type"
            } else if old["value"] != new["value"]
                || old["count"] != new["count"]
                || old["related"] != new["related"]
            {
                "value"
            } else if effects(old) != effects(new) {
                "effects"
            } else {
                "same"
            }
        }
        (true, false) if failed(new) => "new-broken",
        (false, true) if failed(old) => "new-fixed",
        _ => "both-fail",
    }
}

/// Probe every shared method on both apps and compare; writes runs/<name>/units.json.
pub fn check(config: &Config, only: Option<&str>) -> Res<String> {
    runner::ensure_ready(&config.old)?;
    runner::ensure_ready(&config.new)?;
    say("Listing both apps' classes and methods ...");
    let (old, new) = (
        inventory(config, &config.old)?,
        inventory(config, &config.new)?,
    );
    let items = items(&old, &new, only);
    let clock = if config.clock == "auto" {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        config.clock.clone()
    };
    say(&format!(
        "Running {} methods of {} classes on both apps ...",
        items.len(),
        items
            .iter()
            .map(|i| i["class"].as_str().unwrap_or_default())
            .collect::<BTreeSet<_>>()
            .len()
    ));
    let (old_rows, new_rows) = std::thread::scope(|scope| {
        let a = scope.spawn(|| run_side(config, &config.old, &items, &clock));
        let b = scope.spawn(|| run_side(config, &config.new, &items, &clock));
        (a.join().unwrap(), b.join().unwrap())
    });
    let by_id = |rows: Vec<Value>| -> HashMap<String, Value> {
        rows.into_iter()
            .filter_map(|r| r["id"].as_str().map(|id| (id.to_string(), r.clone())))
            .collect()
    };
    let (old_rows, new_rows) = (by_id(old_rows), by_id(new_rows));
    let mut results = vec![];
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for item in &items {
        let id = item["id"].as_str().unwrap_or_default();
        let (o, n) = (
            old_rows.get(id).cloned().unwrap_or(Value::Null),
            new_rows.get(id).cloned().unwrap_or(Value::Null),
        );
        let verdict = verdict(item, &o, &n);
        *counts.entry(verdict).or_default() += 1;
        let mut row = item.clone();
        row["verdict"] = json!(verdict);
        row["old"] = o;
        row["new"] = n;
        results.push(row);
    }
    let mut previous = last(config).unwrap_or_default();
    if only.is_some() {
        // keep the other classes' results
        let mut kept: Vec<Value> = previous["results"]
            .as_array_mut()
            .map(std::mem::take)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !results.iter().any(|n| n["id"] == r["id"]))
            .collect();
        kept.extend(results);
        results = kept;
    }
    let mut all_counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in &results {
        *all_counts
            .entry(r["verdict"].as_str().unwrap_or("?").to_string())
            .or_default() += 1;
    }
    write(
        &config.work.join("units.json"),
        &json!({"clock": clock, "counts": all_counts, "results": results}),
    )?;
    let line = counts
        .iter()
        .map(|(k, v)| format!("{k} {v}"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "Methods: {line}. Details: {}",
        config.work.join("units.json").display()
    ))
}

/// Run a few lines of PHP on both apps (rolled back, faked) and return what each gave back.
pub fn probe_code(config: &Config, code: &str) -> Res<Value> {
    runner::ensure_ready(&config.old)?;
    runner::ensure_ready(&config.new)?;
    let clock = if config.clock == "auto" {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        config.clock.clone()
    };
    let job = json!({"mode": "probe", "clock": clock, "timeout": TIMEOUT, "users": config.users,
                     "items": [{"id": "code", "class": "", "method": "", "code": code}]});
    let answer = |side: &Side| -> Value {
        match runner::run_job_timed(config, side, &job, 300, Some(TIMEOUT + 30)) {
            Ok(rows) => rows
                .into_iter()
                .find(|r| r["id"] == "code")
                .map(|mut r| {
                    if let Some(o) = r.as_object_mut() {
                        o.remove("id");
                    }
                    r
                })
                .unwrap_or_else(|| json!({"status": "runner-error", "error": "no answer"})),
            Err(e) => json!({"status": "runner-error", "error": e}),
        }
    };
    let (old, new) = std::thread::scope(|scope| {
        let a = scope.spawn(|| answer(&config.old));
        let b = scope.spawn(|| answer(&config.new));
        (a.join().unwrap(), b.join().unwrap())
    });
    let same = old["status"] == new["status"]
        && old["type"] == new["type"]
        && old["value"] == new["value"];
    Ok(json!({"same": same, "old": old, "new": new}))
}

fn short(error: &str) -> String {
    error.chars().take(160).collect()
}

pub fn fixes(config: &Config) -> Vec<Fix> {
    let Some(found) = last(config) else {
        return vec![];
    };
    let results = found["results"].as_array().cloned().unwrap_or_default();
    let mut out = vec![];
    let pick = |verdict: &str| -> Vec<&Value> {
        results.iter().filter(|r| r["verdict"] == verdict).collect()
    };
    let place = |r: &Value| {
        format!(
            "{}{}",
            r["file"].as_str().unwrap_or_default(),
            r["line"]
                .as_u64()
                .map(|l| format!(":{l}"))
                .unwrap_or_default()
        )
    };
    // new-broken grouped by what the new app throws
    let mut broken: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for r in pick("new-broken") {
        let error = r["new"]["error"].as_str().unwrap_or("stopped");
        let cause = error.split(':').next().unwrap_or(error).to_string();
        let at = r["new"]["file"].as_str().unwrap_or_default().to_string();
        broken.entry(format!("{cause}|{at}")).or_default().push(r);
    }
    for (_, rows) in broken {
        let first = rows[0];
        out.push(Fix {
            priority: 1,
            area: "Code".into(),
            title: format!(
                "{} method{} that work on the old app throw on the new one: {}",
                rows.len(),
                if rows.len() == 1 { "" } else { "s" },
                short(first["new"]["error"].as_str().unwrap_or("stopped"))
            ),
            location: first["new"]["file"]
                .as_str()
                .map(String::from)
                .or_else(|| Some(place(first))),
            hint: None,
            routes: vec![],
            details: rows
                .iter()
                .map(|r| format!("{} ({})", r["key"].as_str().unwrap_or_default(), place(r)))
                .collect(),
        });
    }
    let mut add = |priority: u8, verdict: &str, title: &str, hint: &str| {
        let rows = pick(verdict);
        if rows.is_empty() {
            return;
        }
        out.push(Fix {
            priority,
            area: "Code".into(),
            title: format!("{} {title}", rows.len()),
            location: None,
            hint: Some(hint.into()),
            routes: vec![],
            details: rows
                .iter()
                .map(|r| {
                    let what = match verdict {
                        "config" => config_diff(&r["old"]["value"], &r["new"]["value"]),
                        "type" => format!(
                            "{} -> {}",
                            r["old"]["type"].as_str().unwrap_or("?"),
                            r["new"]["type"].as_str().unwrap_or("?")
                        ),
                        _ => String::new(),
                    };
                    format!(
                        "{} ({}){}",
                        r["key"].as_str().unwrap_or_default(),
                        place(r),
                        if what.is_empty() {
                            what
                        } else {
                            format!(": {what}")
                        }
                    )
                })
                .collect(),
        });
    };
    add(
        2,
        "config",
        "models whose settings differ (table, key, fillable, hidden, casts)",
        "Laravel Shift moves $dates into $casts and may rewrite casts; a date field missing from the new casts comes back as a string.",
    );
    add(
        2,
        "type",
        "methods return a different type on the new app",
        "Compare the method on both apps with `laravel-parity units <pair> --only <Class>`; a collection becoming an array (or the other way) breaks callers.",
    );
    add(
        2,
        "effects",
        "methods write to the database differently on the new app",
        "The same call inserts, updates or deletes different rows: check the method's queries.",
    );
    add(
        3,
        "value",
        "methods return a different value on the new app",
        "Often dates, number formats or ordering; units.json has both answers.",
    );
    out
}

fn config_diff(old: &Value, new: &Value) -> String {
    let mut parts = vec![];
    for key in ["table", "key", "fillable", "hidden"] {
        if old[key] != new[key] {
            parts.push(format!("{key} {} -> {}", old[key], new[key]));
        }
    }
    let (a, b) = (
        old["casts"].as_object().cloned().unwrap_or_default(),
        new["casts"].as_object().cloned().unwrap_or_default(),
    );
    for (field, cast) in &a {
        match b.get(field) {
            None => parts.push(format!("cast {field} ({cast}) is gone")),
            Some(c) if c != cast => parts.push(format!("cast {field} {cast} -> {c}")),
            _ => {}
        }
    }
    for (field, cast) in &b {
        if !a.contains_key(field) {
            parts.push(format!("new cast {field} ({cast})"));
        }
    }
    parts.join("; ")
}

/// Unit test files written, and the (class, method) of each test in them with its "Class::method" key.
pub struct Written {
    pub files: Vec<PathBuf>,
    pub made: Vec<testgen::Made>,
    pub cannot: Vec<(String, String)>,
}

/// A PHP expression for one argument, made the way the probe made it.
fn argument(how: &Value) -> String {
    let class = |h: &Value| format!("\\{}", h["class"].as_str().unwrap_or_default());
    match how["how"].as_str().unwrap_or_default() {
        "given" | "default" => php_value(&how["value"]),
        "model" => format!("$this->generatedRow({}::class)", class(how)),
        "request" => format!("{}::create('/', 'GET')", class(how)),
        "now" => "\\Illuminate\\Support\\Carbon::now()".into(),
        "make" => format!("app({}::class)", class(how)),
        _ => "null".into(),
    }
}

fn arguments(list: &Value) -> String {
    list.as_array()
        .into_iter()
        .flatten()
        .map(argument)
        .collect::<Vec<_>>()
        .join(", ")
}

fn test_name(method: &str, taken: &mut HashSet<String>) -> String {
    let mut snake = String::new();
    for (i, c) in method.trim_start_matches('@').chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            snake.push('_');
        }
        if c.is_alphanumeric() || c == '_' {
            snake.push(c.to_ascii_lowercase());
        }
    }
    let base = format!("test_{snake}");
    let mut name = base.clone();
    let mut n = 2;
    while !taken.insert(name.clone()) {
        name = format!("{base}_{n}");
        n += 1;
    }
    name
}

/// What the test checks about the value the old app gave back.
fn expect(old: &Value, var: &str) -> Vec<String> {
    let kind = old["type"].as_str().unwrap_or("NULL");
    let mut lines = vec![];
    if kind == "NULL" {
        lines.push(format!("        $this->addToAssertionCount(1); // the old app returned null: it must not throw (uses {var})"));
        return lines;
    }
    lines.push(format!(
        "        $this->assertGeneratedType({}, {var});",
        php_string(kind)
    ));
    if let Some(related) = old["related"].as_str() {
        lines.push(format!(
            "        $this->assertInstanceOf(\\{related}::class, {var}->getRelated());"
        ));
    }
    if kind == "array" {
        for key in old["keys"].as_array().into_iter().flatten().take(20) {
            if let Some(k) = key.as_str() {
                lines.push(format!(
                    "        $this->assertArrayHasKey({}, {var});",
                    php_string(k)
                ));
            }
        }
    }
    lines
}

/// The body of one test from the old app's answer; None when the old app gave nothing to test against.
fn test_body(row: &Value) -> Option<Vec<String>> {
    let old = &row["old"];
    if !ok(old) {
        return None;
    }
    let class = format!("\\{}", row["class"].as_str().unwrap_or_default());
    let method = row["method"].as_str().unwrap_or_default();
    let model = row["kind"] == "model";
    let mut lines = vec![];
    if method == "@config" {
        let v = &old["value"];
        lines.push(format!("        $model = new {class}();"));
        lines.push(format!(
            "        $this->assertSame({}, $model->getTable());",
            php_value(&v["table"])
        ));
        lines.push(format!(
            "        $this->assertSame({}, $model->getKeyName());",
            php_value(&v["key"])
        ));
        lines.push("        $fillable = $model->getFillable();".into());
        lines.push("        sort($fillable);".into());
        lines.push(format!(
            "        $this->assertSame({}, $fillable);",
            php_value(&v["fillable"])
        ));
        lines.push("        $hidden = $model->getHidden();".into());
        lines.push("        sort($hidden);".into());
        lines.push(format!(
            "        $this->assertSame({}, $hidden);",
            php_value(&v["hidden"])
        ));
        lines.push(
            "        // dates the old app kept in $dates count as datetime casts (Laravel Shift moves them)".into(),
        );
        lines.push("        $casts = $model->getCasts();".into());
        lines.push("        foreach (method_exists($model, 'getDates') ? $model->getDates() : [] as $date) {".into());
        lines.push("            $casts[$date] = $casts[$date] ?? 'datetime';".into());
        lines.push("        }".into());
        lines.push("        ksort($casts);".into());
        lines.push(format!(
            "        $this->assertSame({}, $casts);",
            php_value(&v["casts"])
        ));
        return Some(lines);
    }
    let static_call = old["static"] == true;
    if model && method.starts_with("scope") && method.len() > 5 {
        let scope = format!("{}{}", method[5..6].to_lowercase(), &method[6..]);
        lines.push(format!(
            "        $query = {class}::query()->{scope}({});",
            arguments(&old["args"])
        ));
        lines.push(
            "        $this->assertInstanceOf(\\Illuminate\\Database\\Eloquent\\Builder::class, $query);".into(),
        );
        return Some(lines);
    }
    if let Some(attribute) = old["attribute"].as_str() {
        lines.push(format!(
            "        $value = $this->generatedRow({class}::class)->getAttribute({});",
            php_string(attribute)
        ));
        lines.extend(expect(old, "$value"));
        return Some(lines);
    }
    let subject = if model {
        if old["related"].is_string() {
            // a relation needs no row
            format!("(new {class}())")
        } else {
            lines.push(format!(
                "        $subject = $this->generatedRow({class}::class);"
            ));
            "$subject".into()
        }
    } else if static_call {
        String::new()
    } else {
        lines.push(format!(
            "        $subject = new {class}({});",
            arguments(&old["construct"])
        ));
        "$subject".into()
    };
    let call = if subject.is_empty() {
        format!("{class}::{method}({})", arguments(&old["args"]))
    } else {
        format!("{subject}->{method}({})", arguments(&old["args"]))
    };
    lines.push(format!("        $result = {call};"));
    lines.extend(expect(old, "$result"));
    Some(lines)
}

/// The methods of a class's generated unit tests: the tests, then the helpers they call.
fn members(config: &Config, tests: &[(String, Vec<String>)]) -> Vec<String> {
    let attribute = testgen::phpunit_attributes(&config.new.repo);
    let void = testgen::phpunit_major(&config.new.repo).is_none_or(|m| m >= 8);
    let ret = if void { ": void" } else { "" };
    let mut out = vec![];
    for (name, body) in tests {
        let mut lines = vec![];
        if attribute {
            lines.push(format!(
                "#[\\PHPUnit\\Framework\\Attributes\\Group('{GROUP}')]"
            ));
        } else {
            lines.push(format!("/** @group {GROUP} */"));
        }
        lines.push(format!("public function {name}(){ret}"));
        lines.push("{".into());
        lines.push("    $this->fakeOutsideWorld();".into());
        // the bodies are written for a class body (8 spaces); the block indents them again
        lines.extend(
            body.iter()
                .map(|l| l.strip_prefix("    ").unwrap_or(l).to_string()),
        );
        lines.push("}".into());
        out.push(lines.join("\n"));
    }
    out.push(format!(
        "/** Mail, notifications, queued jobs and HTTP calls are faked: a generated test sends nothing. */
private function fakeOutsideWorld(){ret}
{{
    \\Illuminate\\Support\\Facades\\Mail::fake();
    \\Illuminate\\Support\\Facades\\Notification::fake();
    \\Illuminate\\Support\\Facades\\Queue::fake();
    if (class_exists(\\Illuminate\\Support\\Facades\\Http::class)) {{
        \\Illuminate\\Support\\Facades\\Http::fake();
    }}
}}"
    ));
    out.push(
        "/** A row of the model: one already in the test database, else one from its factory, else a new one. */
private function generatedRow(string $class)
{
    $row = $class::query()->first();
    if ($row === null && method_exists($class, 'factory')) {
        try {
            $row = $class::factory()->create();
        } catch (\\Throwable $e) {
            // HasFactory without a factory class, or a factory the test data cannot satisfy
        }
    }

    return $row ?? new $class();
}"
        .to_string(),
    );
    out.push(format!(
        "/** The old app's type; null passes (the test data may lack what the old app's data had). */
private function assertGeneratedType(string $expected, $value){ret}
{{
    if ($value === null) {{
        $this->addToAssertionCount(1);
    }} elseif (class_exists($expected) || interface_exists($expected)) {{
        $this->assertInstanceOf($expected, $value);
    }} elseif (in_array($expected, ['integer', 'double'], true)) {{
        $this->assertIsNumeric($value);
    }} else {{
        $this->assertSame($expected, gettype($value));
    }}
}}"
    ));
    out
}

fn unit_doc(class: &str) -> Vec<String> {
    vec![
        "/**".to_string(),
        format!(" * {MARKER}: what {class}'s methods gave back on the old app (type, keys, related class; a model's"),
        " * table, keys, fillable, hidden and casts exactly). Regenerating replaces this file; once you edit it, remove"
            .into(),
        " * this marker so the tool leaves it alone (it then adds its tests in a block at the end).".into(),
        " */".into(),
    ]
}

/// Write the unit tests from the last probe. `skip` holds (test class, test method) to leave out (failing ones,
/// with --drop-failing).
pub fn generate(config: &Config, skip: &HashSet<(String, String)>) -> Written {
    let mut written = Written {
        files: vec![],
        made: vec![],
        cannot: vec![],
    };
    let Some(found) = last(config) else {
        return written;
    };
    let mut by_class: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in found["results"].as_array().into_iter().flatten() {
        by_class
            .entry(row["class"].as_str().unwrap_or_default().to_string())
            .or_default()
            .push(row.clone());
    }
    for (class, rows) in by_class {
        let place = layout::place(config, Kind::Unit, &class);
        let existing = std::fs::read_to_string(&place.file).ok();
        // a test file the app already has: the generated tests go in a block at the end of its class
        let test_class = match existing.as_deref() {
            Some(text) if !testfile::owned(text) => {
                if let Err(why) = testfile::usable(text) {
                    // counted per method, as every other method that gets no test
                    let why = format!("{}.php {why}", place.class);
                    written.cannot.extend(rows.iter().map(|r| {
                        (
                            format!("{class}::{}", r["method"].as_str().unwrap_or_default()),
                            why.clone(),
                        )
                    }));
                    continue;
                }
                testfile::class_of(text).unwrap_or_else(|| place.fqcn())
            }
            _ => place.fqcn(),
        };
        let mut taken = testgen::taken_names(existing.as_deref());
        // the helpers the generated tests call
        taken.extend(["fakeOutsideWorld", "generatedRow", "assertGeneratedType"].map(String::from));
        let mut tests = vec![];
        for row in rows {
            let method = row["method"].as_str().unwrap_or_default();
            let key = format!("{class}::{method}");
            let Some(body) = test_body(&row) else {
                let why = row["old"]["error"]
                    .as_str()
                    .map(|e| format!("the old app: {}", short(e)))
                    .unwrap_or_else(|| "the old app did not run it".into());
                written.cannot.push((key, why));
                continue;
            };
            let name = test_name(method, &mut taken);
            if skip.contains(&(test_class.clone(), name.clone())) {
                continue;
            }
            written.made.push(testgen::Made {
                class: test_class.clone(),
                method: name.clone(),
                key,
            });
            tests.push((name, body));
        }
        if tests.is_empty() {
            testfile::clear(&place.file);
            continue;
        }
        if let Some(parent) = place.file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let base = config
            .tests
            .unit_base_class
            .as_deref()
            .unwrap_or("Tests\\TestCase")
            .trim_start_matches('\\');
        let text = testfile::render(
            existing.as_deref(),
            &place,
            &unit_doc(&class),
            base,
            &members(config, &tests),
        );
        let _ = std::fs::write(&place.file, text);
        written.files.push(place.file);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_methods_become_items() {
        let old = vec![
            json!({"class": "App\\Models\\Unit", "kind": "model", "file": "app/Models/Unit.php",
                   "methods": [{"name": "owner", "line": 10}, {"name": "gone", "line": 20}, {"name": "bootSoft"}]}),
            json!({"class": "App\\Services\\Rent", "kind": "class", "file": "app/Services/Rent.php",
                   "methods": [{"name": "total", "line": 5}]}),
        ];
        let new = vec![
            json!({"class": "App\\Models\\Unit", "kind": "model", "file": "app/Models/Unit.php",
                   "methods": [{"name": "owner", "line": 12}, {"name": "bootSoft"}, {"name": "setNameAttribute"},
                               {"name": "added"}]}),
            json!({"class": "App\\Services\\Rent", "kind": "class", "file": "app/Services/Rent.php",
                   "methods": [{"name": "total", "line": 5}]}),
            json!({"class": "App\\Services\\New", "kind": "class", "methods": [{"name": "x"}]}),
        ];
        let all = items(&old, &new, None);
        let keys: Vec<&str> = all.iter().map(|i| i["key"].as_str().unwrap()).collect();
        assert_eq!(
            keys,
            [
                "App\\Models\\Unit::@config",
                "App\\Models\\Unit::owner",
                "App\\Services\\Rent::total"
            ]
        );
        assert_eq!(all[1]["line"], 12);
        assert_eq!(all[1]["id"], request_id("App\\Models\\Unit::owner"));
        assert_eq!(items(&old, &new, Some("Rent")).len(), 1);
    }

    #[test]
    fn verdicts() {
        let item = json!({"method": "total"});
        let ok = |v: Value| json!({"status": "ok", "type": "integer", "value": v, "effects": {}});
        assert_eq!(verdict(&item, &ok(json!(1)), &ok(json!(1))), "same");
        assert_eq!(verdict(&item, &ok(json!(1)), &ok(json!(2))), "value");
        let mut float = ok(json!(1.0));
        float["type"] = json!("double");
        assert_eq!(verdict(&item, &ok(json!(1)), &float), "type");
        let threw = json!({"status": "threw", "error": "TypeError: x"});
        assert_eq!(verdict(&item, &ok(json!(1)), &threw), "new-broken");
        assert_eq!(verdict(&item, &threw, &ok(json!(1))), "new-fixed");
        assert_eq!(verdict(&item, &threw, &threw), "both-fail");
        assert_eq!(
            verdict(&item, &json!({"status": "not-run"}), &ok(json!(1))),
            "not-run"
        );
        assert_eq!(verdict(&item, &Value::Null, &ok(json!(1))), "not-run");
        let mut wrote = ok(json!(1));
        wrote["effects"] = json!({"writes": ["insert into units"]});
        assert_eq!(verdict(&item, &ok(json!(1)), &wrote), "effects");
        let mut warned = ok(json!(1));
        warned["effects"] = json!({"logs": ["warning: deprecated"]});
        assert_eq!(verdict(&item, &warned, &ok(json!(1))), "same");
        let config = json!({"method": "@config"});
        let a =
            json!({"status": "ok", "type": "config", "value": {"casts": {"paid_at": "datetime"}}});
        let b = json!({"status": "ok", "type": "config", "value": {"casts": {}}});
        assert_eq!(verdict(&config, &a, &a), "same");
        assert_eq!(verdict(&config, &a, &b), "config");
        assert_eq!(
            config_diff(&a["value"], &b["value"]),
            "cast paid_at (\"datetime\") is gone"
        );
    }

    #[test]
    fn test_bodies() {
        let row = |method: &str, kind: &str, old: Value| json!({"class": "App\\Models\\Unit", "method": method, "kind": kind, "old": old, "new": {}});
        let config = test_body(&row("@config", "model", json!({"status": "ok", "type": "config",
            "value": {"table": "units", "key": "id", "fillable": ["name"], "hidden": [], "casts": {"id": "int"}}})))
        .unwrap()
        .join("\n");
        assert!(config.contains("$this->assertSame('units', $model->getTable());"));
        assert!(config.contains("$this->assertSame(['name'], $fillable);"));
        assert!(config.contains("$this->assertSame(['id' => 'int'], $casts);"));

        let relation = test_body(&row("owner", "model", json!({"status": "ok",
            "type": "Illuminate\\Database\\Eloquent\\Relations\\BelongsTo", "related": "App\\Models\\User", "args": []})))
        .unwrap();
        assert_eq!(
            relation,
            [
                "        $result = (new \\App\\Models\\Unit())->owner();",
                "        $this->assertGeneratedType('Illuminate\\\\Database\\\\Eloquent\\\\Relations\\\\BelongsTo', $result);",
                "        $this->assertInstanceOf(\\App\\Models\\User::class, $result->getRelated());"
            ]
        );
        let scope = test_body(&row(
            "scopeActive",
            "model",
            json!({"status": "ok", "type": "x",
            "args": [{"how": "default", "value": 1}]}),
        ))
        .unwrap();
        assert_eq!(
            scope[0],
            "        $query = \\App\\Models\\Unit::query()->active(1);"
        );
        let accessor = test_body(&row(
            "getFullNameAttribute",
            "model",
            json!({"status": "ok", "type": "string",
            "attribute": "full_name"}),
        ))
        .unwrap();
        assert_eq!(
            accessor[0],
            "        $value = $this->generatedRow(\\App\\Models\\Unit::class)->getAttribute('full_name');"
        );
        assert!(test_body(&row("x", "model", json!({"status": "threw"}))).is_none());

        let service = json!({"class": "App\\Services\\Rent", "method": "summary", "kind": "class", "new": {},
            "old": {"status": "ok", "type": "array", "keys": ["total", "due"],
                    "construct": [{"how": "make", "class": "App\\Services\\Clock"}],
                    "args": [{"how": "model", "class": "App\\Models\\Unit"}, {"how": "now"}, {"how": "null"},
                             {"how": "request", "class": "Illuminate\\Http\\Request"}]}});
        assert_eq!(
            test_body(&service).unwrap(),
            [
                "        $subject = new \\App\\Services\\Rent(app(\\App\\Services\\Clock::class));",
                "        $result = $subject->summary($this->generatedRow(\\App\\Models\\Unit::class), \\Illuminate\\Support\\Carbon::now(), null, \\Illuminate\\Http\\Request::create('/', 'GET'));",
                "        $this->assertGeneratedType('array', $result);",
                "        $this->assertArrayHasKey('total', $result);",
                "        $this->assertArrayHasKey('due', $result);"
            ]
        );
        let helper = json!({"class": "App\\Helpers\\Money", "method": "format", "kind": "class", "new": {},
            "old": {"status": "ok", "type": "NULL", "static": true, "args": [{"how": "given", "value": "1.50"}]}});
        assert_eq!(
            test_body(&helper).unwrap()[0],
            "        $result = \\App\\Helpers\\Money::format('1.50');"
        );

        let mut taken = HashSet::new();
        assert_eq!(
            test_name("getFullNameAttribute", &mut taken),
            "test_get_full_name_attribute"
        );
        assert_eq!(test_name("@config", &mut taken), "test_config");
        assert_eq!(test_name("config", &mut taken), "test_config_2");
    }
}
