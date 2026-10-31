//! Real requests from a web server's access log (nginx / Apache "combined", or any line with `"GET /path HTTP/1.1"
//! 200`) as extra cases: the ids, filters, sorting and paging real users sent, instead of values the tool made up.
//! Only successful GETs (an access log has no request bodies); query values that look like secrets are dropped.
//! The recorder (`laravel-parity recorder`, a middleware for the old app on a test or staging server) writes real saves
//! with their bodies as JSON lines; the same import reads them as `rec-<n>` cases.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::config::Config;
use crate::ops::{Res, read, write};
use crate::plan::{PARAM, SLASHES, Spec, request_id, variant};
use crate::say;

static LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(GET|POST|PUT|PATCH|DELETE|HEAD) (\S+) HTTP/[\d.]+"\s+(\d{3})"#).unwrap()
});
static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)token|secret|password|passwd|signature|api_?key|auth|session|code$").unwrap()
});

/// "%2F" and "+" back to text.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A query string as JSON, `a[]=1&a[]=2` as a list; secret-looking keys left out.
pub fn query(text: &str) -> Map<String, Value> {
    let mut out = Map::new();
    for part in text.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        let (key, value) = (decode(key), decode(value));
        if SECRET.is_match(key.trim_end_matches("[]")) {
            continue;
        }
        if let Some(name) = key.strip_suffix("[]") {
            let list = out.entry(name.to_string()).or_insert(json!([]));
            if let Some(items) = list.as_array_mut() {
                items.push(json!(value));
            }
        } else {
            out.insert(key, json!(value));
        }
    }
    out
}

/// A route template as a regex that captures its parameters by name.
fn pattern(template: &str) -> Option<Regex> {
    let mut regex = String::from("^/?");
    let mut last = 0;
    for cap in PARAM.captures_iter(template) {
        let whole = cap.get(0).unwrap();
        regex.push_str(&regex::escape(&template[last..whole.start()]));
        if cap.get(2).is_some() {
            regex.push_str(&format!("(?P<{}>[^/]*)", &cap[1]));
        } else {
            regex.push_str(&format!("(?P<{}>[^/]+)", &cap[1]));
        }
        last = whole.end();
    }
    regex.push_str(&regex::escape(&template[last..]));
    regex.push_str("/?$");
    Regex::new(&regex).ok()
}

/// Add up to `per_route` distinct real requests per route from the log as `log-<n>` cases (kept by later plans).
pub fn import(config: &Config, file: &Path, per_route: usize) -> Res<String> {
    let path = config.work.join("requests.json");
    let mut specs: Vec<Spec> = read(&path).ok_or("no plan yet: run plan first")?;
    let text = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let line = add_from(&mut specs, &String::from_utf8_lossy(&text), per_route);
    write(&path, &specs)?;
    say(&line);
    Ok(line)
}

/// The cases a log or recorder file adds to the plan; returns the summary line.
fn add_from(specs: &mut Vec<Spec>, text: &str, per_route: usize) -> String {
    // GET routes, most specific template first (api/units/export before api/units/{unit})
    let mut routes: Vec<(&Spec, Regex)> = specs
        .iter()
        .filter(|s| s.case == "happy" && s.method == "GET")
        .filter_map(|s| Some((s, pattern(&s.template)?)))
        .collect();
    routes.sort_by_key(|(s, _)| {
        (
            s.template.matches('{').count(),
            std::cmp::Reverse(s.template.len()),
        )
    });
    let existing: BTreeSet<(String, String)> = specs
        .iter()
        .filter(|s| s.custom)
        .map(|s| (s.key.clone(), s.case.clone()))
        .collect();
    let mut seen: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut added: Vec<Spec> = vec![];
    let (mut lines, mut matched) = (0, 0);
    let all_routes: HashMap<&str, &Spec> = specs
        .iter()
        .filter(|s| s.case == "happy")
        .map(|s| (s.key.as_str(), s))
        .collect();
    for line in text.lines() {
        // a line the recorder wrote: {"method", "route", "values", "query", "body", "status"}
        if line.trim_start().starts_with('{') {
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            lines += 1;
            let key = format!(
                "{} {}",
                entry["method"].as_str().unwrap_or_default(),
                entry["route"]
                    .as_str()
                    .unwrap_or_default()
                    .trim_start_matches('/')
            );
            let Some(happy) = all_routes.get(key.as_str()).copied() else {
                continue;
            };
            matched += 1;
            let clean = |v: &Value| -> Map<String, Value> {
                v.as_object()
                    .map(|m| {
                        m.iter()
                            .filter(|(k, _)| !SECRET.is_match(k))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let query = clean(&entry["query"]);
            let body = clean(&entry["body"]);
            let values: BTreeMap<String, String> = entry["values"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                v.as_str()
                                    .map(String::from)
                                    .unwrap_or_else(|| v.to_string()),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let signature = format!("{values:?}{query:?}{body:?}");
            let known = seen.entry(happy.key.clone()).or_default();
            if known.len() >= per_route || !known.insert(signature.clone()) {
                continue;
            }
            let case_name = format!("rec-{}", &request_id(&signature)[..6]);
            if existing.contains(&(happy.key.clone(), case_name.clone())) {
                continue;
            }
            let mut case = variant(happy, &case_name);
            case.custom = true;
            case.note = Some(json!("recorded on the old app"));
            case.query = (!query.is_empty()).then_some(query);
            case.body = (happy.method != "GET").then_some(Value::Object(body));
            case.made = None;
            add_case(&mut added, case, &values);
            continue;
        }
        let Some(cap) = LINE.captures(line) else {
            continue;
        };
        lines += 1;
        let status: u16 = cap[3].parse().unwrap_or(0);
        if &cap[1] != "GET" || !(200..400).contains(&status) {
            continue;
        }
        let target = &cap[2];
        let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
        let path = decode(raw_path);
        // behind a proxy the app's path can carry a prefix (/service/api/...): try without the first segment too
        let shorter = path
            .trim_start_matches('/')
            .split_once('/')
            .map(|(_, rest)| format!("/{rest}"));
        let Some((happy, regex, found)) = [Some(path.clone()), shorter]
            .into_iter()
            .flatten()
            .find_map(|p| {
                routes.iter().find_map(|(s, r)| {
                    r.captures(&p).map(|c| {
                        (
                            *s,
                            r,
                            c.iter()
                                .map(|m| m.map(|m| m.as_str().to_string()))
                                .collect::<Vec<_>>(),
                        )
                    })
                })
            })
        else {
            continue;
        };
        matched += 1;
        let query = query(raw_query);
        let mut values = BTreeMap::new();
        for (i, name) in regex.capture_names().enumerate() {
            if let (Some(name), Some(Some(v))) = (name, found.get(i))
                && !v.is_empty()
            {
                values.insert(name.to_string(), v.clone());
            }
        }
        let signature = format!("{values:?}{query:?}");
        let known = seen.entry(happy.key.clone()).or_default();
        if known.len() >= per_route || !known.insert(signature.clone()) {
            continue;
        }
        let case_name = format!("log-{}", &request_id(&signature)[..6]);
        if existing.contains(&(happy.key.clone(), case_name.clone())) {
            continue;
        }
        let mut case = variant(happy, &case_name);
        case.custom = true;
        case.note = Some(json!("from the access log"));
        case.query = (!query.is_empty()).then_some(query);
        add_case(&mut added, case, &values);
    }
    let routes_hit = added
        .iter()
        .map(|s| s.key.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let count = added.len();
    specs.extend(added);
    specs.sort_by(|a, b| {
        (&a.key, a.case != "happy", &a.case).cmp(&(&b.key, b.case != "happy", &b.case))
    });
    format!(
        "{lines} requests in the file, {matched} on planned routes (successful GETs from an access log, saves from the \
         recorder); added {count} cases on {routes_hit} routes (at most {per_route} per route). Next: run (or rerun a \
         route) to send them to both apps."
    )
}

/// The route's parameters as the real request had them, then the case joins the others.
fn add_case(added: &mut Vec<Spec>, mut case: Spec, values: &BTreeMap<String, String>) {
    for (k, v) in values {
        case.values.insert(k.clone(), v.clone());
        case.filled.insert(k.clone(), v.clone());
    }
    let filled = case.filled.clone();
    let uri = PARAM.replace_all(&case.template, |c: &regex::Captures| {
        filled.get(&c[1]).cloned().unwrap_or_default()
    });
    case.uri = SLASHES
        .replace_all(&uri, "/")
        .trim_end_matches('/')
        .to_string();
    added.push(case);
}

/// The middleware that records real saves on the old app, written to `runs/<name>/ParityRecorder.php`.
pub fn recorder(config: &Config) -> Res<String> {
    let path = config.work.join("ParityRecorder.php");
    std::fs::create_dir_all(&config.work).map_err(|e| e.to_string())?;
    std::fs::write(&path, include_str!("../php/ParityRecorder.php")).map_err(|e| e.to_string())?;
    let line = format!(
        "Wrote {}.\n\
         It records the saves (POST, PUT, PATCH, DELETE answered 2xx) a real user makes, with their bodies, so the tool can\n\
         send the same saves to both apps. Secret-looking values and uploaded files are left out.\n\n\
         1. Use a TEST or STAGING copy of the OLD app, not production. Copy the file to app/Http/Middleware/.\n\
         2. Register it for the API routes:\n\
         \x20  Laravel 5.5 to 10: in app/Http/Kernel.php add \\App\\Http\\Middleware\\ParityRecorder::class to\n\
         \x20  $middlewareGroups['api'] (and 'web' if the app saves through web routes).\n\
         \x20  Laravel 11 and later: in bootstrap/app.php, ->withMiddleware(function ($middleware) {{\n\
         \x20  $middleware->api(append: [\\App\\Http\\Middleware\\ParityRecorder::class]); }})\n\
         3. Use the app (or let testers use it) for a while. Saves land in storage/logs/parity-requests.jsonl.\n\
         4. Copy that file here and run: laravel-parity import-log <pair file> parity-requests.jsonl\n\
         5. Remove the middleware.",
        path.display()
    );
    say(&line);
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_and_queries() {
        let re = pattern("api/units/{unit}/leases/{lease?}").unwrap();
        let c = re.captures("/api/units/12/leases/").unwrap();
        assert_eq!(&c["unit"], "12");
        assert!(re.captures("/api/units/12/other").is_none());
        let q = query("page=2&sort=name%20asc&ids[]=1&ids[]=2&api_token=x&access_token=y");
        assert_eq!(q["page"], "2");
        assert_eq!(q["sort"], "name asc");
        assert_eq!(q["ids"], json!(["1", "2"]));
        assert!(!q.contains_key("api_token") && !q.contains_key("access_token"));
        let line = r#"1.2.3.4 - - [31/Oct/2026:10:00:00 +0800] "GET /api/units/5?page=3 HTTP/1.1" 200 512 "-" "x""#;
        assert!(LINE.is_match(line));
    }

    #[test]
    fn recorded_saves() {
        let happy = Spec {
            id: "a".into(),
            key: "PUT api/units/{unit}".into(),
            case: "happy".into(),
            method: "PUT".into(),
            template: "api/units/{unit}".into(),
            uri: "api/units/1".into(),
            body: Some(json!({"name": "parity name"})),
            ..Default::default()
        };
        let mut specs = vec![happy];
        let line = r#"{"method":"PUT","route":"api/units/{unit}","values":{"unit":"7"},"query":{},"body":{"name":"Block A","api_token":"x"},"status":200}"#;
        add_from(&mut specs, &format!("{line}\n{line}\n"), 5);
        assert_eq!(specs.len(), 2);
        let rec = specs.iter().find(|s| s.case.starts_with("rec-")).unwrap();
        assert_eq!(rec.uri, "api/units/7");
        assert_eq!(rec.body, Some(json!({"name": "Block A"})));
    }
}
