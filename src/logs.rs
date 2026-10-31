//! Real requests from a web server's access log (nginx / Apache "combined", or any line with `"GET /path HTTP/1.1"
//! 200`) as extra cases: the ids, filters, sorting and paging real users sent, instead of values the tool made up.
//! Only successful GETs (an access log has no request bodies); query values that look like secrets are dropped.

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
    let text = String::from_utf8_lossy(&text);
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
    for line in text.lines() {
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
        for (k, v) in &values {
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
    write(&path, &specs)?;
    let line = format!(
        "{lines} requests in the log, {matched} successful GETs on planned routes; added {count} cases on {routes_hit} routes \
         (at most {per_route} per route). Next: run (or rerun a route) to send them to both apps."
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
}
