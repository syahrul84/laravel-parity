//! Broadcasting after the upgrade (Laravel Echo + Pusher / Redis to Reverb): who may join each private and presence
//! channel (routes/channels.php, asked the way /broadcasting/auth asks), whether that route still exists, and which
//! driver each app broadcasts with. The channel names come from what the requests broadcast during `run`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::config::{Config, Side};
use crate::fixlist::Fix;
use crate::ops::{Res, jsonl, read, write};
use crate::{runner, say};

/// "private-orders.5" -> "orders.5": the name the channel callbacks match.
pub fn normalize(channel: &str) -> Option<String> {
    ["private-encrypted-", "private-", "presence-"]
        .iter()
        .find_map(|p| channel.strip_prefix(p))
        .map(String::from)
}

/// Private and presence channels the apps broadcast on during the run (public ones need no sign-in).
fn seen(config: &Config) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for file in [
        "results-old.jsonl",
        "results-new.jsonl",
        "console-old.jsonl",
        "console-new.jsonl",
    ] {
        let rows: Vec<Value> = if file.starts_with("console") {
            std::fs::read_to_string(config.work.join(file))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        } else {
            jsonl(&config.work.join(file)).into_values().collect()
        };
        for row in rows {
            for broadcast in row["effects"]["broadcasts"]
                .as_array()
                .into_iter()
                .flatten()
            {
                for channel in broadcast["channels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    if let Some(name) = normalize(channel) {
                        names.insert(name);
                    }
                }
            }
        }
    }
    names
}

fn ask(config: &Config, side: &Side, channels: &[String]) -> Res<Value> {
    runner::ensure_ready(side)?;
    let rows = runner::run_job(
        config,
        side,
        &json!({"mode": "channels", "users": config.users, "channels": channels}),
        0,
    )?;
    let info = rows
        .iter()
        .find_map(|r| r.get("info").cloned())
        .unwrap_or(json!({}));
    if info["capabilities"]["channels"] == json!(false) {
        return Ok(json!({"not_compared": true}));
    }
    rows.into_iter()
        .find_map(|r| r.get("channels").cloned())
        .ok_or_else(|| format!("the {} app did not answer the channel check", side.label))
}

pub fn check(config: &Config) -> Res<Value> {
    let names: Vec<String> = seen(config).into_iter().collect();
    say(&format!(
        "Asking both apps who may join {} private / presence channels seen in the run, plus fixed ones ...",
        names.len()
    ));
    let (old, new) = (
        ask(config, &config.old, &names)?,
        ask(config, &config.new, &names)?,
    );
    if old.get("not_compared").is_some() || new.get("not_compared").is_some() {
        let result = json!({"not_compared": true, "summary": "Channels: not compared (a Laravel internal the tool reads is missing; update laravel-parity)"});
        write(&config.work.join("channels.json"), &result)?;
        return Ok(result);
    }
    let answers = |v: &Value| -> BTreeMap<(String, String), String> {
        v["answers"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| {
                (
                    (
                        a["channel"].as_str().unwrap_or_default().to_string(),
                        a["who"].as_str().unwrap_or_default().to_string(),
                    ),
                    a["answer"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    };
    let (a, b) = (answers(&old), answers(&new));
    let mut changed = vec![];
    for (key, before) in &a {
        // a pattern the new app no longer has is reported once, as removed
        let Some(after) = b.get(key) else { continue };
        if after != before {
            changed.push(json!({"channel": key.0, "who": key.1, "old": before, "new": after}));
        }
    }
    let patterns = |v: &Value| -> BTreeSet<String> {
        v["patterns"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().map(String::from))
            .collect()
    };
    let (pa, pb) = (patterns(&old), patterns(&new));
    let result = json!({
        "driver": {"old": old["driver"], "new": new["driver"]},
        "driver_config": {"old": old["driver_config"], "new": new["driver_config"]},
        "auth_route": {"old": old["auth_route"], "new": new["auth_route"]},
        "patterns": {"old": pa.len(), "new": pb.len(),
                     "removed": pa.difference(&pb).collect::<Vec<_>>(), "added": pb.difference(&pa).collect::<Vec<_>>()},
        "asked": a.keys().filter(|k| b.contains_key(*k)).count(),
        "answers": {"old": old["answers"], "new": new["answers"]},
        "changed": changed,
    });
    let line = format!(
        "Channels: broadcasting {} -> {}; {} channel answers compared, {} differ; {} channel patterns removed",
        old["driver"].as_str().unwrap_or("?"),
        new["driver"].as_str().unwrap_or("?"),
        result["asked"],
        result["changed"].as_array().map_or(0, Vec::len),
        result["patterns"]["removed"].as_array().map_or(0, Vec::len)
    );
    say(&line);
    let mut result = result;
    result["summary"] = json!(line);
    write(&config.work.join("channels.json"), &result)?;
    // the fix list picks this up (when there are results to build it from)
    let _ = crate::fixlist::write_all(config);
    Ok(result)
}

pub fn last(config: &Config) -> Option<Value> {
    read(&config.work.join("channels.json"))
}

/// The channel check as fix-list items (area Broadcasting).
pub fn fixes(config: &Config) -> Vec<Fix> {
    let Some(found) = last(config) else {
        return vec![];
    };
    let mut out = vec![];
    let mut add = |priority: u8, title: String, hint: &str, details: Vec<String>| {
        if !details.is_empty() {
            out.push(Fix {
                priority,
                area: "Broadcasting".into(),
                title,
                location: None,
                hint: Some(hint.into()),
                routes: vec![],
                details,
            });
        }
    };
    if found["not_compared"] == json!(true) {
        add(
            4,
            "Broadcast channels were not compared".into(),
            "Update laravel-parity (`laravel-parity update`).",
            vec!["a Laravel internal the channel check reads is missing".into()],
        );
        return out;
    }
    // the user channel every Laravel skeleton ships, usually never subscribed to
    let stub = |p: &String| p == "App.User.{id}" || p == "App.Models.User.{id}";
    let (stubs, removed): (Vec<String>, Vec<String>) = found["patterns"]["removed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(String::from))
        .partition(stub);
    add(
        4,
        "Laravel's default user channel is gone".into(),
        "Only matters if the web app subscribes to it (Echo.private('App.User.' + id)).",
        stubs,
    );
    add(
        1,
        format!(
            "{} broadcast channels are no longer defined: Echo clients subscribing to them are refused",
            removed.len()
        ),
        "Laravel 11+ loads channels from routes/channels.php through bootstrap/app.php (->withBroadcasting / withRouting(channels:)); check it is loaded and the patterns match.",
        removed,
    );
    let changed: Vec<&Value> = found["changed"].as_array().into_iter().flatten().collect();
    let detail = |c: &&Value| {
        format!(
            "{} as {}: {} -> {}",
            c["channel"].as_str().unwrap_or_default(),
            c["who"].as_str().unwrap_or_default(),
            c["old"].as_str().unwrap_or_default(),
            c["new"].as_str().unwrap_or_default()
        )
    };
    let lost: Vec<String> = changed
        .iter()
        .filter(|c| {
            c["old"].as_str().unwrap_or_default().starts_with("allowed")
                && !c["new"].as_str().unwrap_or_default().starts_with("allowed")
        })
        .map(detail)
        .collect();
    add(
        1,
        format!(
            "{} channel subscriptions are now refused (users stop getting live updates)",
            lost.len()
        ),
        "Compare the channel callback in routes/channels.php: its parameters (model binding), the guard option and the user model changed in the upgrade.",
        lost,
    );
    let opened: Vec<String> = changed
        .iter()
        .filter(|c| {
            !c["old"].as_str().unwrap_or_default().starts_with("allowed")
                && c["new"].as_str().unwrap_or_default().starts_with("allowed")
        })
        .map(detail)
        .collect();
    add(
        1,
        format!(
            "{} channel subscriptions are now allowed that the old app refused",
            opened.len()
        ),
        "Someone may now receive events meant for others: check the channel callback.",
        opened,
    );
    let presence: Vec<String> = changed
        .iter()
        .filter(|c| {
            c["old"]
                .as_str()
                .unwrap_or_default()
                .starts_with("allowed ")
                && c["new"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("allowed ")
        })
        .map(detail)
        .collect();
    add(
        3,
        format!(
            "{} presence channels return other member data",
            presence.len()
        ),
        "The web app shows what the presence callback returns: check the fields it reads.",
        presence,
    );
    if !found["auth_route"]["old"].is_null() && found["auth_route"]["new"].is_null() {
        add(1, "The new app has no /broadcasting/auth route: private and presence channels cannot be joined".into(),
            "Call Broadcast::routes() (Laravel 11+: ->withBroadcasting() in bootstrap/app.php).",
            vec![format!("old: {}", found["auth_route"]["old"]["uri"].as_str().unwrap_or("broadcasting/auth"))]);
    } else if !found["auth_route"]["new"].is_null()
        && found["auth_route"]["old"]["uri"] != found["auth_route"]["new"]["uri"]
    {
        add(
            2,
            "The broadcasting auth route moved: Echo must call the new address".into(),
            "Set authEndpoint in the web app's Echo options to the new path (with the API's base URL).",
            vec![format!(
                "{} -> {}",
                found["auth_route"]["old"]["uri"]
                    .as_str()
                    .unwrap_or("broadcasting/auth"),
                found["auth_route"]["new"]["uri"]
                    .as_str()
                    .unwrap_or_default()
            )],
        );
    }
    if !found["auth_route"]["new"].is_null()
        && !found["auth_route"]["old"].is_null()
        && found["auth_route"]["old"]["middleware"] != found["auth_route"]["new"]["middleware"]
    {
        add(
            3,
            "The /broadcasting/auth route has other middleware".into(),
            "Echo signs in with the guard this middleware uses (auth:api, auth:sanctum ...): it must match the token the web app sends.",
            vec![format!(
                "{} -> {}",
                found["auth_route"]["old"]["middleware"], found["auth_route"]["new"]["middleware"]
            )],
        );
    }
    let (old_driver, new_driver) = (
        found["driver"]["old"].as_str().unwrap_or_default(),
        found["driver"]["new"].as_str().unwrap_or_default(),
    );
    if ["null", "log"].contains(&new_driver) && !["null", "log", ""].contains(&old_driver) {
        add(
            2,
            format!(
                "The new app broadcasts with the '{new_driver}' driver (the old one used '{old_driver}'): nothing reaches the browsers"
            ),
            "Set BROADCAST_CONNECTION (Laravel 11+; BROADCAST_DRIVER before) to reverb in the new environment.",
            vec![format!("{old_driver} -> {new_driver}")],
        );
    } else if old_driver != new_driver {
        let keys: Vec<String> = found["driver_config"]["new"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|k| k.as_str().map(String::from))
            .collect();
        let missing: Vec<String> = ["key", "secret", "app_id"]
            .iter()
            .filter(|k| !keys.iter().any(|x| x == *k))
            .map(|k| k.to_string())
            .collect();
        if new_driver == "reverb" && !missing.is_empty() {
            add(
                2,
                "The reverb connection is missing settings in the environment the tool ran".into(),
                "Set REVERB_APP_KEY, REVERB_APP_SECRET and REVERB_APP_ID (and the web app's VITE_REVERB_* / Echo settings) in every environment.",
                missing
                    .into_iter()
                    .map(|k| format!("broadcasting.connections.reverb.{k} is empty"))
                    .collect(),
            );
        }
        add(
            4,
            format!("Broadcasting moved from '{old_driver}' to '{new_driver}'"),
            "The web app's Echo settings (broadcaster, key, host, port, TLS) must change with it.",
            vec![format!("{old_driver} -> {new_driver}")],
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(normalize("private-orders.5").as_deref(), Some("orders.5"));
        assert_eq!(normalize("presence-room.1").as_deref(), Some("room.1"));
        assert_eq!(normalize("private-encrypted-x").as_deref(), Some("x"));
        assert_eq!(normalize("public-news"), None);
    }
}
