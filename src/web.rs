//! The web app's side, read by codemap (Vue 2, Vue 3, Nuxt, React ...): every HTTP call it makes, tied to the API
//! route that serves it. Two uses:
//! - the fields each call sends, so a request body looks like the one the web app really sends
//! - the calls the upgrade breaks: run once for the old web against the old API (the baseline) and once for the web
//!   that ships against the new API; what only the second run finds is caused by the upgrade.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde_json::{Value, json};

use crate::codemap::Codemap;
use crate::config::Config;
use crate::ops::{Res, read, write};
use crate::say;

/// Findings that mean a call will fail or show wrong data. Placeholders, external URLs and query-parameter notes
/// are left out; "unresolved" is kept apart (codemap could not tell which route).
const BREAKING: [&str; 7] = [
    "missing",
    "method_mismatch",
    "missing_required",
    "unknown_field",
    "fields_not_returned",
    "possible_rename",
    "variant_without_route",
];

/// Every call of one web app, against one API.
pub fn scan(web: &Path, api: &Path, id: &str) -> Res<Vec<Value>> {
    let mut codemap = Codemap::start_linked(web, id, &format!("backend={}", api.display()))?;
    let mut calls = vec![];
    let mut cursor: Option<String> = None;
    loop {
        let mut args = json!({"status": "all", "detail": "full", "limit": 500});
        if let Some(c) = &cursor {
            args["cursor"] = json!(c);
        }
        let answer = codemap.call("api_usage", args)?;
        calls.extend(
            answer["data"]["calls"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
        cursor = answer["meta"]["next_cursor"].as_str().map(String::from);
        if cursor.is_none() {
            return Ok(calls);
        }
    }
}

/// The kinds of trouble one call has (a matched call can still send fields the route rejects).
pub fn problems(call: &Value) -> Vec<String> {
    let mut kinds = vec![];
    if let Some(status) = call["status"].as_str()
        && BREAKING.contains(&status)
    {
        kinds.push(status.to_string());
    }
    for key in [
        "missing_required",
        "unknown_field",
        "fields_not_returned",
        "possible_rename",
    ] {
        if call[key].as_array().is_some_and(|a| !a.is_empty()) && !kinds.iter().any(|k| k == key) {
            kinds.push(key.to_string());
        }
    }
    kinds
}

/// What makes two findings the same, across two web versions (the file and line may move): the call and the kind.
fn signature(call: &Value, kind: &str) -> String {
    let detail = call.get(kind).map(|v| v.to_string()).unwrap_or_default();
    format!("{kind}|{}|{detail}", call["call"].as_str().unwrap_or(""))
}

/// "POST api/x/{y}" -> fields the web app sends to it (all callers together).
pub fn sends_by_route(calls: &[Value]) -> HashMap<String, BTreeSet<String>> {
    let mut found: HashMap<String, BTreeSet<String>> = HashMap::new();
    for call in calls {
        let Some(route) = call["route"].as_str() else {
            continue;
        };
        let Some(sends) = call["payload_evidence"]["sends"].as_array() else {
            continue;
        };
        let (methods, uri) = route.split_once(' ').unwrap_or(("", route));
        for method in methods.split('|').filter(|m| *m != "HEAD") {
            found
                .entry(format!("{method} {uri}"))
                .or_default()
                .extend(sends.iter().filter_map(Value::as_str).map(String::from));
        }
    }
    found
}

fn web_repos(config: &Config) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let old = config.web_old.clone()?;
    let new = config.web_new.clone().unwrap_or_else(|| old.clone());
    Some((old, new))
}

/// Run both scans and keep what the upgrade changed; writes web.json in the run folder.
pub fn check(config: &Config) -> Res<Value> {
    let Some((old_web, new_web)) = web_repos(config) else {
        return Err("no [web] old = \"path\" in the pair file".into());
    };
    say(&format!(
        "Reading the web app's API calls with codemap: {} against the old API ...",
        old_web.display()
    ));
    let before = scan(
        &old_web,
        &config.old.repo,
        &format!("lp-{}-web-before", config.name),
    )?;
    say(&format!(
        "... and {} against the new API ...",
        new_web.display()
    ));
    let after = scan(
        &new_web,
        &config.new.repo,
        &format!("lp-{}-web-after", config.name),
    )?;

    // codemap reads route files; a route a package registers at runtime (Passport's oauth/token ...) is not in
    // them. A "missing" call the new app does answer at runtime is no finding.
    let runtime = runtime_routes(config).unwrap_or_default();
    let after: Vec<Value> = after
        .into_iter()
        .filter(|c| {
            !(matches!(c["status"].as_str(), Some("missing" | "method_mismatch"))
                && answered(c, &runtime))
        })
        .collect();

    let known: BTreeSet<String> = before
        .iter()
        .flat_map(|c| problems(c).into_iter().map(move |k| signature(c, &k)))
        .collect();
    let now: BTreeSet<String> = after
        .iter()
        .flat_map(|c| problems(c).into_iter().map(move |k| signature(c, &k)))
        .collect();
    let mut broken = vec![];
    for call in &after {
        for kind in problems(call) {
            if !known.contains(&signature(call, &kind)) {
                broken.push(json!({
                    "kind": kind, "at": call["at"], "call": call["call"], "route": call["route"],
                    "detail": call.get(&kind).cloned().unwrap_or(Value::Null),
                    "path_exists_as": call.get("path_exists_as").cloned().unwrap_or(Value::Null),
                    "closest": call.get("closest").cloned().unwrap_or(Value::Null),
                    "accepts": call["payload_evidence"]["accepts"].clone(),
                    "accepts_at": call["payload_evidence"]["accepts_at"].clone(),
                }));
            }
        }
    }
    let fixed = known.difference(&now).count();
    let count = |calls: &[Value]| -> BTreeMap<String, usize> {
        let mut c = BTreeMap::new();
        for call in calls {
            for k in problems(call) {
                *c.entry(k).or_default() += 1;
            }
        }
        c
    };
    let result = json!({
        "old_web": old_web, "new_web": new_web,
        "calls_before": before.len(), "calls_after": after.len(),
        "problems_before": count(&before), "problems_after": count(&after),
        "broken_by_upgrade": broken, "fixed_by_upgrade": fixed,
        "unresolved_after": after.iter().filter(|c| c["status"] == "unresolved").count(),
    });
    write(&config.work.join("web.json"), &result)?;
    say(&format!(
        "Web calls: {} new problems caused by the upgrade, {} problems the upgrade fixed ({} calls with a finding before, {} after).",
        result["broken_by_upgrade"].as_array().map_or(0, Vec::len),
        fixed,
        before.len(),
        after.len()
    ));
    Ok(result)
}

/// A route parameter, {id} or {id?}.
static ROUTE_PARAM: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\{[^}]+\}").unwrap());

/// (method, uri pattern) of every route the new app has once booted.
fn runtime_routes(config: &Config) -> Res<Vec<(String, regex::Regex)>> {
    let rows = crate::runner::run_job(config, &config.new, &json!({"mode": "routes"}), 0)?;
    let mut found = vec![];
    for route in rows
        .iter()
        .flat_map(|r| r["routes"].as_array().cloned().unwrap_or_default())
    {
        let uri = route["uri"].as_str().unwrap_or_default().trim_matches('/');
        let pattern = format!(
            "^/?{}/?$",
            regex::escape(uri)
                .replace(r"\{", "{")
                .replace(r"\}", "}")
                .replace(r"\?", "?")
        );
        let pattern = ROUTE_PARAM.replace_all(&pattern, "[^/]+");
        let Ok(re) = regex::Regex::new(&pattern) else {
            continue;
        };
        for method in route["methods"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            found.push((method.to_string(), re.clone()));
        }
    }
    Ok(found)
}

/// Whether the new app answers this call (with or without the API's /api prefix).
fn answered(call: &Value, runtime: &[(String, regex::Regex)]) -> bool {
    let Some((method, path)) = call["call"].as_str().and_then(|c| c.split_once(' ')) else {
        return false;
    };
    let path = path.replace("{}", "x");
    let with_prefix = format!("/api{path}");
    runtime
        .iter()
        .any(|(m, re)| m == method && (re.is_match(&path) || re.is_match(&with_prefix)))
}

/// The fields the shipping web app sends per route, read once per plan (empty without [web]).
pub fn sends(config: &Config) -> HashMap<String, BTreeSet<String>> {
    let Some((_, new_web)) = web_repos(config) else {
        return HashMap::new();
    };
    match scan(
        &new_web,
        &config.new.repo,
        &format!("lp-{}-web-after", config.name),
    ) {
        Ok(calls) => sends_by_route(&calls),
        Err(e) => {
            say(&format!(
                "  web app not read ({e}); bodies come from validation rules only"
            ));
            HashMap::new()
        }
    }
}

/// The last web check, for the report and the fix list.
/// None without [web] in the pair file, even when an earlier run left a web.json.
pub fn last(config: &Config) -> Option<Value> {
    config.web_old.as_ref()?;
    read(&config.work.join("web.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_of_a_call() {
        assert_eq!(problems(&json!({"status": "missing"})), vec!["missing"]);
        assert!(problems(&json!({"status": "matched"})).is_empty());
        assert!(problems(&json!({"status": "unresolved"})).is_empty());
        assert!(problems(&json!({"status": "external", "unknown_field": []})).is_empty());
        // a matched call can still send fields the route rejects; a status already counted is not repeated
        assert_eq!(
            problems(&json!({
                "status": "missing_required",
                "missing_required": ["name"],
                "unknown_field": ["nmae"],
                "possible_rename": [{"from": "nmae", "to": "name"}],
                "fields_not_returned": [],
            })),
            vec!["missing_required", "unknown_field", "possible_rename"]
        );
        let call = json!({"call": "POST /shops", "at": "a.vue:1", "unknown_field": ["x"]});
        let moved = json!({"call": "POST /shops", "at": "b.vue:9", "unknown_field": ["x"]});
        assert_eq!(
            signature(&call, "unknown_field"),
            signature(&moved, "unknown_field")
        );
        assert_eq!(
            signature(&call, "unknown_field"),
            "unknown_field|POST /shops|[\"x\"]"
        );
        assert_eq!(signature(&call, "missing"), "missing|POST /shops|");
        assert_ne!(
            signature(&call, "unknown_field"),
            signature(
                &json!({"call": "POST /shops", "unknown_field": ["y"]}),
                "unknown_field"
            )
        );
    }

    #[test]
    fn fields_sent_per_route() {
        let calls = vec![
            json!({"route": "GET|HEAD api/shops", "payload_evidence": {"sends": ["page"]}}),
            json!({"route": "PUT|PATCH api/shops/{shop}", "payload_evidence": {"sends": ["name", 1]}}),
            json!({"route": "PUT|PATCH api/shops/{shop}", "payload_evidence": {"sends": ["email", "name"]}}),
            json!({"route": "POST api/orders", "payload_evidence": {}}),
            json!({"payload_evidence": {"sends": ["lost"]}}),
            json!({"route": "orphan", "payload_evidence": {"sends": ["q"]}}),
        ];
        let found = sends_by_route(&calls);
        let names = |k: &str| found[k].iter().cloned().collect::<Vec<_>>();
        assert_eq!(names("GET api/shops"), vec!["page"]);
        assert_eq!(names("PUT api/shops/{shop}"), vec!["email", "name"]);
        assert_eq!(names("PATCH api/shops/{shop}"), vec!["email", "name"]);
        assert_eq!(names(" orphan"), vec!["q"]);
        assert!(!found.contains_key("HEAD api/shops"));
        assert!(!found.contains_key("POST api/orders"));
        assert_eq!(found.len(), 4);
    }

    #[test]
    fn answered_at_runtime() {
        let runtime = vec![
            (
                "POST".to_string(),
                regex::Regex::new(r"^/?oauth/token/?$").unwrap(),
            ),
            (
                "GET".to_string(),
                regex::Regex::new(r"^/?api/shops/[^/]+/?$").unwrap(),
            ),
        ];
        assert!(answered(&json!({"call": "POST /oauth/token"}), &runtime));
        assert!(answered(&json!({"call": "GET /shops/{}"}), &runtime));
        assert!(answered(&json!({"call": "GET /api/shops/7/"}), &runtime));
        assert!(!answered(&json!({"call": "DELETE /shops/{}"}), &runtime));
        assert!(!answered(
            &json!({"call": "GET /shops/{}/orders"}),
            &runtime
        ));
        assert!(!answered(&json!({"call": "GET"}), &runtime));
        assert!(!answered(&json!({}), &runtime));
        assert!(!answered(&json!({"call": "GET /shops/1"}), &[]));
    }

    #[test]
    fn last_needs_web_in_the_pair_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for repo in ["shop-old", "shop", "shop-web"] {
            std::fs::create_dir_all(root.join(repo)).unwrap();
        }
        let pair = root.join("shop.toml");
        let text = format!(
            "work = {:?}\n[old]\nrepo = {:?}\n[new]\nrepo = {:?}\n[database]\nsource = \"shop\"\n",
            root.join("runs").display().to_string(),
            root.join("shop-old").display().to_string(),
            root.join("shop").display().to_string()
        );
        std::fs::write(&pair, &text).unwrap();
        let config = crate::config::load(&pair).unwrap();
        std::fs::create_dir_all(&config.work).unwrap();
        write(
            &config.work.join("web.json"),
            &json!({"fixed_by_upgrade": 2}),
        )
        .unwrap();
        assert!(last(&config).is_none());
        assert!(web_repos(&config).is_none());
        assert!(sends(&config).is_empty());
        assert!(check(&config).unwrap_err().contains("no [web] old"));

        let web = root.join("shop-web").display().to_string();
        std::fs::write(&pair, format!("{text}[web]\nold = {web:?}\n")).unwrap();
        let config = crate::config::load(&pair).unwrap();
        assert_eq!(last(&config).unwrap()["fixed_by_upgrade"], 2);
        let (old, new) = web_repos(&config).unwrap();
        assert_eq!(old, new);

        std::fs::write(
            &pair,
            format!("{text}[web]\nold = {web:?}\nnew = \"/srv/shop-web-new\"\n"),
        )
        .unwrap();
        let config = crate::config::load(&pair).unwrap();
        let (old, new) = web_repos(&config).unwrap();
        assert_eq!(old, root.join("shop-web"));
        assert_eq!(new, Path::new("/srv/shop-web-new"));
    }
}
