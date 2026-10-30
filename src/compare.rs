//! Compares what the old and new app did for the same request.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::glob;
use crate::plan::Spec;

/// Worst first; the report is sorted by this.
pub const VERDICTS: [(&str, &str); 8] = [
    (
        "new-broken",
        "New app fails (5xx or crash) where the old one answered",
    ),
    ("status", "Different status code"),
    (
        "effects",
        "Different side effects (rows written, jobs, mail, notifications, broadcasts, HTTP calls)",
    ),
    ("body", "Different response body"),
    ("minor", "Only types (\"1\" vs 1) or list order differ"),
    ("new-fixed", "Old app failed, new app answers"),
    ("both-fail", "Both apps fail"),
    ("same", "Same"),
];

fn happy() -> String {
    "happy".into()
}

impl Outcome {
    /// "METHOD uri", with the case when it is not the happy one.
    pub fn name(&self) -> String {
        if self.case == "happy" {
            self.key.clone()
        } else {
            format!("{} [{}]", self.key, self.case)
        }
    }
}

pub fn rank(verdict: &str) -> usize {
    VERDICTS
        .iter()
        .position(|(v, _)| *v == verdict)
        .unwrap_or(VERDICTS.len())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub id: String,
    pub key: String,
    #[serde(default = "happy")]
    pub case: String,
    pub uri: String,
    pub verdict: String,
    pub old_status: Value,
    pub new_status: Value,
    pub notes: Vec<String>,
}

pub struct Rules<'a> {
    pub ignore: &'a [String],
    pub unordered: bool,
    /// Side-effect kinds one of the apps could not record (a Laravel internal is missing): never compared.
    pub not_compared: &'a [String],
}

impl Rules<'_> {
    fn ignored_key(&self, key: &str) -> bool {
        self.ignore
            .iter()
            .any(|p| !p.starts_with('$') && glob::matches(key, p))
    }

    fn ignored_path(&self, path: &str) -> bool {
        static INDEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\d+\]").unwrap());
        let generic = INDEX.replace_all(path, "[*]");
        self.ignore
            .iter()
            .any(|p| p.starts_with('$') && (glob::matches(path, p) || glob::matches(&generic, p)))
    }
}

/// The row with each text (an app URL, a host name) replaced by <masked>, longest first.
pub fn mask(row: &Value, texts: &[String]) -> Value {
    let mut texts: Vec<String> = texts
        .iter()
        .map(|t| t.trim_end_matches('/').to_string())
        .filter(|t| t.len() > 3)
        .collect();
    texts.sort_by_key(|t| std::cmp::Reverse(t.len()));
    texts.dedup();
    if texts.is_empty() {
        return row.clone();
    }
    let mut dumped = row.to_string();
    for text in texts {
        let encoded = serde_json::to_string(&text).unwrap();
        dumped = dumped.replace(&encoded[1..encoded.len() - 1], "<masked>");
    }
    serde_json::from_str(&dumped).unwrap_or_else(|_| row.clone())
}

fn failed(row: &Value) -> bool {
    row["status"].as_i64().is_none_or(|s| s >= 500)
}

/// Differences as (kind, path, old, new); kind is value, missing, added, type or order.
pub fn compare_json(
    a: &Value,
    b: &Value,
    rules: &Rules,
    path: &str,
    out: &mut Vec<(String, String, Value, Value)>,
) {
    const LIMIT: usize = 25;
    if out.len() >= LIMIT {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            // an exception rendered in debug mode: where it was thrown moves between framework versions
            let debug = x.contains_key("exception") && y.contains_key("exception");
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                if (debug && ["file", "line", "trace"].contains(&key.as_str()))
                    || rules.ignored_key(key)
                {
                    continue;
                }
                let here = format!("{path}.{key}");
                if rules.ignored_path(&here) {
                    continue;
                }
                match (x.get(key), y.get(key)) {
                    (Some(v), None) => out.push(("missing".into(), here, v.clone(), Value::Null)),
                    (None, Some(v)) => out.push(("added".into(), here, Value::Null, v.clone())),
                    (Some(v), Some(w)) => compare_json(v, w, rules, &here, out),
                    _ => {}
                }
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if rules.unordered && x.len() == y.len() && x != y {
                let strip = |v: &Value| canonical(&without(v, rules));
                let (mut xs, mut ys): (Vec<String>, Vec<String>) =
                    (x.iter().map(strip).collect(), y.iter().map(strip).collect());
                let same_order = xs == ys;
                xs.sort();
                ys.sort();
                if xs == ys && !same_order {
                    out.push(("order".into(), path.into(), Value::Null, Value::Null));
                    return;
                }
            }
            if x.len() != y.len() {
                out.push((
                    "value".into(),
                    format!("{path}.length"),
                    json!(x.len()),
                    json!(y.len()),
                ));
            }
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                compare_json(v, w, rules, &format!("{path}[{i}]"), out);
                if out.len() >= LIMIT {
                    break;
                }
            }
        }
        _ if a != b => {
            if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64())
                && (x - y).abs() < 1e-9
                && a.is_f64()
                && b.is_f64()
            {
                return;
            }
            let same_text =
                scalar_text(a) == scalar_text(b) || (number(a).is_some() && number(a) == number(b));
            out.push((
                if same_text { "type" } else { "value" }.into(),
                path.into(),
                a.clone(),
                b.clone(),
            ));
        }
        _ => {}
    }
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.trim().is_empty() => s.trim().parse().ok(),
        _ => None,
    }
}

/// Sorted keys, so two equal objects print the same.
fn canonical(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .map(|k| format!("{:?}:{}", k, canonical(&map[*k])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(list) => format!(
            "[{}]",
            list.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

fn without(v: &Value, rules: &Rules) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !rules.ignored_key(k))
                .map(|(k, v)| (k.clone(), without(v, rules)))
                .collect(),
        ),
        Value::Array(list) => Value::Array(list.iter().map(|v| without(v, rules)).collect()),
        other => other.clone(),
    }
}

static WRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)^\s*(insert(?:\s+ignore)?\s+into|replace\s+into|update|delete\s+from)\s+[`"]?(\w+)"#,
    )
    .unwrap()
});

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|l| l.iter().map(scalar_text).collect())
        .unwrap_or_default()
}

/// The side effects in a comparable form: kind -> sorted list.
pub fn effect_summary(row: &Value) -> Vec<(&'static str, Vec<String>)> {
    let effects = &row["effects"];
    let mut writes: Vec<String> = effects["writes"]
        .as_array()
        .map(|l| {
            l.iter()
                .map(|w| {
                    let sql = w["sql"].as_str().unwrap_or_default();
                    WRITE
                        .captures(sql)
                        .map(|c| {
                            format!(
                                "{} {}",
                                c[1].split_whitespace().next().unwrap().to_lowercase(),
                                &c[2]
                            )
                        })
                        .unwrap_or_else(|| sql.chars().take(60).collect())
                })
                .collect()
        })
        .unwrap_or_default();
    let list = |key: &str, f: &dyn Fn(&Value) -> String| -> Vec<String> {
        let mut out: Vec<String> = effects[key]
            .as_array()
            .map(|l| l.iter().map(f).collect())
            .unwrap_or_default();
        out.sort();
        out
    };
    writes.sort();
    let mut jobs = strings(&effects["jobs"]);
    jobs.sort();
    let mut http = strings(&effects["http"]);
    http.sort();
    vec![
        ("writes", writes),
        ("jobs", jobs),
        (
            "mails",
            list("mails", &|m| m["class"].as_str().unwrap_or_default().into()),
        ),
        (
            "notifications",
            list("notifications", &|n| {
                format!(
                    "{} -> {}",
                    scalar_text(&n["class"]),
                    scalar_text(&n["notifiable"])
                )
            }),
        ),
        (
            "broadcasts",
            list("broadcasts", &|b| {
                format!(
                    "{} on {}",
                    scalar_text(&b["event"]),
                    strings(&b["channels"]).join(",")
                )
            }),
        ),
        ("http", http),
    ]
}

fn minus(a: &[String], b: &[String]) -> Vec<String> {
    let mut rest = b.to_vec();
    let mut left = vec![];
    for item in a {
        if let Some(i) = rest.iter().position(|r| r == item) {
            rest.remove(i);
        } else {
            left.push(item.clone());
        }
    }
    left
}

fn message(row: &Value) -> String {
    if let Some(e) = row["error"].as_str() {
        return e.to_string();
    }
    if let Some(m) = row["json"]["message"].as_str().filter(|m| !m.is_empty()) {
        return match row["json"]["exception"].as_str() {
            Some(e) => format!("{e}: {m}"),
            None => m.to_string(),
        };
    }
    row["effects"]["logs"]
        .get(0)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn short(v: &Value) -> String {
    let text = v.to_string();
    if text.chars().count() <= 120 {
        text
    } else {
        format!("{}...", text.chars().take(117).collect::<String>())
    }
}

fn describe(kind: &str, path: &str, a: &Value, b: &Value) -> String {
    match kind {
        "missing" => format!("{path} is missing in new (old: {})", short(a)),
        "added" => format!("{path} is new (new: {})", short(b)),
        "order" => format!("{path} same items, different order"),
        "type" => format!("{path} type only: {} -> {}", short(a), short(b)),
        _ => format!("{path}: {} -> {}", short(a), short(b)),
    }
}

fn filename(row: &Value) -> String {
    static NAME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"filename\*?=(?:UTF-8'')?"?([^";]+)"#).unwrap());
    static TIME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\d{4}[-_]?\d{2}[-_]?\d{2}([ _T-]?\d{2}[:_-]?\d{2}([:_-]?\d{2})?)?|\d{10,}")
            .unwrap()
    });
    // names often carry a date or time; compare the rest
    NAME.captures(row["disposition"].as_str().unwrap_or_default())
        .map(|c| TIME.replace_all(&c[1], "<time>").into_owned())
        .unwrap_or_default()
}

fn media(row: &Value) -> &str {
    row["content_type"]
        .as_str()
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
}

pub fn compare(spec: &Spec, old: Option<&Value>, new: Option<&Value>, rules: &Rules) -> Outcome {
    let mut outcome = Outcome {
        id: spec.id.clone(),
        key: spec.key.clone(),
        case: spec.case.clone(),
        uri: spec.uri.clone(),
        verdict: String::new(),
        old_status: Value::Null,
        new_status: Value::Null,
        notes: vec![],
    };
    let (Some(old), Some(new)) = (old, new) else {
        outcome.verdict = "both-fail".into();
        let side = if old.is_none() { "old" } else { "new" };
        outcome
            .notes
            .push(format!("no result from the {side} app (runner crashed?)"));
        return outcome;
    };
    outcome.old_status = old["status"].clone();
    outcome.new_status = new["status"].clone();
    match (failed(old), failed(new)) {
        (true, true) => {
            outcome.verdict = "both-fail".into();
            outcome.notes = vec![
                format!("old: {}", message(old)),
                format!("new: {}", message(new)),
            ];
            return outcome;
        }
        (false, true) => {
            outcome.verdict = "new-broken".into();
            outcome.notes.push(format!("new: {}", message(new)));
            return outcome;
        }
        (true, false) => {
            outcome.verdict = "new-fixed".into();
            outcome.notes.push(format!("old: {}", message(old)));
            return outcome;
        }
        _ => {}
    }

    let mut verdicts = vec![];
    if old["status"] != new["status"] {
        verdicts.push("status");
        outcome
            .notes
            .push(format!("status {} -> {}", old["status"], new["status"]));
        for (side, row) in [("old", old), ("new", new)] {
            if row["status"].as_i64().is_some_and(|s| s >= 400) && !message(row).is_empty() {
                outcome.notes.push(format!("{side}: {}", message(row)));
            }
        }
    }

    let (a, b) = (effect_summary(old), effect_summary(new));
    let mut effect_notes = vec![];
    for ((kind, x), (_, y)) in a.iter().zip(&b) {
        if rules.not_compared.iter().any(|k| k == kind) {
            continue;
        }
        if x != y {
            effect_notes.push(format!(
                "{kind}: only old {:?}; only new {:?}",
                minus(x, y),
                minus(y, x)
            ));
        }
    }
    if !effect_notes.is_empty() {
        verdicts.push("effects");
        outcome.notes.extend(effect_notes);
    }

    if old["kind"] == "json" && new["kind"] == "json" {
        let mut diffs = vec![];
        compare_json(&old["json"], &new["json"], rules, "$", &mut diffs);
        let major = diffs.iter().any(|d| d.0 != "type" && d.0 != "order");
        verdicts.push(if major {
            "body"
        } else if diffs.is_empty() {
            "same"
        } else {
            "minor"
        });
        for (kind, path, x, y) in diffs.iter().take(12) {
            outcome.notes.push(describe(kind, path, x, y));
        }
    } else {
        let mut mismatch = vec![];
        // a PDF printed by the code and a PDF returned as a response are the same answer; files are not compared
        // byte for byte (they carry creation dates)
        let files = ["printed", "text", "stream", "file"];
        let is_file = |r: &Value| files.contains(&r["kind"].as_str().unwrap_or_default());
        let same_file =
            is_file(old) && is_file(new) && media(old) == media(new) && media(old) != "text/html";
        if old["kind"] != new["kind"] && !same_file {
            mismatch.push(format!("kind {} -> {}", old["kind"], new["kind"]));
        }
        if media(old) != media(new) {
            mismatch.push(format!("content type {} -> {}", media(old), media(new)));
        }
        // printed files set their name with header(), which a CLI run cannot see
        if old["kind"] != "printed" && new["kind"] != "printed" && filename(old) != filename(new) {
            mismatch.push(format!("file name {} -> {}", filename(old), filename(new)));
        }
        if old["kind"] == "text"
            && new["kind"] == "text"
            && !same_file
            && old["sha1"] != new["sha1"]
        {
            mismatch.push(format!(
                "text differs ({} -> {} bytes)",
                old["size"], new["size"]
            ));
        }
        verdicts.push(if mismatch.is_empty() { "same" } else { "body" });
        outcome.notes.extend(mismatch);
    }

    outcome.verdict = verdicts
        .into_iter()
        .min_by_key(|v| rank(v))
        .unwrap_or("same")
        .to_string();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_types_and_order() {
        let ignore = vec!["created_at".to_string(), "$.meta.links[*].page".to_string()];
        let rules = Rules {
            ignore: &ignore,
            unordered: true,
            not_compared: &[],
        };
        let mut out = vec![];
        compare_json(
            &json!({"a": "1", "created_at": 1, "meta": {"links": [{"url": "x"}]}, "l": [1, 2]}),
            &json!({"a": 1, "created_at": 2, "meta": {"links": [{"url": "x", "page": null}]}, "l": [2, 1]}),
            &rules,
            "$",
            &mut out,
        );
        let kinds: Vec<&str> = out.iter().map(|d| d.0.as_str()).collect();
        assert_eq!(kinds, vec!["type", "order"]);
    }
}
