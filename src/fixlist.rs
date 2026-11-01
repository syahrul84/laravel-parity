//! The fix list: the run's findings turned into things to fix, worst first, grouped by cause, with where to look
//! (the file and line in the new code, or in the web app) and what usually causes it. Written as fixes.md (a
//! checklist for people) and fixes.json (for the MCP `fix_list` tool).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::compare::Outcome;
use crate::config::Config;
use crate::ops::{Res, jsonl, read, write};
use crate::plan::Spec;
use crate::routes::RouteDiff;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fix {
    /// 1 = fix before release ... 4 = know about it
    pub priority: u8,
    /// API, Access, Validation, UI, Code, Database, Jobs & commands, Broadcasting, Response, Side effects, Speed,
    /// Not checked
    pub area: String,
    pub title: String,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub hint: Option<String>,
    pub routes: Vec<String>,
    #[serde(default)]
    pub details: Vec<String>,
}

/// What an error message usually means after an upgrade, and the usual fix.
fn hint(message: &str) -> Option<&'static str> {
    static HINTS: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
        [
            (r"Argument #\d+ \(\$\w+\) must be of type (int|float|bool|string|array), \w+ given",
             "A type added during the upgrade (often by Laravel Shift) is stricter than the data: the route or caller passes another type. Widen or remove the type, or cast the value before the call."),
            (r"Return value must be of type .+, (none|null|\w+) returned",
             "A return type added during the upgrade does not match what the method returns on this path. Return the declared type on every path, or widen the type."),
            (r"json_decode\(\): Argument #1 \(\$json\) must be of type string, array given",
             "The attribute is already cast to an array (model $casts) in the new code, so it must not be json_decode'd again."),
            (r"Unknown column '([^']+)'",
             "The query uses a column the upgraded database does not have: add it in a migration, or change the query."),
            (r"Base table or view not found",
             "A table is missing on the upgraded database: a migration was not converted or not run."),
            (r"Class \x22?([\w\\]+)\x22? not found",
             "A class was renamed, moved or removed (framework or package upgrade). Update the import or the package."),
            (r"Call to undefined (method|function)",
             "A method or helper was removed or renamed in the new Laravel / PHP / package version; use its replacement."),
            (r"filename and the fallback cannot contain",
             "Symfony 7 refuses '/' and '\\' in download file names: sanitise the name before Content-Disposition."),
            (r"(Carbon|InvalidFormatException|diffIn)",
             "Carbon 3 (Laravel 11+): stricter parsing of strings, and diffIn*() now returns signed floats. Parse explicitly, use abs()/intval() where the old code relied on positive integers."),
            (r"Passing null to parameter .+ is deprecated|Deprecated",
             "A PHP 8.1+ deprecation turned into an error: give the function a string/array instead of null."),
            (r"Too many attempts|429",
             "A rate limiter applies to this route now; check the limiter's key and limit."),
            (r"Unauthenticated|401",
             "The route now needs another guard or token: compare its auth middleware between old and new."),
            (r"^Error \w+ \w+\.?$|^(Error|Failed)[\w ]*\.?$",
             "The controller catches the exception and answers only this text, so the real cause is hidden: open the action's try/catch and log or rethrow the exception (then rerun this route)."),
            (r"must not be accessed before initialization",
             "A typed property added during the upgrade has no default: give it one or set it in the constructor."),
            (r"Cannot assign .+ to property .+ of type",
             "A property type added during the upgrade is stricter than the values assigned to it."),
        ]
        .into_iter()
        .map(|(p, h)| (Regex::new(p).unwrap(), h))
        .collect()
    });
    HINTS
        .iter()
        .find(|(p, _)| p.is_match(message))
        .map(|(_, h)| *h)
}

/// Where the app's own code threw: the first frame in app/ (not vendor/), relative to the repo.
fn location(row: &Value, app_path: &str) -> Option<String> {
    let json = &row["json"];
    let frames = json["trace"].as_array().cloned().unwrap_or_default();
    let mut candidates: Vec<(String, Value)> = vec![(
        json["file"].as_str().unwrap_or("").to_string(),
        json["line"].clone(),
    )];
    candidates.extend(frames.iter().map(|f| {
        (
            f["file"].as_str().unwrap_or("").to_string(),
            f["line"].clone(),
        )
    }));
    let prefix = format!("{}/", app_path.trim_end_matches('/'));
    candidates
        .into_iter()
        .find(|(file, _)| !file.is_empty() && !file.contains("/vendor/"))
        .map(|(file, line)| format!("{}:{}", file.strip_prefix(&prefix).unwrap_or(&file), line))
}

fn message(row: &Value) -> String {
    let json = &row["json"];
    let text = json["message"]
        .as_str()
        .or(row["error"].as_str())
        .unwrap_or("")
        .to_string();
    match json["exception"].as_str() {
        Some(e) => format!("{}: {text}", e.rsplit('\\').next().unwrap_or(e)),
        None => text,
    }
}

/// The message with ids, numbers and quoted values blurred, so one cause on many routes is one group.
fn cause(text: &str) -> String {
    static VALUES: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\(SQL: .*|\d+|'[^']*' given").unwrap());
    VALUES.replace_all(text, "…").chars().take(220).collect()
}

fn ok(status: &Value) -> bool {
    status.as_i64().is_some_and(|s| (200..300).contains(&s))
}

fn denied(status: &Value) -> bool {
    matches!(status.as_i64(), Some(401 | 403))
}

pub fn build(config: &Config) -> Res<Vec<Fix>> {
    let results: Vec<Outcome> =
        read(&config.work.join("results.json")).ok_or("no results yet: run first")?;
    let specs: Vec<Spec> = read(&config.work.join("requests.json")).unwrap_or_default();
    let spec_of: HashMap<&str, &Spec> = specs.iter().map(|s| (s.id.as_str(), s)).collect();
    let new_rows = jsonl(&config.work.join("results-new.jsonl"));
    let diff: RouteDiff = read(&config.work.join("routes-diff.json")).unwrap_or_default();
    let middleware: HashMap<&str, &str> = diff
        .changed
        .iter()
        .flat_map(|c| {
            c.changes
                .iter()
                .filter(|x| x.starts_with("middleware"))
                .map(move |x| (c.route.as_str(), x.as_str()))
        })
        .collect();
    let mut fixes: Vec<Fix> = vec![];
    let mut add = |fix: Fix| fixes.push(fix);

    // 1. the new app fails where the old one answered: one fix per cause
    // (cause, location) -> (message, routes, logged lines)
    type Crash = (String, Vec<String>, Vec<String>);
    let mut crashes: BTreeMap<(String, String), Crash> = BTreeMap::new();
    for r in results.iter().filter(|r| r.verdict == "new-broken") {
        let row = new_rows.get(&r.id).cloned().unwrap_or(Value::Null);
        let text = message(&row);
        // no trace: the controller answered the error itself; point at its action
        let place = location(&row, &config.new.app_path)
            .or_else(|| spec_of.get(r.id.as_str()).and_then(|s| s.action.clone()))
            .unwrap_or_default();
        // a controller that catches the exception and answers "Error ..." hides it; the app's log still has it
        let logged: Vec<String> = row["effects"]["logs"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(Value::as_str)
                    .filter(|x| x.starts_with("error") || x.starts_with("critical"))
                    .take(2)
                    .map(|x| format!("logged: {}", x.chars().take(300).collect::<String>()))
                    .collect()
            })
            .unwrap_or_default();
        let entry = crashes
            .entry((cause(&text), place))
            .or_insert_with(|| (text.clone(), vec![], logged));
        entry.1.push(r.name());
    }
    for ((_, place), (text, routes, logged)) in crashes {
        let why = logged
            .iter()
            .map(String::as_str)
            .chain([text.as_str()])
            .collect::<Vec<_>>()
            .join(" ");
        add(Fix {
            priority: 1,
            area: "API".into(),
            title: format!(
                "Crashes on the new app: {}",
                text.chars().take(160).collect::<String>()
            ),
            location: (!place.is_empty()).then_some(place),
            hint: hint(&why).map(String::from),
            routes,
            details: logged,
        });
    }

    // 2. access: open without signing in, or no longer reachable
    let open: Vec<String> = results
        .iter()
        .filter(|r| r.case == "guest" && denied(&r.old_status) && ok(&r.new_status))
        .map(|r| r.key.clone())
        .collect();
    if !open.is_empty() {
        add(Fix {
            priority: 1,
            area: "Access".into(),
            title: format!("{} routes answer WITHOUT signing in on the new app (the old app refused)", open.len()),
            location: None,
            hint: Some("An auth middleware was dropped or its guard changed. Compare the route's middleware between old and new (routes-diff.json).".into()),
            details: open.iter().filter_map(|k| middleware.get(k.as_str()).map(|m| format!("{k}: {m}"))).take(20).collect(),
            routes: open,
        });
    }
    let mut lost: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in results
        .iter()
        .filter(|r| r.case == "happy" && ok(&r.old_status) && denied(&r.new_status))
    {
        let why = middleware
            .get(r.key.as_str())
            .map(|m| m.to_string())
            .unwrap_or_else(|| "middleware unchanged: check policies, gates and scopes".into());
        lost.entry(why).or_default().push(r.key.clone());
    }
    for (why, routes) in lost {
        add(Fix {
            priority: 2,
            area: "Access".into(),
            title: format!(
                "{} routes the signed-in user could use now answer 401/403",
                routes.len()
            ),
            location: None,
            hint: Some(format!(
                "Route change: {why}. If intended, the web app must stop calling these for that user; if not, restore the access."
            )),
            routes,
            details: vec![],
        });
    }

    // 3. validation: a rule lost, or a normal save now refused
    let mut unvalidated: Vec<String> = vec![];
    let mut refused: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in results.iter().filter(|r| r.verdict != "same") {
        let spec = spec_of.get(r.id.as_str());
        let without = spec.map(|s| s.without.join(", ")).unwrap_or_default();
        if (r.case == "empty" || r.case.starts_with("without-"))
            && r.old_status == json!(422)
            && ok(&r.new_status)
        {
            unvalidated.push(if without.is_empty() {
                format!("{} (empty body)", r.key)
            } else {
                format!("{} (without {without})", r.key)
            });
        }
        if r.case == "happy" && ok(&r.old_status) && r.new_status == json!(422) {
            let fields: Vec<String> = new_rows
                .get(&r.id)
                .and_then(|row| {
                    row["json"]["errors"]
                        .as_object()
                        .map(|e| e.keys().cloned().collect())
                })
                .unwrap_or_default();
            refused
                .entry(fields.join(", "))
                .or_default()
                .push(r.key.clone());
        }
    }
    if !unvalidated.is_empty() {
        add(Fix {
            priority: 2,
            area: "Validation".into(),
            title: format!("{} saves the old app refused (422) are accepted by the new app", unvalidated.len()),
            location: None,
            hint: Some("A validation rule was lost in the upgrade (a FormRequest no longer used, a rule removed): bad data can now be saved.".into()),
            routes: unvalidated,
            details: vec![],
        });
    }
    for (fields, routes) in refused {
        add(Fix {
            priority: 2,
            area: "Validation".into(),
            title: format!("A save that works on the old app is refused by the new one (422 on: {})", if fields.is_empty() { "?" } else { &fields }),
            location: None,
            hint: Some("A rule is stricter after the upgrade (a new required field, a type, the Password or date rules). The web app's forms will fail the same way.".into()),
            routes,
            details: vec![],
        });
    }

    // 4. the web app's calls the upgrade breaks
    if let Some(web) = crate::web::last(config) {
        let mut by_kind: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
        for item in web["broken_by_upgrade"].as_array().into_iter().flatten() {
            by_kind
                .entry(item["kind"].as_str().unwrap_or("?").to_string())
                .or_default()
                .push(item);
        }
        for (kind, items) in by_kind {
            let (priority, what) = match kind.as_str() {
                "missing" => (1, "call a route the new API does not have"),
                "method_mismatch" => (1, "use an HTTP method the new route does not accept"),
                "missing_required" => (1, "do not send a field the new API requires"),
                "fields_not_returned" => (2, "read response fields the new API no longer returns"),
                "possible_rename" => (2, "send a field the new API seems to have renamed"),
                "variant_without_route" => (2, "build a URL variant the new API has no route for"),
                _ => (3, "send fields the new API ignores"),
            };
            let details: Vec<String> = items
                .iter()
                .map(|i| {
                    let mut line = format!(
                        "{} `{}`",
                        i["at"].as_str().unwrap_or("?"),
                        i["call"].as_str().unwrap_or("?")
                    );
                    if !i["detail"].is_null() {
                        line += &format!(" — {}", i["detail"]);
                    }
                    if let Some(alt) = i["path_exists_as"].as_array().filter(|a| !a.is_empty()) {
                        line += &format!(
                            " — the path exists as {}",
                            alt.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" / ")
                        );
                    }
                    if let Some(alt) = i["closest"].as_array().filter(|a| !a.is_empty()) {
                        line += &format!(
                            " — closest route: {}",
                            alt.iter()
                                .take(2)
                                .map(|v| v.as_str().map(String::from).unwrap_or(v.to_string()))
                                .collect::<Vec<_>>()
                                .join(" / ")
                        );
                    }
                    line
                })
                .collect();
            add(Fix {
                priority,
                area: "UI".into(),
                title: format!("{} web app calls {what}", items.len()),
                location: None,
                hint: Some("Change the call in the web app (file:line below), or keep the old route/field in the API.".into()),
                routes: items.iter().filter_map(|i| i["route"].as_str().map(String::from)).collect(),
                details,
            });
        }
    }

    // 4b. the upgraded code itself (the `code` step)
    if let Some(code) = crate::code::last(config) {
        let items = |v: &Value| v.as_array().cloned().unwrap_or_default();
        let missing_new = items(&code["missing_code"]["new"]);
        if !missing_new.is_empty() {
            add(Fix {
                priority: 1,
                area: "Code".into(),
                title: format!("{} routes point to controller methods that do not exist (new in the upgrade)", missing_new.len()),
                location: None,
                hint: Some("The route still names a method the controller no longer has (renamed or not ported): add the method or fix the route. Calling the route crashes.".into()),
                routes: missing_new.iter().filter_map(|m| m["route"].as_str().map(String::from)).collect(),
                details: missing_new.iter().map(|m| format!("{} — {}", m["location"].as_str().unwrap_or("?"), m["detail"].as_str().unwrap_or(""))).collect(),
            });
        }
        let missing_before = items(&code["missing_code"]["before"]);
        if !missing_before.is_empty() {
            add(Fix {
                priority: 4,
                area: "Code".into(),
                title: format!("{} routes point to controller methods that do not exist (already broken before the upgrade)", missing_before.len()),
                location: None,
                hint: Some("Not caused by the upgrade, but these routes crash when called: remove them or add the methods.".into()),
                routes: missing_before.iter().filter_map(|m| m["route"].as_str().map(String::from)).collect(),
                details: missing_before.iter().map(|m| format!("{} — {}", m["location"].as_str().unwrap_or("?"), m["detail"].as_str().unwrap_or(""))).collect(),
            });
        }
        let warned = items(&code["warnings"]["new"]);
        let (parse, other): (Vec<Value>, Vec<Value>) = warned.into_iter().partition(|w| {
            w["message"]
                .as_str()
                .unwrap_or("")
                .contains("could not parse")
        });
        if !parse.is_empty() {
            add(Fix {
                priority: 2,
                area: "Code".into(),
                title: format!("{} places codemap could not parse in the new code", parse.len()),
                location: None,
                hint: Some("A syntax error, or syntax newer than codemap reads; the syntax check below says which PHP rejects.".into()),
                routes: vec![],
                details: parse.iter().map(|w| format!("{}:{}", w["path"].as_str().unwrap_or("?"), w["line"])).collect(),
            });
        }
        if !other.is_empty() {
            add(Fix {
                priority: 2,
                area: "Code".into(),
                title: format!("{} migration / schema problems in the new code", other.len()),
                location: None,
                hint: Some("A fresh `php artisan migrate` fails or builds another schema than the one the code expects.".into()),
                routes: vec![],
                details: other.iter().map(|w| format!("{}:{} — {}", w["path"].as_str().unwrap_or("?"), w["line"], w["message"].as_str().unwrap_or(""))).collect(),
            });
        }
        for rule in items(&code["rules"]) {
            let hits: Vec<String> = items(&rule["hits"])
                .iter()
                .filter_map(|h| h.as_str().map(String::from))
                .collect();
            add(Fix {
                priority: rule["priority"].as_u64().unwrap_or(3) as u8,
                area: "Code".into(),
                title: format!(
                    "{} ({} places)",
                    rule["title"].as_str().unwrap_or("?"),
                    hits.len()
                ),
                location: None,
                hint: rule["fix"].as_str().map(String::from),
                routes: vec![],
                details: hits,
            });
        }
        let lint_errors = items(&code["lint"]["errors"]);
        if !lint_errors.is_empty() {
            add(Fix {
                priority: 1,
                area: "Code".into(),
                title: format!(
                    "{} files the new PHP ({}) cannot parse",
                    lint_errors.len(),
                    code["lint"]["php"].as_str().unwrap_or("?")
                ),
                location: None,
                hint: Some("A fatal error as soon as the file is loaded.".into()),
                routes: vec![],
                details: lint_errors
                    .iter()
                    .map(|e| {
                        format!(
                            "{}:{} — {}",
                            e["file"].as_str().unwrap_or("?"),
                            e["line"],
                            e["message"].as_str().unwrap_or("")
                        )
                    })
                    .collect(),
            });
        }
        if let Some(groups) = code["phpstan"]["groups"].as_object() {
            let mut groups: Vec<(&String, &Value)> = groups.iter().collect();
            groups.sort_by_key(|(_, v)| std::cmp::Reverse(v.as_array().map_or(0, Vec::len)));
            for (message, places) in groups.into_iter().take(15) {
                let places: Vec<String> = places
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p.as_str().map(String::from))
                    .collect();
                add(Fix {
                    priority: 3,
                    area: "Code".into(),
                    title: format!(
                        "PHPStan: {} ({} places)",
                        message.chars().take(150).collect::<String>(),
                        places.len()
                    ),
                    location: None,
                    hint: None,
                    routes: vec![],
                    details: places,
                });
            }
        }
    }

    // 5. answers that lost fields, other status changes, different side effects
    let mut lost_fields: Vec<String> = vec![];
    let mut status_changes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut effects: Vec<String> = vec![];
    for r in &results {
        match r.verdict.as_str() {
            "body" => {
                let missing: Vec<&str> = r
                    .notes
                    .iter()
                    .filter(|n| n.contains("is missing in new"))
                    .map(|n| n.split(" is missing").next().unwrap_or(""))
                    .collect();
                if !missing.is_empty() {
                    lost_fields.push(format!("{}: {}", r.name(), missing.join(", ")));
                }
            }
            "status"
                if !(r.case == "guest"
                    || (ok(&r.old_status) && denied(&r.new_status))
                    || (ok(&r.old_status) && r.new_status == json!(422))) =>
            {
                status_changes
                    .entry(format!("{} -> {}", r.old_status, r.new_status))
                    .or_default()
                    .push(r.name());
            }
            "effects" => effects.push(format!(
                "{}: {}",
                r.name(),
                r.notes
                    .iter()
                    .find(|n| n.contains("only old"))
                    .cloned()
                    .unwrap_or_default()
            )),
            _ => {}
        }
    }
    if !lost_fields.is_empty() {
        add(Fix {
            priority: 3,
            area: "Response".into(),
            title: format!("{} answers lost fields the old app returned", lost_fields.len()),
            location: None,
            hint: Some("A resource, cast or relation changed. If the web app reads one of these fields its screen shows nothing there.".into()),
            routes: vec![],
            details: lost_fields,
        });
    }
    for (change, routes) in status_changes {
        add(Fix {
            priority: 3,
            area: "API".into(),
            title: format!("Status {change} on {} requests", routes.len()),
            location: None,
            hint: None,
            routes,
            details: vec![],
        });
    }
    if !effects.is_empty() {
        add(Fix {
            priority: 3,
            area: "Side effects".into(),
            title: format!(
                "{} requests write other rows or send other jobs, mail or broadcasts",
                effects.len()
            ),
            location: None,
            hint: Some(
                "Same answer, different work behind it: check events, observers and queued jobs."
                    .into(),
            ),
            routes: vec![],
            details: effects,
        });
    }

    // scenarios someone added that crash on both apps: a bug the old code already had
    let ai_crashes: Vec<String> = results
        .iter()
        .filter(|r| {
            r.verdict == "both-fail" && (r.case.starts_with("ai-") || r.case.starts_with("log-"))
        })
        .map(|r| {
            format!(
                "{}: {}",
                r.name(),
                r.notes.first().cloned().unwrap_or_default()
            )
        })
        .collect();
    if !ai_crashes.is_empty() {
        add(Fix {
            priority: 3,
            area: "API".into(),
            title: format!("{} added scenarios (AI or access log) crash on both apps (bugs the old code already had)", ai_crashes.len()),
            location: None,
            hint: Some("Not caused by the upgrade, and no test can expect them until the code answers properly (a 404 or 422 instead of a 500).".into()),
            routes: vec![],
            details: ai_crashes,
        });
    }

    // 6. what this run could not check
    let meta: Value = read(&config.work.join("run.json")).unwrap_or(json!({}));
    for (side, kind, version) in crate::ops::missing_capabilities(&meta) {
        add(Fix {
            priority: 4,
            area: "Not checked".into(),
            title: format!(
                "{kind} were not compared: the {side} app (Laravel {version}) lacks an internal the tool reads"
            ),
            location: None,
            hint: Some("Update laravel-parity (`laravel-parity update`).".into()),
            routes: vec![],
            details: vec![],
        });
    }
    let unfilled = specs
        .iter()
        .filter(|s| s.case == "happy" && !s.unresolved.is_empty())
        .count();
    if unfilled > 0 {
        add(Fix {
            priority: 4,
            area: "Not checked".into(),
            title: format!("{unfilled} routes were not run: a route parameter had no value in the test data"),
            location: None,
            hint: Some("Give them values in the pair file's [params], or add rows to the old database; report.md lists them.".into()),
            routes: vec![],
            details: vec![],
        });
    }
    let both_fail = results
        .iter()
        .filter(|r| r.verdict == "both-fail" && r.case == "happy")
        .count();
    if both_fail > 0 {
        add(Fix {
            priority: 4,
            area: "Not checked".into(),
            title: format!("{both_fail} routes fail on both apps, so the upgrade could not be judged for them"),
            location: None,
            hint: Some("Each one's error is in report.md (the same crash on both apps: a bug the old code already had, or test data the route needs). Set a body or values with the MCP tool set_request or in requests.json, then rerun.".into()),
            routes: vec![],
            details: vec![],
        });
    }
    // saves both apps refused even after learning from the old app's validation errors: what each still wants
    let refused: Vec<String> = results
        .iter()
        .filter(|r| r.verdict == "rejected" && r.case == "happy")
        .map(|r| {
            let said = r
                .notes
                .first()
                .map(|n| n.trim_start_matches("both apps refuse the input: "))
                .unwrap_or_default();
            format!("{}: {said}", r.key)
        })
        .collect();
    if !refused.is_empty() {
        add(Fix {
            priority: 4,
            area: "Not checked".into(),
            title: format!(
                "{} saves were refused by validation on both apps, so what they save was not compared",
                refused.len()
            ),
            location: None,
            hint: Some("Each line says what the old app still wants. Give a real body with set_request or in requests.json (or record real requests: `laravel-parity recorder`), then rerun.".into()),
            routes: vec![],
            details: refused,
        });
    }

    // the database, commands / jobs / schedule and broadcast channels (their own steps), and speed
    fixes.extend(crate::schema::fixes(config));
    fixes.extend(crate::console::fixes(config));
    fixes.extend(crate::units::fixes(config));
    fixes.extend(crate::channels::fixes(config));
    fixes.extend(speed(&results));

    fixes.sort_by(|a, b| {
        (
            a.priority,
            std::cmp::Reverse(a.routes.len().max(a.details.len())),
        )
            .cmp(&(
                b.priority,
                std::cmp::Reverse(b.routes.len().max(b.details.len())),
            ))
    });
    Ok(fixes)
}

/// Routes that answer the same but work much harder on the new app: many more queries (an N+1 the upgrade brought:
/// a lost eager load, a relation renamed) or much slower. Time is noisy (other machines, other MySQL); queries are not.
pub fn speed(results: &[Outcome]) -> Vec<Fix> {
    let answered =
        |r: &&Outcome| !["new-broken", "both-fail", "new-fixed"].contains(&r.verdict.as_str());
    let mut more: Vec<(i64, String)> = results
        .iter()
        .filter(answered)
        .filter_map(|r| {
            let [a, b] = r.queries?;
            (b >= a * 2 && b - a >= 10)
                .then(|| (b - a, format!("{}: {a} -> {b} queries", r.name())))
        })
        .collect();
    let mut slower: Vec<(i64, String)> = results
        .iter()
        .filter(answered)
        .filter_map(|r| {
            let [a, b] = r.ms?;
            (b >= a * 3 && b - a >= 500).then(|| (b - a, format!("{}: {a} -> {b} ms", r.name())))
        })
        .collect();
    more.sort_by_key(|x| std::cmp::Reverse(x.0));
    slower.sort_by_key(|x| std::cmp::Reverse(x.0));
    let mut out = vec![];
    if !more.is_empty() {
        out.push(Fix {
            priority: 3,
            area: "Speed".into(),
            title: format!("{} routes run far more database queries on the new app", more.len()),
            location: None,
            hint: Some("Usually an N+1: an eager load (with()) or a relation lost or renamed in the upgrade, or a $with removed from a model. Production data makes it worse than the test copy.".into()),
            routes: vec![],
            details: more.into_iter().map(|(_, d)| d).collect(),
        });
    }
    if !slower.is_empty() {
        out.push(Fix {
            priority: 4,
            area: "Speed".into(),
            title: format!("{} routes answer much slower on the new app", slower.len()),
            location: None,
            hint: Some("Times depend on the machine and MySQL server each app ran on; rerun the route to confirm before digging in.".into()),
            routes: vec![],
            details: slower.into_iter().map(|(_, d)| d).collect(),
        });
    }
    out
}

const PRIORITY: [&str; 4] = ["Fix before release", "Fix soon", "Check", "Know about"];

/// fixes.md and fixes.json in the run folder.
pub fn write_all(config: &Config) -> Res<Vec<Fix>> {
    let fixes = build(config)?;
    write(&config.work.join("fixes.json"), &fixes)?;
    let mut out = String::new();
    let _ = writeln!(out, "# What to fix: {}\n", config.name);
    let _ = writeln!(
        out,
        "From comparing the old app ({}) with the upgraded one ({}): every item is something the upgrade changed. \
         Worst first; tick them off as you go. Details per route are in report.md; the MCP tool `show` gives one \
         route's old and new answer side by side.\n",
        config.old.repo.display(),
        config.new.repo.display()
    );
    let _ = writeln!(
        out,
        "| Priority | Items | Routes or calls |\n| --- | ---: | ---: |"
    );
    for (i, label) in PRIORITY.iter().enumerate() {
        let items: Vec<&Fix> = fixes
            .iter()
            .filter(|f| f.priority as usize == i + 1)
            .collect();
        if !items.is_empty() {
            let count: usize = items
                .iter()
                .map(|f| f.routes.len().max(f.details.len()))
                .sum();
            let _ = writeln!(out, "| {} {label} | {} | {count} |", i + 1, items.len());
        }
    }
    for (i, label) in PRIORITY.iter().enumerate() {
        let items: Vec<&Fix> = fixes
            .iter()
            .filter(|f| f.priority as usize == i + 1)
            .collect();
        if items.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n## {} {label}\n", i + 1);
        for fix in items {
            let _ = writeln!(out, "- [ ] **[{}] {}**", fix.area, fix.title);
            if let Some(l) = &fix.location {
                let _ = writeln!(out, "  - Where: `{l}` (new repo)");
            }
            if let Some(h) = &fix.hint {
                let _ = writeln!(out, "  - Likely cause: {h}");
            }
            if !fix.routes.is_empty() {
                let shown: Vec<String> = fix
                    .routes
                    .iter()
                    .take(8)
                    .map(|r| format!("`{r}`"))
                    .collect();
                let more = if fix.routes.len() > 8 {
                    format!(" and {} more", fix.routes.len() - 8)
                } else {
                    String::new()
                };
                let _ = writeln!(
                    out,
                    "  - Routes ({}): {}{more}",
                    fix.routes.len(),
                    shown.join(", ")
                );
            }
            for d in fix.details.iter().take(15) {
                let _ = writeln!(out, "  - {d}");
            }
            if fix.details.len() > 15 {
                let _ = writeln!(out, "  - … {} more in fixes.json", fix.details.len() - 15);
            }
        }
    }
    std::fs::write(config.work.join("fixes.md"), out).map_err(|e| e.to_string())?;
    Ok(fixes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pair file in a fresh temp folder whose runs go to <folder>/work; the repos need not exist.
    fn config(name: &str) -> Config {
        let dir = std::env::temp_dir().join(format!(
            "laravel-parity-fixlist-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).unwrap();
        let pair = dir.join("shop.toml");
        let toml = format!(
            "name = \"shop\"\nwork = '{}'\n[old]\nrepo = \"/nonexistent/shop-old\"\ncontainer = \"shop-old-php\"\n\
             app_path = \"/var/www/html\"\n[new]\nrepo = \"/nonexistent/shop\"\ncontainer = \"shop-php\"\n\
             app_path = \"/var/www/html\"\n[database]\nsource = \"shop\"\n",
            dir.join("work").display()
        );
        std::fs::write(&pair, toml).unwrap();
        crate::config::load(&pair).unwrap()
    }

    fn outcome(id: &str, key: &str, case: &str, verdict: &str, old: i64, new: i64) -> Value {
        json!({"id": id, "key": key, "case": case, "uri": key.split(' ').nth(1).unwrap_or(""),
               "verdict": verdict, "old_status": old, "new_status": new, "notes": []})
    }

    #[test]
    fn hints_and_places() {
        assert!(
            hint("Foo::bar(): Argument #1 ($id) must be of type int, string given")
                .unwrap()
                .starts_with("A type added")
        );
        assert!(
            hint("SQLSTATE[42S22]: Unknown column 'shop_id' in 'where clause'")
                .unwrap()
                .contains("migration")
        );
        assert!(hint("Error saving order.").unwrap().contains("try/catch"));
        assert_eq!(hint("everything is fine"), None);
        assert_eq!(cause("No order 42 for 'x' given"), "No order … for …");
        assert_eq!(cause("Bad (SQL: select * from orders)"), "Bad …");
        let row = json!({"json": {"exception": "Symfony\\Component\\HttpKernel\\Exception\\HttpException",
            "message": "Boom", "file": "/var/www/html/vendor/laravel/framework/src/Foo.php", "line": 10,
            "trace": [{"file": "/var/www/html/vendor/x.php", "line": 1},
                      {"file": "/var/www/html/app/Http/Controllers/OrderController.php", "line": 42}]}});
        assert_eq!(
            location(&row, "/var/www/html/").as_deref(),
            Some("app/Http/Controllers/OrderController.php:42")
        );
        assert_eq!(message(&row), "HttpException: Boom");
        assert_eq!(message(&json!({"error": "timed out"})), "timed out");
        assert_eq!(
            location(&json!({"error": "timed out"}), "/var/www/html"),
            None
        );
        assert!(ok(&json!(204)) && !ok(&json!(302)) && !ok(&Value::Null));
        assert!(denied(&json!(401)) && denied(&json!(403)) && !denied(&json!(404)));
    }

    #[test]
    fn speed_items() {
        let mut fast: Outcome =
            serde_json::from_value(outcome("1", "GET api/orders", "happy", "same", 200, 200))
                .unwrap();
        fast.queries = Some([3, 40]);
        fast.ms = Some([100, 900]);
        let mut small = fast.clone();
        small.key = "GET api/shops".into();
        small.queries = Some([3, 9]); // more than double, but under 10 more
        small.ms = Some([100, 500]); // under three times
        let mut broken = fast.clone();
        broken.verdict = "new-broken".into();
        let fixes = speed(&[fast, small, broken]);
        assert_eq!(fixes.len(), 2);
        assert_eq!((fixes[0].priority, fixes[0].area.as_str()), (3, "Speed"));
        assert_eq!(fixes[0].details, ["GET api/orders: 3 -> 40 queries"]);
        assert_eq!(fixes[1].priority, 4);
        assert_eq!(fixes[1].details, ["GET api/orders: 100 -> 900 ms"]);
        assert!(speed(&[]).is_empty());
    }

    #[test]
    fn items_from_a_run() {
        let config = config("run");
        assert_eq!(build(&config).unwrap_err(), "no results yet: run first");
        let work = &config.work;
        let mut results = vec![
            outcome(
                "1",
                "GET api/orders/{order}",
                "happy",
                "new-broken",
                200,
                500,
            ),
            outcome("2", "GET api/orders", "happy", "new-broken", 200, 500),
            outcome("3", "GET api/admin", "guest", "status", 401, 200),
            outcome("4", "GET api/profile", "happy", "status", 200, 403),
            outcome("5", "POST api/orders", "empty", "status", 422, 201),
            outcome("6", "POST api/users", "without-email", "status", 422, 200),
            outcome("7", "PUT api/orders/{order}", "happy", "status", 200, 422),
            outcome("8", "GET api/cart", "happy", "body", 200, 200),
            outcome("9", "POST api/cart", "happy", "effects", 200, 200),
            outcome("10", "GET api/reports", "happy", "both-fail", 500, 500),
            outcome("11", "GET api/reports", "ai-1", "both-fail", 500, 500),
            outcome("12", "POST api/shops", "happy", "rejected", 422, 422),
        ];
        results[7]["notes"] = json!(["$.data.total is missing in new", "$.data.tax differs"]);
        results[8]["notes"] = json!(["rows: only old wrote orders"]);
        results[10]["notes"] = json!(["500 on both"]);
        results[11]["notes"] =
            json!(["both apps refuse the input: name: The name field is required."]);
        write(&work.join("results.json"), &results).unwrap();
        let trace = json!({"exception": "TypeError",
            "message": "App\\Http\\Controllers\\OrderController::show(): Argument #1 ($id) must be of type int, string given",
            "file": "/var/www/html/vendor/laravel/framework/src/Route.php", "line": 3,
            "trace": [{"file": "/var/www/html/app/Http/Controllers/OrderController.php", "line": 42}]});
        let rows = [
            json!({"id": "1", "json": trace}),
            json!({"id": "2", "json": trace}),
            json!({"id": "7", "json": {"message": "invalid", "errors": {"phone": ["required"]}}}),
        ];
        let jsonl: String = rows.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(work.join("results-new.jsonl"), jsonl).unwrap();
        let specs = vec![
            Spec {
                id: "6".into(),
                key: "POST api/users".into(),
                case: "without-email".into(),
                without: vec!["email".into()],
                ..Default::default()
            },
            Spec {
                id: "13".into(),
                key: "GET api/reports/{year}".into(),
                case: "happy".into(),
                unresolved: vec!["year".into()],
                ..Default::default()
            },
        ];
        write(&work.join("requests.json"), &specs).unwrap();
        let diff = json!({"removed": [], "added": [],
            "changed": [{"route": "GET api/admin", "changes": ["middleware: -auth:api"]}]});
        write(&work.join("routes-diff.json"), &diff).unwrap();
        let meta =
            json!({"new": {"info": {"laravel": "11.0", "capabilities": {"broadcasts": false}}}});
        write(&work.join("run.json"), &meta).unwrap();

        let fixes = write_all(&config).unwrap();
        let find = |title: &str| {
            fixes
                .iter()
                .find(|f| f.title.starts_with(title))
                .unwrap_or_else(|| panic!("no fix titled {title}"))
        };
        // one crash cause on two routes, pointed at the app's own frame, worst first
        let crash = &fixes[0];
        assert_eq!((crash.priority, crash.area.as_str()), (1, "API"));
        assert!(
            crash
                .title
                .starts_with("Crashes on the new app: TypeError: ")
        );
        assert_eq!(crash.routes, ["GET api/orders/{order}", "GET api/orders"]);
        assert_eq!(
            crash.location.as_deref(),
            Some("app/Http/Controllers/OrderController.php:42")
        );
        assert!(crash.hint.as_deref().unwrap().starts_with("A type added"));
        let open = find("1 routes answer WITHOUT signing in");
        assert_eq!((open.priority, open.area.as_str()), (1, "Access"));
        assert_eq!(open.details, ["GET api/admin: middleware: -auth:api"]);
        let lost = find("1 routes the signed-in user could use");
        assert_eq!(lost.priority, 2);
        assert!(
            lost.hint
                .as_deref()
                .unwrap()
                .contains("middleware unchanged")
        );
        let unvalidated = find("2 saves the old app refused");
        assert_eq!(
            unvalidated.routes,
            [
                "POST api/orders (empty body)",
                "POST api/users (without email)"
            ]
        );
        let refused = find("A save that works on the old app is refused");
        assert!(refused.title.ends_with("(422 on: phone)"));
        assert_eq!(refused.routes, ["PUT api/orders/{order}"]);
        let fields = find("1 answers lost fields");
        assert_eq!(fields.details, ["GET api/cart: $.data.total"]);
        // 401/403 and 422 changes have their own items; guest cases too
        assert_eq!(
            find("Status 422 -> 201").routes,
            ["POST api/orders [empty]"]
        );
        assert_eq!(
            find("Status 422 -> 200").routes,
            ["POST api/users [without-email]"]
        );
        assert!(!fixes.iter().any(|f| f.title.contains("200 -> 4")));
        assert_eq!(
            find("1 requests write other rows").details,
            ["POST api/cart: rows: only old wrote orders"]
        );
        assert_eq!(
            find("1 added scenarios").details,
            ["GET api/reports [ai-1]: 500 on both"]
        );
        // what was not checked
        let caps = find("broadcasts were not compared");
        assert!(caps.title.contains("the new app (Laravel 11.0)"));
        assert_eq!(caps.priority, 4);
        assert_eq!(find("1 routes were not run").area, "Not checked");
        assert_eq!(find("1 routes fail on both apps").priority, 4);
        assert_eq!(
            find("1 saves were refused by validation").details,
            ["POST api/shops: name: The name field is required."]
        );
        // worst first, then the biggest item first
        assert!(fixes.windows(2).all(|w| w[0].priority <= w[1].priority));

        let md = std::fs::read_to_string(work.join("fixes.md")).unwrap();
        assert!(md.starts_with("# What to fix: shop\n"));
        assert!(md.contains("| 1 Fix before release | 2 | 3 |"));
        assert!(md.contains("\n## 4 Know about\n"));
        assert!(md.contains("- [ ] **[Access] 1 routes answer WITHOUT signing in"));
        assert!(md.contains("  - Where: `app/Http/Controllers/OrderController.php:42` (new repo)"));
        assert!(md.contains("  - Routes (2): `GET api/orders/{order}`, `GET api/orders`\n"));
        let saved: Vec<Fix> = read(&work.join("fixes.json")).unwrap();
        assert_eq!(saved.len(), fixes.len());
        let _ = std::fs::remove_dir_all(work.parent().unwrap());
    }
}
