//! Learning from the old app's refusals. A made-up body often misses fields: rules the tool could not read (written in
//! a base class, built in a loop) or fields the controller checks by hand. The old app says which ones in its
//! validation errors ("The posting date field is required.", "The tenant.id field is required."), so the tool fills
//! exactly those, sends the request again, and repeats while each round gets further. Only requests that should pass
//! (happy, and real ones from a log or the recorder) are filled; the validation cases keep their missing fields.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::compare::rejected;
use crate::config::Config;
use crate::ops::{Res, jsonl, read, write};
use crate::plan::{Sampler, Spec};
use crate::rules::{Field, valid_value};
use crate::say;

/// Set `a.b.0.c` inside a JSON value, making objects and lists on the way.
pub fn set_path(root: &mut Value, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut at = root;
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let next_is_index = parts.get(i + 1).is_some_and(|p| p.parse::<usize>().is_ok());
        let empty = if next_is_index { json!([]) } else { json!({}) };
        if let Ok(index) = part.parse::<usize>()
            && at.is_array()
        {
            let list = at.as_array_mut().unwrap();
            while list.len() <= index {
                list.push(Value::Null);
            }
            if last {
                list[index] = value;
                return;
            }
            if !(list[index].is_object() || list[index].is_array()) {
                list[index] = empty;
            }
            at = &mut list[index];
            continue;
        }
        if !at.is_object() {
            *at = json!({});
        }
        let map = at.as_object_mut().unwrap();
        if last {
            map.insert(part.to_string(), value);
            return;
        }
        let slot = map.entry(part.to_string()).or_insert(Value::Null);
        if !(slot.is_object() || slot.is_array()) {
            *slot = empty;
        }
        at = slot;
    }
}

/// The value at `a.b.0.c`, if there is one.
pub fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(root, |at, part| match at {
        Value::Array(list) => list.get(part.parse::<usize>().ok()?),
        Value::Object(map) => map.get(part),
        _ => None,
    })
}

/// A PHP date() format ("Y-m-d\TH:i:sP") applied to the run's clock.
fn php_date(format: &str, clock: &str) -> Option<String> {
    let time = chrono::NaiveDateTime::parse_from_str(clock, "%Y-%m-%d %H:%M:%S").ok()?;
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        let piece = match c {
            '\\' => chars.next().map(String::from).unwrap_or_default(),
            'Y' => time.format("%Y").to_string(),
            'y' => time.format("%y").to_string(),
            'm' => time.format("%m").to_string(),
            'n' => time.format("%-m").to_string(),
            'd' => time.format("%d").to_string(),
            'j' => time.format("%-d").to_string(),
            'H' => time.format("%H").to_string(),
            'G' => time.format("%-H").to_string(),
            'h' => time.format("%I").to_string(),
            'i' => time.format("%M").to_string(),
            's' => time.format("%S").to_string(),
            'A' => time.format("%p").to_string(),
            'M' => time.format("%b").to_string(),
            'F' => time.format("%B").to_string(),
            'D' => time.format("%a").to_string(),
            'P' => chrono::Local::now().format("%:z").to_string(),
            'O' => chrono::Local::now().format("%z").to_string(),
            'u' => "000000".into(),
            'v' => "000".into(),
            other => other.to_string(),
        };
        out.push_str(&piece);
    }
    Some(out)
}

/// What a validation message asks for, as rules the value maker understands.
fn rules_from(name: &str, message: &str) -> Vec<String> {
    static MAX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:greater than|more than|exceed|max(?:imum)?(?: of)?) (\d+) char")
            .unwrap()
    });
    static MIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)at least (\d+) char").unwrap());
    let said = message.to_lowercase();
    let leaf = name.rsplit('.').next().unwrap_or(name).to_lowercase();
    let mut rules = vec![];
    if said.contains("integer") || said.contains("must be a number") || said.contains("numeric") {
        rules.push("integer".into());
    } else if said.contains("true or false")
        || said.contains("boolean")
        || said.contains("must be accepted")
    {
        rules.push("boolean".into());
    } else if said.contains("email") || leaf.contains("email") {
        rules.push("email".into());
    } else if said.contains("must be an array") || said.contains("items") {
        rules.push("array".into());
    } else if said.contains("date")
        || leaf.ends_with("date")
        || leaf.ends_with("_at")
        || leaf.starts_with("date")
    {
        rules.push("date".into());
    } else if said.contains("url") {
        rules.push("url".into());
    } else if [
        "amount", "qty", "quantity", "price", "rate", "total", "count", "year", "month", "day",
    ]
    .iter()
    .any(|w| leaf == *w || leaf.ends_with(&format!("_{w}")))
    {
        rules.push("numeric".into());
    }
    if let Some(c) = MAX.captures(message) {
        rules.push(format!("max:{}", &c[1]));
    }
    if let Some(c) = MIN.captures(message) {
        rules.push(format!("min:{}", &c[1]));
    }
    rules
}

/// A value for one field the old app asked for, or None when the tool cannot make one (a file upload).
fn value_for(
    name: &str,
    message: &str,
    template: &str,
    clock: &str,
    sampler: &mut Sampler,
) -> Option<Value> {
    static FORMAT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)does not match the format (.+?)\.?$").unwrap());
    let said = message.to_lowercase();
    let leaf = name.rsplit('.').next().unwrap_or(name);
    if said.contains("upload")
        || said.contains("file")
        || said.contains("image")
        || said.contains("attachment")
    {
        return None;
    }
    if let Some(c) = FORMAT.captures(message) {
        return php_date(c[1].trim(), clock).map(Value::from);
    }
    // an id: a real row of the table the name points at ("tenant.id", "unit_id", "status_id", "selected unit is invalid")
    let parts: Vec<&str> = name
        .split('.')
        .filter(|p| p.parse::<usize>().is_err())
        .collect();
    let id_like = leaf == "id" || leaf.ends_with("_id") || said.contains("selected");
    if id_like {
        let owner = if leaf == "id" {
            parts.iter().rev().nth(1).copied().unwrap_or(leaf)
        } else {
            leaf
        };
        let param = if owner.ends_with("_id") {
            owner.to_string()
        } else {
            format!("{owner}_id")
        };
        if let (Some(v), _) = sampler.by_name(template, &param) {
            return Some(v.parse::<i64>().map(Value::from).unwrap_or(Value::from(v)));
        }
        if leaf == "id" || leaf.ends_with("_id") {
            return Some(json!(1));
        }
    }
    let rules = rules_from(name, message);
    // a list: one empty item, so the next answer names the item's own fields ("paid_items.0.amount is required")
    if rules.iter().any(|r| r == "array") {
        return Some(json!([{}]));
    }
    let field = Field {
        name: leaf.to_string(),
        rules,
    };
    let mut none = |_: &str, _: &str| None;
    valid_value(&field, clock, &mut none)
}

/// One round: fill what the old app asked for in the requests that should have passed. Returns the changed ids.
fn fill(config: &Config, specs: &mut [Spec], clock: &str) -> Res<Vec<String>> {
    let old = jsonl(&config.work.join("results-old.jsonl"));
    let mut sampler: Option<Sampler> = None;
    let mut changed = vec![];
    // what each route learned, so its without-<field> cases get it too
    let mut learned_by_key: Vec<(String, Vec<(String, Value)>)> = vec![];
    for spec in specs.iter_mut() {
        let wants_pass =
            spec.case == "happy" || spec.case.starts_with("log-") || spec.case.starts_with("rec-");
        if !wants_pass || spec.skip.is_some() {
            continue;
        }
        let Some(row) = old.get(&spec.id) else {
            continue;
        };
        let Some(fields) = rejected(row) else {
            continue;
        };
        let sampler = match &mut sampler {
            Some(s) => s,
            None => sampler.insert(Sampler::new(config)?),
        };
        sampler.start_route();
        let in_query = spec.method == "GET";
        let mut added = vec![];
        for name in fields {
            // tried already and still refused with the same message: another value will not come from guessing
            let message = crate::compare::rejections(row)
                .into_iter()
                .find_map(|l| l.strip_prefix(&format!("{name}: ")).map(String::from))
                .unwrap_or_default();
            let tag = format!("{name}: {message}");
            let sent = if in_query {
                spec.query.as_ref().map(|q| Value::Object(q.clone()))
            } else {
                spec.body.clone()
            };
            // "The password confirmation does not match."
            let (name, value) = if message.to_lowercase().contains("confirmation") {
                match sent.as_ref().and_then(|t| get_path(t, &name)).cloned() {
                    Some(v) => (format!("{name}_confirmation"), v),
                    None => continue,
                }
            } else {
                match value_for(&name, &message, &spec.template, clock, sampler) {
                    Some(v) => (name, v),
                    None => continue,
                }
            };
            // the same value again would get the same refusal
            if sent.as_ref().and_then(|t| get_path(t, &name)) == Some(&value) {
                continue;
            }
            if !spec.learned.contains(&tag) {
                spec.learned.push(tag);
            }
            added.push((name, value));
        }
        if added.is_empty() {
            continue;
        }
        if in_query {
            let mut query = Value::Object(spec.query.clone().unwrap_or_default());
            for (name, value) in &added {
                set_path(&mut query, name, value.clone());
            }
            spec.query = query.as_object().cloned();
        } else {
            let mut body = match spec.body.clone() {
                Some(Value::Object(b)) => Value::Object(b),
                _ => json!({}),
            };
            for (name, value) in &added {
                set_path(&mut body, name, value.clone());
            }
            spec.made = Some(body.clone());
            spec.body = Some(body);
        }
        if spec.case == "happy" {
            learned_by_key.push((spec.key.clone(), added.clone()));
        }
        changed.push(spec.id.clone());
    }
    // the validation cases send the happy body minus one field: they get what it learned
    for spec in specs.iter_mut().filter(|s| s.case.starts_with("without-")) {
        let Some((_, added)) = learned_by_key.iter().find(|(k, _)| *k == spec.key) else {
            continue;
        };
        let mut body = spec.body.clone().unwrap_or(json!({}));
        for (name, value) in added {
            if !spec.without.contains(name) {
                set_path(&mut body, name, value.clone());
            }
        }
        spec.made = Some(body.clone());
        spec.body = Some(body);
        changed.push(spec.id.clone());
    }
    Ok(changed)
}

/// Rounds of: fill what the old app's validation errors name, send those requests again. After a run.
pub fn improve(config: &Config, workers: Option<usize>) -> Res<String> {
    let path = config.work.join("requests.json");
    let meta: Value = read(&config.work.join("run.json")).unwrap_or(json!({}));
    let clock = meta["clock"]
        .as_str()
        .unwrap_or("2026-01-01 09:00:00")
        .to_string();
    let mut total: HashSet<String> = HashSet::new();
    for round in 1..=config.learn {
        let mut specs: Vec<Spec> = read(&path).ok_or("no plan yet: run plan first")?;
        let changed = fill(config, &mut specs, &clock)?;
        if changed.is_empty() {
            break;
        }
        write(&path, &specs)?;
        let ids: HashSet<&str> = changed.iter().map(String::as_str).collect();
        let run: Vec<Spec> = specs
            .iter()
            .filter(|s| ids.contains(s.id.as_str()))
            .cloned()
            .collect();
        say(&format!(
            "Learning from the old app's validation errors (round {round} of {}): {} requests get the fields it asked for",
            config.learn,
            run.len()
        ));
        crate::ops::send(config, run, 0, true, false, false, workers)?;
        total.extend(changed);
    }
    if total.is_empty() {
        return Ok(
            "nothing to learn: no request that should pass was refused with validation errors"
                .into(),
        );
    }
    Ok(format!(
        "{} requests were sent again with the fields the old app's validation errors asked for",
        total.len()
    ))
}

/// Keep what earlier rounds learned when a new plan rebuilds a body from the rules.
pub fn carry(
    learned_from: &Spec,
    body: &mut Option<Value>,
    query: &mut Option<Map<String, Value>>,
) {
    for tag in &learned_from.learned {
        let Some((name, _)) = tag.split_once(": ") else {
            continue;
        };
        let names = [name.to_string(), format!("{name}_confirmation")];
        for name in names {
            if learned_from.method == "GET" {
                let source = learned_from
                    .query
                    .as_ref()
                    .map(|q| Value::Object(q.clone()));
                if let Some(v) = source.as_ref().and_then(|s| get_path(s, &name)).cloned() {
                    let mut target = Value::Object(query.clone().unwrap_or_default());
                    set_path(&mut target, &name, v);
                    *query = target.as_object().cloned();
                }
            } else if let Some(v) = learned_from
                .body
                .as_ref()
                .and_then(|b| get_path(b, &name))
                .cloned()
            {
                let target = body.get_or_insert_with(|| json!({}));
                set_path(target, &name, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_formats() {
        let mut v = json!({});
        set_path(&mut v, "tenant.id", json!(5));
        set_path(&mut v, "uploads.0.name", json!("a"));
        set_path(&mut v, "date", json!("x"));
        assert_eq!(
            v,
            json!({"tenant": {"id": 5}, "uploads": [{"name": "a"}], "date": "x"})
        );
        assert_eq!(get_path(&v, "uploads.0.name"), Some(&json!("a")));
        assert_eq!(
            php_date("Y-m-d", "2026-10-31 09:05:07").unwrap(),
            "2026-10-31"
        );
        assert!(
            php_date("Y-m-d\\TH:i:sP", "2026-10-31 09:05:07")
                .unwrap()
                .starts_with("2026-10-31T09:05:07")
        );
        assert_eq!(
            rules_from("received_amount", "The received amount field is required."),
            ["numeric"]
        );
        assert_eq!(
            rules_from("posting_date", "The posting date field is required."),
            ["date"]
        );
        assert_eq!(
            rules_from("name", "The name must not be greater than 5 characters."),
            ["max:5"]
        );
    }
}
