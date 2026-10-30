//! Route lists from codemap for both apps, and what changed between them.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::codemap::Codemap;
use crate::config::Config;
use crate::glob;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub method: String,
    pub uri: String,
    pub action: Option<String>,
    pub middleware: Vec<String>,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Changed {
    pub route: String,
    pub changes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteDiff {
    pub removed: Vec<String>,
    pub added: Vec<String>,
    pub changed: Vec<Changed>,
}

/// The guard a request signs in through: the first one named by an auth:... middleware.
pub fn guard_of(middleware: &[String]) -> Option<String> {
    for entry in middleware {
        if let Some(guards) = entry.strip_prefix("auth:") {
            return guards.split(',').next().map(String::from);
        }
        if entry == "auth" {
            return Some("default".into());
        }
    }
    None
}

/// "METHOD uri" -> route, one entry per method.
pub fn collect(codemap: &mut Codemap) -> Result<BTreeMap<String, Route>, String> {
    let mut found = BTreeMap::new();
    for route in codemap.routes()? {
        let uri = route["uri"].as_str().unwrap_or_default().to_string();
        let middleware: BTreeSet<String> = route["middleware"]
            .as_array()
            .map(|m| {
                m.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        for method in route["methods"]
            .as_str()
            .unwrap_or_default()
            .split('|')
            .filter(|m| *m != "HEAD")
        {
            found.insert(
                format!("{method} {uri}"),
                Route {
                    method: method.to_string(),
                    uri: uri.clone(),
                    action: route["action"].as_str().map(String::from),
                    middleware: middleware.iter().cloned().collect(),
                    file: route["file"].as_str().map(String::from),
                },
            );
        }
    }
    Ok(found)
}

pub fn wanted(config: &Config, key: &str) -> bool {
    let (method, uri) = key.split_once(' ').unwrap_or((key, ""));
    config.methods.iter().any(|m| m == method)
        && glob::any(uri, &config.include)
        && !(glob::any(uri, &config.exclude) || glob::any(key, &config.exclude))
}

/// Routes only in the old app, only in the new one, and shared routes whose controller or middleware changed.
pub fn diff(old: &BTreeMap<String, Route>, new: &BTreeMap<String, Route>) -> RouteDiff {
    let mut result = RouteDiff::default();
    for (key, a) in old {
        let Some(b) = new.get(key) else {
            result.removed.push(key.clone());
            continue;
        };
        let mut changes = vec![];
        if a.action != b.action {
            changes.push(format!(
                "action {} -> {}",
                a.action.as_deref().unwrap_or("?"),
                b.action.as_deref().unwrap_or("?")
            ));
        }
        if a.middleware != b.middleware {
            let gone: Vec<_> = a
                .middleware
                .iter()
                .filter(|m| !b.middleware.contains(m))
                .collect();
            let added: Vec<_> = b
                .middleware
                .iter()
                .filter(|m| !a.middleware.contains(m))
                .collect();
            changes.push(format!("middleware -{gone:?} +{added:?}"));
        }
        if !changes.is_empty() {
            result.changed.push(Changed {
                route: key.clone(),
                changes,
            });
        }
    }
    result.added = new
        .keys()
        .filter(|k| !old.contains_key(*k))
        .cloned()
        .collect();
    result
}
