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

    #[test]
    fn paths_make_and_find() {
        let mut v = json!({"name": "a", "items": "x"});
        // a scalar on the way becomes a list (next part is an index) or an object
        set_path(&mut v, "items.1.qty", json!(2));
        set_path(&mut v, "name.first", json!("b"));
        set_path(&mut v, "items.0", json!("c"));
        set_path(&mut v, "grid.0.0", json!(1));
        assert_eq!(
            v,
            json!({"name": {"first": "b"}, "items": ["c", {"qty": 2}], "grid": [[1]]})
        );
        set_path(&mut v, "items.1.price", json!(5));
        assert_eq!(v["items"][1], json!({"qty": 2, "price": 5}));
        let mut root = json!(null);
        set_path(&mut root, "tenant.id", json!(7));
        assert_eq!(root, json!({"tenant": {"id": 7}}));

        assert_eq!(get_path(&v, "items.1.qty"), Some(&json!(2)));
        assert_eq!(get_path(&v, "grid.0.0"), Some(&json!(1)));
        assert_eq!(get_path(&v, "items.5"), None);
        assert_eq!(get_path(&v, "items.x"), None);
        assert_eq!(get_path(&v, "name.first.more"), None);
        assert_eq!(get_path(&json!({"0": "zero"}), "0"), Some(&json!("zero")));
    }

    #[test]
    fn php_formats() {
        let clock = "2026-10-31 09:05:07";
        assert_eq!(php_date("d/m/Y H:i", clock).unwrap(), "31/10/2026 09:05");
        assert_eq!(
            php_date("j n y G", "2026-02-03 04:05:06").unwrap(),
            "3 2 26 4"
        );
        assert_eq!(
            php_date("h:i A", "2026-10-31 21:05:07").unwrap(),
            "09:05 PM"
        );
        assert_eq!(php_date("D, M F", clock).unwrap(), "Sat, Oct October");
        assert_eq!(php_date("H:i:s.u v", clock).unwrap(), "09:05:07.000000 000");
        // an escaped letter is kept as it is
        assert_eq!(php_date("\\Y\\m Y", clock).unwrap(), "Ym 2026");
        assert_eq!(php_date("Y\\", clock).unwrap(), "2026");
        assert!(php_date("O", clock).unwrap().len() == 5);
        assert!(php_date("Y-m-d", "31/10/2026").is_none());
    }

    #[test]
    fn rules_from_messages() {
        let r = |name: &str, message: &str| rules_from(name, message);
        assert_eq!(r("qty", "The qty must be an integer."), ["integer"]);
        assert_eq!(r("total", "The total must be a number."), ["integer"]);
        assert_eq!(
            r("active", "The active field must be true or false."),
            ["boolean"]
        );
        assert_eq!(r("terms", "The terms must be accepted."), ["boolean"]);
        assert_eq!(
            r("contact_email", "The contact email field is required."),
            ["email"]
        );
        assert_eq!(
            r("owner", "The owner must be a valid email address."),
            ["email"]
        );
        assert_eq!(r("lines", "The lines must be an array."), ["array"]);
        assert_eq!(
            r("paid_items", "The paid items field is required."),
            ["array"]
        );
        assert_eq!(
            r("shipped_at", "The shipped at field is required."),
            ["date"]
        );
        assert_eq!(r("date_from", "The date from field is required."), ["date"]);
        assert_eq!(r("site", "The site format is invalid URL."), ["url"]);
        assert_eq!(
            r(
                "lines.0.unit_price",
                "The lines.0.unit_price field is required."
            ),
            ["numeric"]
        );
        assert_eq!(r("year", "The year field is required."), ["numeric"]);
        // "rated" is not "rate"
        assert!(r("rated", "The rated field is required.").is_empty());
        assert!(r("name", "The name field is required.").is_empty());
        assert_eq!(
            r("password", "The password must be at least 8 characters."),
            ["min:8"]
        );
        assert_eq!(
            r("code", "The code may not be greater than 12 characters."),
            ["max:12"]
        );
        assert_eq!(
            r("note", "The note must not exceed 255 characters."),
            ["max:255"]
        );
        assert_eq!(
            r(
                "count",
                "The count must be a number and at least 1 characters."
            ),
            ["integer", "min:1"]
        );
    }

    fn spec(id: &str, case: &str, method: &str, body: Value) -> Spec {
        serde_json::from_value(
            json!({"id": id, "key": format!("{method} api/orders"), "case": case,
            "method": method, "template": "api/orders", "uri": "api/orders", "body": body}),
        )
        .unwrap()
    }

    #[test]
    fn fill_leaves_what_it_cannot_learn_from() {
        let dir = std::env::temp_dir().join(format!("laravel-parity-learn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pairs")).unwrap();
        let path = dir.join("pairs/shop.toml");
        let toml = format!(
            "name = \"shop\"\n[old]\nrepo = '{0}/old'\n[new]\nrepo = '{0}/new'\n[database]\nsource = \"shop\"\n",
            dir.display()
        );
        std::fs::write(&path, toml).unwrap();
        let config = crate::config::load(&path).unwrap();
        let mut skipped = spec("4", "happy", "POST", json!({}));
        skipped.skip = Some(json!("no id"));
        let mut specs = vec![
            spec("1", "happy", "POST", json!({"name": "a"})),
            spec("2", "guest", "POST", json!({})),
            spec("3", "without-name", "POST", json!({})),
            skipped,
            spec("5", "log-1", "PUT", json!({})),
        ];
        let before = serde_json::to_value(&specs).unwrap();
        // no answers from the old app yet
        assert!(
            fill(&config, &mut specs, "2026-10-31 09:00:00")
                .unwrap()
                .is_empty()
        );

        // answered, refused by a caught exception, or refused in a case that should not pass: no Sampler (no MySQL) needed
        let refused =
            json!({"message": "invalid", "errors": {"name": ["The name field is required."]}});
        let rows = [
            json!({"id": "1", "status": 200, "json": {}}),
            json!({"id": "2", "status": 422, "json": refused}),
            json!({"id": "3", "status": 422, "json": refused}),
            json!({"id": "4", "status": 422, "json": refused}),
            json!({"id": "5", "status": 500, "json": {"errors": {"general": "boom"}}}),
        ];
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        std::fs::create_dir_all(&config.work).unwrap();
        std::fs::write(config.work.join("results-old.jsonl"), text).unwrap();
        assert!(
            fill(&config, &mut specs, "2026-10-31 09:00:00")
                .unwrap()
                .is_empty()
        );
        assert_eq!(serde_json::to_value(&specs).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn carry_keeps_learned_fields() {
        let mut from = spec(
            "1",
            "happy",
            "POST",
            json!({"tenant": {"id": 7}, "password": "secret1", "password_confirmation": "secret1",
                   "name": "made"}),
        );
        from.learned = vec![
            "tenant.id: The tenant.id field is required.".into(),
            "password: The password confirmation does not match.".into(),
            "missing: The missing field is required.".into(),
            "no separator".into(),
        ];
        let (mut body, mut query) = (None, None);
        carry(&from, &mut body, &mut query);
        assert_eq!(
            body,
            Some(
                json!({"tenant": {"id": 7}, "password": "secret1", "password_confirmation": "secret1"})
            )
        );
        assert_eq!(query, None);

        // a rebuilt body keeps its own fields; learned ones win
        let mut body = Some(json!({"name": "new", "tenant": {"id": 1, "kind": "x"}}));
        carry(&from, &mut body, &mut query);
        assert_eq!(
            body,
            Some(json!({"name": "new", "tenant": {"id": 7, "kind": "x"},
                        "password": "secret1", "password_confirmation": "secret1"}))
        );

        // GET: from and into the query
        let mut get = spec("2", "happy", "GET", json!({"from": "ignored"}));
        get.query = json!({"from": "2026-10-31", "page": 1})
            .as_object()
            .cloned();
        get.learned = vec!["from: The from does not match the format Y-m-d.".into()];
        let (mut body, mut query) = (None, Some(Map::new()));
        carry(&get, &mut body, &mut query);
        assert_eq!(body, None);
        assert_eq!(query, json!({"from": "2026-10-31"}).as_object().cloned());

        // nothing learned: nothing changes
        let plain = spec("3", "happy", "POST", json!({"a": 1}));
        let mut body = Some(json!({"b": 2}));
        carry(&plain, &mut body, &mut query);
        assert_eq!(body, Some(json!({"b": 2})));
    }
}
