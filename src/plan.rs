//! Turns the shared routes into concrete requests: route parameters are filled with real ids from the test data,
//! guessed from their names or found through codemap in the controller that handles them.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::codemap::Codemap;
use crate::config::Config;
use crate::db::{self, TableInfo};
use crate::routes::{Route, guard_of, wanted};

pub static PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{(\w+)(\?)?\}").unwrap());
/// Runs of slashes, left when an optional parameter is empty.
pub static SLASHES: LazyLock<Regex> = LazyLock::new(|| Regex::new("/+").unwrap());

/// One request, as stored in requests.json. A person may edit body, query, headers, values, skip, guard and note;
/// a new plan keeps them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Spec {
    pub id: String,
    pub key: String,
    /// happy, guest, missing, empty, without-<field>: one route is sent several ways
    #[serde(default = "happy")]
    pub case: String,
    /// Fields taken out after the pair file's default body is merged in (a required field left out).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub without: Vec<String>,
    pub method: String,
    pub template: String,
    pub uri: String,
    #[serde(default)]
    pub guard: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub old_action: Option<String>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<Map<String, Value>>,
    /// Only what a person typed; sampled ids are found again on every plan.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    #[serde(default)]
    pub filled: BTreeMap<String, String>,
    #[serde(default)]
    pub sources: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<Value>,
    /// A case someone added (the AI through `add_case`, or a person in requests.json): kept by every new plan.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom: bool,
    /// The body the tool made (from the rules, the web app's calls and what it learned): a body that differs from it
    /// was typed by a person, and a new plan keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made: Option<Value>,
    /// "field: message" for each field filled because the old app's validation errors asked for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub learned: Vec<String>,
}

/// Whether a person typed this request's body (a new plan keeps it) rather than the tool making it.
pub fn person_typed(spec: &Spec) -> bool {
    let Some(body) = spec.body.as_ref().filter(|b| *b != &json!({})) else {
        return false;
    };
    match &spec.made {
        Some(made) => made != body,
        // requests.json from before `made` was kept: the tool's bodies carry its note, its validation cases a field
        None => {
            !(spec.case.starts_with("without-")
                || spec
                    .note
                    .as_ref()
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.starts_with("body made from")))
        }
    }
}

fn happy() -> String {
    "happy".into()
}

impl Spec {
    /// "METHOD uri" for the happy case, "METHOD uri [guest]" for the others.
    pub fn name(&self) -> String {
        if self.case == "happy" {
            self.key.clone()
        } else {
            format!("{} [{}]", self.key, self.case)
        }
    }
}

/// A short stable id (FNV-1a), the same on every machine.
pub fn request_id(key: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in key.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")[..10].to_string()
}

pub fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out.replace('-', "_")
}

pub fn plurals(word: &str) -> Vec<String> {
    let w = word.to_string();
    if w.ends_with('y') && !["ay", "ey", "oy", "uy"].iter().any(|e| w.ends_with(e)) {
        return vec![format!("{}ies", &w[..w.len() - 1]), format!("{w}s"), w];
    }
    if ["s", "x", "ch", "sh"].iter().any(|e| w.ends_with(e)) {
        return vec![format!("{w}es"), w];
    }
    vec![format!("{w}s"), w]
}

/// Tables a parameter probably points at: {unit}, {unit_id}, {unitId} -> units; {id} -> the segment before it.
pub fn candidate_tables(uri: &str, param: &str) -> Vec<String> {
    static SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(_id|_uuid|id)$").unwrap());
    let mut base = SUFFIX
        .replace(&snake(param), "")
        .trim_matches('_')
        .to_string();
    if base.is_empty() {
        let before = uri
            .split(&format!("{{{param}"))
            .next()
            .unwrap_or("")
            .trim_end_matches('/');
        let last = before.rsplit('/').next().unwrap_or("");
        base = if !last.is_empty() && !last.starts_with('{') {
            snake(last)
        } else {
            uri.split('/')
                .rfind(|s| !s.is_empty() && !s.starts_with('{'))
                .map(snake)
                .unwrap_or_default()
        };
    }
    let mut names = plurals(&base);
    if let Some((_, last)) = base.rsplit_once('_') {
        names.extend(plurals(last));
    }
    names
}

/// Finds a real value for a table in the old test database (both test databases hold the same rows).
///
/// Values are picked to belong together, the way a real user's request would: a row of the project / company / user
/// the pair file sends by default (`project_id` in [run] query or body, or the my-project-id header), and a child row
/// of the parent the same route already picked ({project}/units/{unit}: a unit of that project), through declared
/// foreign keys or the `<parent>_id` naming. When no row matches, the conditions are dropped one by one.
pub struct Sampler<'a> {
    config: &'a Config,
    conn: mysql::Conn,
    pub tables: BTreeMap<String, TableInfo>,
    cache: HashMap<String, Option<String>>,
    /// (column, value) every pick prefers: the pair file's defaults
    scope: Vec<(String, String)>,
    /// (table, value) the current route already picked
    chosen: Vec<(String, String)>,
    /// (table, column) -> the table its foreign key points at
    foreign: HashMap<(String, String), String>,
}

/// "units" -> "unit", "companies" -> "company", "addresses" -> "address".
pub fn singular(table: &str) -> String {
    if let Some(stem) = table.strip_suffix("ies") {
        format!("{stem}y")
    } else if let Some(stem) = table
        .strip_suffix("sses")
        .or_else(|| table.strip_suffix("xes"))
    {
        format!("{stem}{}", if table.ends_with("sses") { "ss" } else { "x" })
    } else if let Some(stem) = table.strip_suffix('s') {
        stem.to_string()
    } else {
        table.to_string()
    }
}

/// The (column, value) pairs the pair file sends with every request: *_id fields of [run] query / body, and the
/// project header.
pub fn default_scope(config: &Config) -> Vec<(String, String)> {
    let mut scope: Vec<(String, String)> = vec![];
    let text = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    if let Some(project) = config.headers.get("my-project-id").and_then(text) {
        scope.push(("project_id".into(), project));
    }
    for (key, value) in config
        .default_query
        .iter()
        .chain(config.default_body.iter())
    {
        if key.ends_with("_id")
            && let Some(v) = text(value)
            && !scope.iter().any(|(k, _)| k == key)
        {
            scope.push((key.clone(), v));
        }
    }
    scope
}

impl<'a> Sampler<'a> {
    pub fn new(config: &'a Config) -> Result<Self, String> {
        let mut conn = db::connect(config, &config.old)?;
        let foreign = db::rows(
            &mut conn,
            &format!(
                "SELECT TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME FROM information_schema.KEY_COLUMN_USAGE \
                 WHERE TABLE_SCHEMA = '{}' AND REFERENCED_TABLE_NAME IS NOT NULL",
                config.old.database.replace('\'', "''")
            ),
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| Some(((r[0].clone()?, r[1].clone()?), r[2].clone()?)))
        .collect();
        Ok(Sampler {
            config,
            conn,
            tables: db::tables(config, &config.old)?,
            cache: HashMap::new(),
            scope: default_scope(config),
            chosen: vec![],
            foreign,
        })
    }

    /// A new route: what the last one picked no longer applies.
    pub fn start_route(&mut self) {
        self.chosen.clear();
    }

    pub fn by_name(&mut self, uri: &str, param: &str) -> (Option<String>, String) {
        for table in candidate_tables(uri, param) {
            let Some(info) = self.tables.get(&table).cloned() else {
                continue;
            };
            let Some(pk) = info.pk.clone() else { continue };
            let key = format!("{table}.{pk}");
            let found = self.cached(&key, &table, &info, None);
            return match found {
                Some(v) => (Some(v), key),
                None => (None, format!("{table} is empty")),
            };
        }
        (None, "no matching table".into())
    }

    /// A value from a table codemap pointed at (its primary key, or the column the controller filters on).
    pub fn in_table(&mut self, table: &str, column: Option<&str>) -> (Option<String>, String) {
        let Some(info) = self.tables.get(table).cloned() else {
            return (None, format!("{table} not in the test database"));
        };
        if let Some(c) = column
            && !info.columns.iter().any(|x| x == c)
        {
            return (
                None,
                format!("code filters {table} on {c}, which the table does not have"),
            );
        }
        let Some(col) = column.map(String::from).or(info.pk.clone()) else {
            return (None, format!("{table} has no primary key"));
        };
        let key = format!("{table}.{col}");
        match self.cached(&key, table, &info, column) {
            Some(v) => (Some(v), format!("{key} (codemap)")),
            None => (None, format!("{table} is empty")),
        }
    }

    /// The conditions a row of `table` should meet, most important first: its parent picked by this route, then the
    /// pair file's defaults.
    fn conditions(&self, table: &str, info: &TableInfo) -> Vec<(String, String)> {
        let mut found: Vec<(String, String)> = vec![];
        for (parent, value) in self.chosen.iter().rev() {
            if parent == table {
                continue;
            }
            let named = format!("{}_id", singular(parent));
            for column in &info.columns {
                let points = self
                    .foreign
                    .get(&(table.to_string(), column.clone()))
                    .is_some_and(|t| t == parent);
                if (points || *column == named || column.ends_with(&format!("_{named}")))
                    && !found.iter().any(|(c, _)| c == column)
                {
                    found.push((column.clone(), value.clone()));
                }
            }
        }
        for (key, value) in &self.scope {
            // project_id, or prefixed: unit_project_id
            if let Some(column) = info
                .columns
                .iter()
                .find(|c| *c == key || c.ends_with(&format!("_{key}")))
                && !found.iter().any(|(c, _)| c == column)
            {
                found.push((column.clone(), value.clone()));
            }
        }
        found
    }

    fn cached(
        &mut self,
        key: &str,
        table: &str,
        info: &TableInfo,
        column: Option<&str>,
    ) -> Option<String> {
        let conditions = self.conditions(table, info);
        let cache_key = format!("{key}|{conditions:?}");
        if !self.cache.contains_key(&cache_key) {
            let value = self.pick(table, info, column, &conditions);
            self.cache.insert(cache_key.clone(), value);
        }
        let value = self.cache[&cache_key].clone();
        // the route's later parameters pick children of this row
        if let Some(v) = &value
            && (column.is_none() || column == info.pk.as_deref())
        {
            self.chosen.push((table.to_string(), v.clone()));
        }
        value
    }

    fn pick(
        &mut self,
        table: &str,
        info: &TableInfo,
        column: Option<&str>,
        conditions: &[(String, String)],
    ) -> Option<String> {
        let col = column.map(String::from).or(info.pk.clone())?;
        let base = column.map(|_| format!("`{col}` IS NOT NULL"));
        // all conditions, then fewer (the least important dropped first), then none
        for keep in (0..=conditions.len()).rev() {
            let mut clauses: Vec<String> = conditions[..keep]
                .iter()
                .map(|(c, v)| format!("`{c}` = '{}'", v.replace('\'', "''")))
                .collect();
            clauses.extend(base.clone());
            let clause = if clauses.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", clauses.join(" AND "))
            };
            let sql = format!(
                "SELECT `{col}` FROM `{}`.`{table}`{clause} ORDER BY `{col}` DESC LIMIT 1",
                self.config.old.database
            );
            if let Ok(rows) = db::rows(&mut self.conn, &sql)
                && let Some(Some(v)) = rows.first().and_then(|r| r.first().cloned())
            {
                return Some(v);
            }
        }
        None
    }

    pub fn sql(&mut self, sql: &str) -> Option<String> {
        let _ = mysql::prelude::Queryable::query_drop(
            &mut self.conn,
            format!("USE `{}`", self.config.old.database),
        );
        db::rows(&mut self.conn, sql)
            .ok()?
            .first()?
            .first()?
            .clone()
    }
}

/// Which table a route parameter points at, read from the controller through codemap.
///
/// Laravel hands route parameters to the controller method by position, not by name ({fsr} arrives as $id), so the
/// n-th route parameter is the n-th method argument that is not an injected service. The argument's model type says
/// the table; for an untyped $id the method body is read for Model::find($id), ->findOrFail($id) or
/// Model::where('column', $id), and codemap gives that model's table.
pub struct CodeLookup<'a> {
    codemap: &'a mut Codemap,
    repo: PathBuf,
    symbols: HashMap<String, Option<Value>>,
}

static INJECTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(Request|Service|Repository|Manager|Factory|Guard|Gate)$").unwrap()
});
static USE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*use\s+([\w\\]+)\s*;").unwrap());
static NAMESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*namespace\s+([\w\\]+)\s*;").unwrap());
static STATIC_CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\\?[A-Z][\w\\]*)::").unwrap());

impl<'a> CodeLookup<'a> {
    pub fn new(codemap: &'a mut Codemap, repo: &Path) -> Self {
        CodeLookup {
            codemap,
            repo: repo.to_path_buf(),
            symbols: HashMap::new(),
        }
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// A class name as written in a file, made full with the file's `use` lines and namespace.
    pub fn class_name(short: &str, source: &str) -> String {
        let namespace = NAMESPACE
            .captures(source)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        Self::class(short, source, &namespace)
    }

    pub fn symbol(&mut self, name: &str) -> Option<Value> {
        if !self.symbols.contains_key(name) {
            let data = self
                .codemap
                .call("get_symbol", json!({"symbol": name}))
                .ok()
                .map(|a| a["data"].clone());
            let data = data.filter(|d| d["found"].as_bool() == Some(true));
            self.symbols.insert(name.to_string(), data);
        }
        self.symbols[name].clone()
    }

    fn class(short: &str, source: &str, namespace: &str) -> String {
        if let Some(full) = short.strip_prefix('\\') {
            return full.to_string();
        }
        let first = short.split('\\').next().unwrap_or(short);
        for cap in USE.captures_iter(source) {
            let full = &cap[1];
            if full.rsplit('\\').next() == Some(first) {
                return format!("{full}{}", &short[first.len()..]);
            }
        }
        if namespace.is_empty() {
            short.to_string()
        } else {
            format!("{namespace}\\{short}")
        }
    }

    fn table(&mut self, class: &str) -> Option<String> {
        self.symbol(class)?["model"]["table"]
            .as_str()
            .map(String::from)
    }

    /// (table, column or None for the primary key), or None.
    pub fn find(
        &mut self,
        action: Option<&str>,
        uri: &str,
        param: &str,
    ) -> Option<(String, Option<String>)> {
        let (class, method) = action?.split_once('@')?;
        let symbol = self.symbol(&format!("{class}::{method}"))?;
        let params: Vec<Value> = symbol["meta"]["params"]
            .as_array()?
            .iter()
            .filter(|p| !p["type"].as_str().is_some_and(|t| INJECTED.is_match(t)))
            .cloned()
            .collect();
        let names: Vec<String> = PARAM.captures_iter(uri).map(|c| c[1].to_string()).collect();
        let position = names.iter().position(|n| n == param)?;
        let argument = params.get(position)?;
        let file = self.repo.join(symbol["file"].as_str()?);
        let source = std::fs::read_to_string(&file).unwrap_or_default();
        let namespace = NAMESPACE
            .captures(&source)
            .map(|c| c[1].to_string())
            .unwrap_or_default();

        if let Some(kind) = argument["type"].as_str()
            && (kind.starts_with(|c: char| c.is_uppercase()) || kind.contains('\\'))
            && let Some(table) = self.table(&Self::class(kind, &source, &namespace))
        {
            return Some((table, None));
        }

        let start = symbol["line"].as_u64()? as usize;
        let end = symbol["end_line"].as_u64().unwrap_or(start as u64) as usize;
        let body: Vec<&str> = source
            .lines()
            .skip(start.saturating_sub(1))
            .take(end + 1 - start)
            .collect();
        let var = regex::escape(argument["name"].as_str()?);
        let mentions = Regex::new(&format!(r"{var}\b")).ok()?;
        let lookup = Regex::new(&format!(
            r"(?:find|findOrFail|findOrNew|firstOrFail|whereKey)\(\s*{var}\b"
        ))
        .ok()?;
        let filter = Regex::new(&format!(
            r#"where\(\s*['"](\w+)['"]\s*,\s*(?:['"]=['"]\s*,\s*)?{var}\b"#
        ))
        .ok()?;
        for statement in body.join("\n").split(';') {
            if !mentions.is_match(statement) {
                continue;
            }
            let Some(model) = STATIC_CALL.captures(statement) else {
                continue;
            };
            let Some(table) = self.table(&Self::class(&model[1], &source, &namespace)) else {
                continue;
            };
            if lookup.is_match(statement) {
                return Some((table, None));
            }
            if let Some(column) = filter.captures(statement) {
                return Some((table, Some(column[1].to_string())));
            }
        }
        None
    }
}

fn override_value(config: &Config, key: &str, uri: &str, param: &str) -> Option<toml::Value> {
    for scope in [key, uri] {
        if let Some(toml::Value::Table(t)) = config.params.get(scope)
            && let Some(v) = t.get(param)
        {
            return Some(v.clone());
        }
    }
    config.params.get(param).filter(|v| !v.is_table()).cloned()
}

pub fn build(
    config: &Config,
    old_routes: &BTreeMap<String, Route>,
    new_routes: &BTreeMap<String, Route>,
    previous: &[Spec],
    mut lookup: Option<&mut CodeLookup>,
) -> Result<Vec<Spec>, String> {
    let kept: HashMap<(&str, &str), &Spec> = previous
        .iter()
        .map(|s| ((s.key.as_str(), s.case.as_str()), s))
        .collect();
    let clock = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let wants = |case: &str| config.cases.iter().any(|c| c == case);
    // what the web app sends to each route (nothing without [web])
    let web_sends = crate::web::sends(config);
    let mut sampler = Sampler::new(config)?;
    let mut specs = vec![];
    for (key, route) in new_routes {
        if !old_routes.contains_key(key) || !wanted(config, key) {
            continue;
        }
        let (method, uri) = key.split_once(' ').unwrap();
        let mut spec = Spec {
            id: request_id(key),
            key: key.clone(),
            case: "happy".into(),
            method: method.to_string(),
            template: uri.to_string(),
            guard: guard_of(&route.middleware),
            action: route.action.clone(),
            old_action: old_routes[key].action.clone(),
            body: if method == "GET" {
                None
            } else {
                Some(json!({}))
            },
            ..Default::default()
        };
        let typed = kept.get(&(key.as_str(), "happy"));
        if let Some(old) = typed {
            spec.body = old.body.clone().or(spec.body);
            spec.made = old.made.clone();
            spec.query = old.query.clone();
            spec.headers = old.headers.clone();
            spec.values = old.values.clone();
            spec.skip = old.skip.clone();
            spec.note = old.note.clone();
            if old.guard.is_some() {
                spec.guard = old.guard.clone();
            }
        }

        let mut filled = spec.values.clone();
        sampler.start_route();
        for cap in PARAM.captures_iter(uri) {
            let (param, optional) = (cap[1].to_string(), cap.get(2).is_some());
            if filled.contains_key(&param) {
                spec.sources.insert(param, "requests.json".into());
                continue;
            }
            if let Some(fixed) = override_value(config, key, uri, &param) {
                let value = match &fixed {
                    toml::Value::String(s) if s.to_lowercase().starts_with("sql:") => {
                        sampler.sql(&s[4..])
                    }
                    toml::Value::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                };
                if let Some(v) = value {
                    filled.insert(param.clone(), v);
                    spec.sources.insert(param, "pair file".into());
                    continue;
                }
            }
            let (mut found, mut source) = sampler.by_name(uri, &param);
            if found.is_none()
                && let Some(code) = lookup.as_deref_mut()
                && let Some((table, column)) = code.find(route.action.as_deref(), uri, &param)
            {
                (found, source) = sampler.in_table(&table, column.as_deref());
            }
            match found {
                Some(v) => {
                    filled.insert(param.clone(), v);
                    spec.sources.insert(param, source);
                }
                None if !optional => spec.unresolved.push(format!("{param} ({source})")),
                None => {}
            }
        }

        let resolved = PARAM.replace_all(uri, |c: &regex::Captures| {
            filled.get(&c[1]).cloned().unwrap_or_default()
        });
        let collapsed = SLASHES
            .replace_all(&resolved, "/")
            .trim_end_matches('/')
            .to_string();
        spec.uri = if collapsed.is_empty() {
            "/".into()
        } else {
            collapsed
        };
        spec.filled = filled;

        // a write gets a body that passes its validation rules (unless a person typed one)
        let rules = if method != "GET" {
            lookup
                .as_deref_mut()
                .and_then(|code| crate::rules::for_action(code, route.action.as_deref()))
        } else {
            None
        };
        let typed_body = typed.is_some_and(|t| person_typed(t));
        let sent = web_sends.get(key.as_str());
        if method != "GET" && !typed_body && (rules.is_some() || sent.is_some()) {
            let fields = rules.clone().unwrap_or_default();
            let mut body = Map::new();
            // fields the web app sends that have no rule: a real id for *_id, else the pair file's default or text
            for name in sent.into_iter().flatten() {
                if fields.iter().any(|f| &f.name == name) {
                    continue;
                }
                let value = if let Some(v) = config.default_body.get(name) {
                    Some(v.clone())
                } else if name.ends_with("_id") {
                    sampler
                        .by_name(uri, name)
                        .0
                        .map(|v| v.parse::<i64>().map(|n| json!(n)).unwrap_or(json!(v)))
                } else {
                    Some(json!(format!("parity {name}")))
                };
                if let Some(v) = value {
                    body.insert(name.clone(), v);
                }
            }
            for field in &fields {
                let mut pick = |table: &str, column: &str| sampler.in_table(table, Some(column)).0;
                if let Some(value) = crate::rules::valid_value(field, &clock, &mut pick) {
                    body.insert(field.name.clone(), value);
                    if field.has("confirmed") {
                        body.insert(
                            format!("{}_confirmation", field.name),
                            body[&field.name].clone(),
                        );
                    }
                }
            }
            spec.body = Some(Value::Object(body));
            let from = match (rules.is_some(), sent.is_some()) {
                (true, true) => "the fields the web app sends and the route's validation rules",
                (true, false) => "the route's validation rules",
                _ => "the fields the web app sends",
            };
            spec.note = spec
                .note
                .clone()
                .filter(|n| !n.as_str().is_some_and(|n| n.starts_with("body made from")))
                .or(Some(json!(format!("body made from {from}"))));
        }
        // fields earlier runs learned from the old app's validation errors stay
        if !typed_body && let Some(old) = typed {
            crate::learn::carry(old, &mut spec.body, &mut spec.query);
            spec.learned = old.learned.clone();
        }
        if !typed_body && method != "GET" {
            spec.made = spec.body.clone();
        }

        let mut cases = vec![];
        if wants("guest") && spec.guard.is_some() {
            let mut case = variant(&spec, "guest");
            case.guard = None;
            cases.push(case);
        }
        if wants("missing") && spec.unresolved.is_empty() {
            // every id that came from a table, replaced by one that does not exist
            let tables: Vec<String> = spec
                .sources
                .iter()
                .filter(|(_, s)| s.contains('.'))
                .map(|(p, _)| p.clone())
                .collect();
            if !tables.is_empty() {
                let mut case = variant(&spec, "missing");
                for param in &tables {
                    case.filled.insert(param.clone(), config.missing_id.clone());
                }
                let filled = case.filled.clone();
                let uri = PARAM.replace_all(&case.template, |c: &regex::Captures| {
                    filled.get(&c[1]).cloned().unwrap_or_default()
                });
                case.uri = SLASHES
                    .replace_all(&uri, "/")
                    .trim_end_matches('/')
                    .to_string();
                cases.push(case);
            }
        }
        if wants("validation") && method != "GET" {
            let mut empty = variant(&spec, "empty");
            empty.body = Some(json!({}));
            cases.push(empty);
            if let (Some(fields), Some(Value::Object(body))) = (&rules, &spec.body) {
                for field in fields
                    .iter()
                    .filter(|f| f.required() && body.contains_key(&f.name))
                    .take(3)
                {
                    let mut case = variant(&spec, &format!("without-{}", field.name));
                    let mut less = body.clone();
                    less.remove(&field.name);
                    case.body = Some(Value::Object(less));
                    case.made = case.body.clone();
                    case.without = vec![field.name.clone()];
                    cases.push(case);
                }
            }
        }
        for mut case in cases {
            // what a person typed for this case in requests.json wins
            if let Some(old) = kept.get(&(key.as_str(), case.case.as_str())) {
                if person_typed(old) {
                    case.body = old.body.clone();
                    case.made = old.made.clone();
                }
                case.query = old.query.clone().or(case.query);
                case.headers = old.headers.clone().or(case.headers);
                case.skip = old.skip.clone();
                case.note = old.note.clone().or(case.note);
            }
            specs.push(case);
        }
        // cases someone added: the route's current ids, their own body / query / values
        for added in previous.iter().filter(|s| s.custom && s.key == *key) {
            let mut case = added.clone();
            case.template = spec.template.clone();
            case.action = spec.action.clone();
            case.old_action = spec.old_action.clone();
            case.sources = spec.sources.clone();
            case.unresolved = spec.unresolved.clone();
            let mut filled = spec.filled.clone();
            filled.extend(added.values.clone());
            let uri = PARAM.replace_all(&case.template, |c: &regex::Captures| {
                filled.get(&c[1]).cloned().unwrap_or_default()
            });
            case.uri = SLASHES
                .replace_all(&uri, "/")
                .trim_end_matches('/')
                .to_string();
            case.filled = filled;
            specs.push(case);
        }
        specs.push(spec);
    }
    specs.sort_by(|a, b| {
        (&a.key, a.case != "happy", &a.case).cmp(&(&b.key, b.case != "happy", &b.case))
    });
    Ok(specs)
}

/// Another way to send the same route.
pub fn variant(spec: &Spec, case: &str) -> Spec {
    let mut copy = spec.clone();
    copy.case = case.to_string();
    copy.id = request_id(&format!("{}#{case}", spec.key));
    copy.values = Default::default();
    copy.skip = None;
    copy.note = None;
    copy.without = vec![];
    copy.learned = vec![];
    copy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singulars() {
        assert_eq!(singular("units"), "unit");
        assert_eq!(singular("companies"), "company");
        assert_eq!(singular("addresses"), "address");
        assert_eq!(singular("boxes"), "box");
        assert_eq!(singular("staff"), "staff");
    }

    /// A pair file with `extra` appended, loaded the way `run` loads it.
    fn config(dir: &str, extra: &str) -> Config {
        let root =
            std::env::temp_dir().join(format!("laravel-parity-plan-{}-{dir}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("shop.toml");
        let text = format!(
            "[old]\nrepo = \"/nowhere/a\"\n[new]\nrepo = \"/nowhere/b\"\n[database]\nsource = \"shop\"\n{extra}"
        );
        std::fs::write(&path, text).unwrap();
        crate::config::load(&path).unwrap()
    }

    fn spec(key: &str, body: Option<Value>) -> Spec {
        let (method, uri) = key.split_once(' ').unwrap();
        Spec {
            id: request_id(key),
            key: key.into(),
            case: "happy".into(),
            method: method.into(),
            template: uri.into(),
            uri: uri.into(),
            body,
            ..Default::default()
        }
    }

    #[test]
    fn ids_and_names() {
        let id = request_id("GET api/units");
        assert_eq!(id.len(), 10);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(id, request_id("GET api/units"));
        assert_ne!(id, request_id("GET api/unit"));
        // FNV-1a of nothing is its offset basis
        assert_eq!(request_id(""), "cbf29ce484");

        let mut s = spec("GET api/units", None);
        assert_eq!(s.name(), "GET api/units");
        s.case = "guest".into();
        assert_eq!(s.name(), "GET api/units [guest]");

        // requests.json without a case is the happy one
        let read: Spec = serde_json::from_value(json!({
            "id": "a", "key": "GET api/units", "method": "GET", "template": "api/units", "uri": "api/units"
        }))
        .unwrap();
        assert_eq!(read.case, "happy");
        assert!(!read.custom && read.made.is_none());
        // empty and default fields stay out of the file
        let written = serde_json::to_value(&read).unwrap();
        for key in [
            "without", "query", "headers", "skip", "note", "custom", "made", "learned",
        ] {
            assert!(written.get(key).is_none(), "{key}");
        }
    }

    #[test]
    fn words_and_tables() {
        assert_eq!(snake("unitId"), "unit_id");
        assert_eq!(snake("PropertyUnit"), "property_unit");
        assert_eq!(snake("lease-items"), "lease_items");
        assert_eq!(plurals("unit"), vec!["units", "unit"]);
        assert_eq!(plurals("company"), vec!["companies", "companys", "company"]);
        assert_eq!(plurals("day"), vec!["days", "day"]);
        assert_eq!(plurals("box"), vec!["boxes", "box"]);
        assert_eq!(plurals("branch"), vec!["branches", "branch"]);

        assert_eq!(
            candidate_tables("api/units/{unit}", "unit"),
            vec!["units", "unit"]
        );
        assert_eq!(candidate_tables("api/x/{unit_id}", "unit_id")[0], "units");
        assert_eq!(candidate_tables("api/x/{unitId}", "unitId")[0], "units");
        // {id} takes the segment before it
        // (an already plural segment is tried as written too)
        assert_eq!(
            candidate_tables("api/companies/{id}", "id"),
            vec!["companieses", "companies"]
        );
        // two parameters in a row: the last plain segment
        assert_eq!(
            candidate_tables("api/leases/{lease}/{id}", "id"),
            vec!["leaseses", "leases"]
        );
        // a compound name also tries its last word
        assert_eq!(
            candidate_tables("api/x/{property_unit}", "property_unit"),
            vec!["property_units", "property_unit", "units", "unit"]
        );

        assert_eq!(
            PARAM
                .captures_iter("api/units/{unit}/leases/{lease?}")
                .map(|c| (c[1].to_string(), c.get(2).is_some()))
                .collect::<Vec<_>>(),
            vec![("unit".into(), false), ("lease".into(), true)]
        );
        assert_eq!(
            SLASHES.replace_all("api/units//leases///", "/"),
            "api/units/leases/"
        );
    }

    #[test]
    fn typed_bodies() {
        // no body, or an empty one: nothing to keep
        assert!(!person_typed(&spec("GET api/units", None)));
        assert!(!person_typed(&spec("POST api/units", Some(json!({})))));

        // the tool's own body is made again; one that differs was typed
        let mut s = spec("POST api/units", Some(json!({"name": "parity name"})));
        s.made = s.body.clone();
        assert!(!person_typed(&s));
        s.body = Some(json!({"name": "Block A"}));
        assert!(person_typed(&s));

        // requests.json from before `made`: the note or the case says the tool made it
        let mut s = spec("POST api/units", Some(json!({"name": "Block A"})));
        assert!(person_typed(&s));
        s.note = Some(json!("body made from the route's validation rules"));
        assert!(!person_typed(&s));
        s.note = Some(json!("keep this one"));
        assert!(person_typed(&s));
        s.note = None;
        s.case = "without-name".into();
        assert!(!person_typed(&s));
    }

    #[test]
    fn variants() {
        let mut s = spec("POST api/units", Some(json!({"name": "parity name"})));
        s.guard = Some("api".into());
        s.values.insert("unit".into(), "5".into());
        s.filled.insert("unit".into(), "5".into());
        s.skip = Some(json!(true));
        s.note = Some(json!("a note"));
        s.without = vec!["name".into()];
        s.learned = vec!["name: The name field is required.".into()];
        s.made = s.body.clone();

        let v = variant(&s, "guest");
        assert_eq!(v.case, "guest");
        assert_eq!(v.id, request_id("POST api/units#guest"));
        assert_ne!(v.id, s.id);
        assert_eq!(v.name(), "POST api/units [guest]");
        // what belongs to the happy case alone is dropped
        assert!(v.values.is_empty() && v.skip.is_none() && v.note.is_none());
        assert!(v.without.is_empty() && v.learned.is_empty());
        // the request itself is the same
        assert_eq!(v.body, s.body);
        assert_eq!(v.made, s.made);
        assert_eq!(v.filled, s.filled);
        assert_eq!(v.guard.as_deref(), Some("api"));
        assert_ne!(variant(&s, "empty").id, v.id);
    }

    #[test]
    fn pair_file_params() {
        let c = config(
            "params",
            r#"
            [params]
            unit = 5
            code = "A1"
            "api/units/{unit}" = { unit = "7" }
            "GET api/units/{unit}" = { unit = "9" }
            [auth.headers]
            my-project-id = 3
            [run]
            query = { project_id = "4", page = 2 }
            body = { company_id = 8, name = "x", owner_id = true }
            "#,
        );
        let get = |key: &str, param: &str| {
            let uri = key.split_once(' ').unwrap().1;
            override_value(&c, key, uri, param)
        };
        // the route key wins, then the uri, then a value for every route
        assert_eq!(
            get("GET api/units/{unit}", "unit"),
            Some(toml::Value::String("9".into()))
        );
        assert_eq!(
            get("PUT api/units/{unit}", "unit"),
            Some(toml::Value::String("7".into()))
        );
        assert_eq!(
            get("GET api/leases/{unit}", "unit"),
            Some(toml::Value::Integer(5))
        );
        assert_eq!(
            get("GET api/units/{code}", "code"),
            Some(toml::Value::String("A1".into()))
        );
        assert_eq!(get("GET api/units/{lease}", "lease"), None);
        // a route's table is never a value
        assert_eq!(get("GET api/x/{p}", "api/units/{unit}"), None);

        // the header's project first, then *_id fields of [run] query and body that are text or numbers
        assert_eq!(
            default_scope(&c),
            vec![
                ("project_id".to_string(), "3".to_string()),
                ("company_id".to_string(), "8".to_string()),
            ]
        );
        assert!(default_scope(&config("noscope", "")).is_empty());
    }

    #[test]
    fn class_names() {
        let source = "<?php\nnamespace App\\Http\\Controllers;\n\nuse App\\Models\\Unit;\nuse App\\Models;\n";
        assert_eq!(CodeLookup::class_name("Unit", source), "App\\Models\\Unit");
        assert_eq!(
            CodeLookup::class_name("Models\\Lease", source),
            "App\\Models\\Lease"
        );
        assert_eq!(
            CodeLookup::class_name("\\App\\Models\\Shop", source),
            "App\\Models\\Shop"
        );
        assert_eq!(
            CodeLookup::class_name("Helper", source),
            "App\\Http\\Controllers\\Helper"
        );
        assert_eq!(CodeLookup::class_name("Helper", "<?php\n"), "Helper");
    }
}
