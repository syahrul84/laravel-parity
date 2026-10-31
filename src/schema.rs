//! The database itself, old against new, read from information_schema (no app code runs):
//!
//! - **upgrade**: the old app's real database against its copy after the upgrade's migrations ran on it (what the
//!   release will do to production: dropped tables and columns, narrowed types, lost indexes);
//! - **fresh**: the old app's real database against an empty one built by the new app's migrations alone (what a new
//!   install, a CI database or a `RefreshDatabase` test gets). After moving from Propel (or hand-made SQL) to Laravel
//!   migrations, this shows what the migrations do not describe.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use mysql::prelude::Queryable;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, Side};
use crate::ops::{Res, read, write};
use crate::{db, runner, say};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Column {
    #[serde(rename = "type")]
    pub kind: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub extra: String,
    pub collation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Table {
    pub columns: BTreeMap<String, Column>,
    /// "a,b" -> unique; compared by columns, not by name (Propel and Laravel name indexes differently)
    pub indexes: BTreeMap<String, bool>,
    /// "column -> table(column) on delete X on update Y"
    pub foreign_keys: BTreeSet<String>,
    pub collation: Option<String>,
}

pub type Schema = BTreeMap<String, Table>;

/// One difference. `priority` follows the fix list: 2 fix soon, 3 check, 4 know about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub kind: String,
    pub table: String,
    #[serde(default)]
    pub column: Option<String>,
    pub old: String,
    pub new: String,
    pub priority: u8,
}

fn esc(text: &str) -> String {
    text.replace('\'', "''")
}

/// `int(11)` and `int` are the same column (MySQL 8 dropped display widths); `tinyint(1)` is kept (booleans).
pub fn normal_type(kind: &str) -> String {
    static WIDTH: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(smallint|mediumint|int|integer|bigint|tinyint)\(\d+\)").unwrap()
    });
    let kind = kind.trim().to_lowercase();
    if kind.starts_with("tinyint(1)") {
        return kind;
    }
    WIDTH
        .replace(&kind, "$1")
        .replace("integer", "int")
        .replace(" zerofill", "")
}

fn normal_default(value: Option<String>) -> Option<String> {
    value.map(|v| {
        let v = v.trim().trim_matches('\'').to_string();
        let lower = v.to_lowercase();
        if lower.starts_with("current_timestamp") {
            "CURRENT_TIMESTAMP".into()
        } else if lower == "null" {
            "NULL".into()
        } else {
            v
        }
    })
}

fn normal_collation(value: Option<String>) -> Option<String> {
    value.map(|c| c.replace("utf8mb3_", "utf8_"))
}

/// Tables, columns, indexes and foreign keys of one database.
pub fn snapshot(conn: &mut mysql::Conn, database: &str) -> Res<Schema> {
    let db = esc(database);
    let mut schema = Schema::new();
    for row in db::rows(
        conn,
        &format!(
            "SELECT TABLE_NAME, TABLE_COLLATION FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = '{db}' AND TABLE_TYPE = 'BASE TABLE'"
        ),
    )? {
        if let Some(name) = row[0].clone() {
            schema.insert(
                name,
                Table {
                    collation: normal_collation(row[1].clone()),
                    ..Default::default()
                },
            );
        }
    }
    for row in db::rows(
        conn,
        &format!(
            "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, COLLATION_NAME \
             FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{db}' ORDER BY TABLE_NAME, ORDINAL_POSITION"
        ),
    )? {
        let (Some(table), Some(column)) = (row[0].clone(), row[1].clone()) else {
            continue;
        };
        let Some(entry) = schema.get_mut(&table) else {
            continue; // a view
        };
        let extra = row[5]
            .clone()
            .unwrap_or_default()
            .to_lowercase()
            .replace("default_generated", "")
            .trim()
            .to_string();
        entry.columns.insert(
            column,
            Column {
                kind: normal_type(&row[2].clone().unwrap_or_default()),
                nullable: row[3].as_deref() == Some("YES"),
                default: normal_default(row[4].clone()),
                extra,
                collation: normal_collation(row[6].clone()),
            },
        );
    }
    let mut indexes: BTreeMap<(String, String), (bool, Vec<String>)> = BTreeMap::new();
    for row in db::rows(
        conn,
        &format!(
            "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, COLUMN_NAME FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = '{db}' ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX"
        ),
    )? {
        let (Some(table), Some(index)) = (row[0].clone(), row[1].clone()) else {
            continue;
        };
        let entry = indexes
            .entry((table, index))
            .or_insert((row[2].as_deref() == Some("0"), vec![]));
        entry
            .1
            .push(row[3].clone().unwrap_or_else(|| "(expression)".into()));
    }
    for ((table, index), (unique, columns)) in indexes {
        if let Some(entry) = schema.get_mut(&table) {
            let key = if index == "PRIMARY" {
                format!("PRIMARY {}", columns.join(","))
            } else {
                columns.join(",")
            };
            let unique = unique || entry.indexes.get(&key).copied().unwrap_or(false);
            entry.indexes.insert(key, unique);
        }
    }
    for row in db::rows(
        conn,
        &format!(
            "SELECT k.TABLE_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, r.DELETE_RULE, r.UPDATE_RULE \
             FROM information_schema.KEY_COLUMN_USAGE k JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
               ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND r.TABLE_NAME = k.TABLE_NAME \
             WHERE k.TABLE_SCHEMA = '{db}' AND k.REFERENCED_TABLE_NAME IS NOT NULL"
        ),
    )? {
        let text = |i: usize| row[i].clone().unwrap_or_default();
        if let Some(entry) = schema.get_mut(&text(0)) {
            entry.foreign_keys.insert(format!(
                "{} -> {}({}) on delete {} on update {}",
                text(1),
                text(2),
                text(3),
                text(4).to_lowercase(),
                text(5).to_lowercase()
            ));
        }
    }
    Ok(schema)
}

/// (family, size rank, length) of a type, to tell a narrowing (data can be cut) from another change.
fn measure(kind: &str) -> (String, i64, Vec<i64>) {
    static NUMBERS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
    let base = kind
        .split(['(', ' '])
        .next()
        .unwrap_or_default()
        .to_string();
    let lengths: Vec<i64> = kind
        .split_once('(')
        .map(|(_, rest)| {
            NUMBERS
                .find_iter(rest.split(')').next().unwrap_or_default())
                .filter_map(|m| m.as_str().parse().ok())
                .collect()
        })
        .unwrap_or_default();
    let order = |list: &[&str]| list.iter().position(|t| *t == base).map(|p| p as i64);
    let ints = ["tinyint", "smallint", "mediumint", "int", "bigint"];
    let texts = [
        "char",
        "varchar",
        "tinytext",
        "text",
        "mediumtext",
        "longtext",
    ];
    let blobs = [
        "binary",
        "varbinary",
        "tinyblob",
        "blob",
        "mediumblob",
        "longblob",
    ];
    let times = ["date", "datetime", "timestamp"];
    if let Some(p) = order(&ints) {
        ("int".into(), p, vec![])
    } else if let Some(p) = order(&texts) {
        ("text".into(), p.max(1), lengths)
    } else if let Some(p) = order(&blobs) {
        ("blob".into(), p.max(1), lengths)
    } else if order(&times).is_some() {
        ("time".into(), 0, vec![])
    } else {
        (base, 0, lengths)
    }
}

/// Whether going from `old` to `new` can cut or refuse data that fit before.
pub fn narrows(old: &str, new: &str) -> bool {
    let (a, b) = (measure(old), measure(new));
    if old.contains("unsigned") != new.contains("unsigned") {
        return true;
    }
    if a.0 != b.0 {
        // varchar <-> text and the like are the same family; anything else (text -> int) can refuse data
        return !(a.0 == "time" && b.0 == "time");
    }
    if b.1 < a.1 {
        return true;
    }
    if a.0 == "text" && a.1 == 1 && b.1 == 1 {
        // char(n) / varchar(n): shorter cuts
        return b.2.first().unwrap_or(&0) < a.2.first().unwrap_or(&0);
    }
    if a.0 == "decimal" {
        let (p, s) = (
            a.2.first().copied().unwrap_or(10),
            a.2.get(1).copied().unwrap_or(0),
        );
        let (q, t) = (
            b.2.first().copied().unwrap_or(10),
            b.2.get(1).copied().unwrap_or(0),
        );
        return q - t < p - s || t < s;
    }
    if a.0 == "enum" || a.0 == "set" {
        // a value the old list allowed is gone
        let values = |k: &str| -> BTreeSet<String> {
            k.split_once('(')
                .map(|(_, r)| {
                    r.trim_end_matches(')')
                        .split(',')
                        .map(|v| v.trim().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        return !values(old).is_subset(&values(new));
    }
    false
}

/// Tables the tool or Laravel itself keeps; never findings.
fn ignored(table: &str) -> bool {
    table == "migrations"
}

/// What changes from `old` to `new`. `fresh` words priorities for "the migrations alone": what they lack matters
/// less than data a release would drop.
pub fn diff(old: &Schema, new: &Schema, fresh: bool) -> Vec<Change> {
    let mut changes = vec![];
    let mut add =
        |kind: &str, table: &str, column: Option<&str>, a: String, b: String, priority: u8| {
            changes.push(Change {
                kind: kind.into(),
                table: table.into(),
                column: column.map(String::from),
                old: a,
                new: b,
                priority,
            })
        };
    for (name, a) in old.iter().filter(|(n, _)| !ignored(n)) {
        let Some(b) = new.get(name) else {
            add(
                "table-missing",
                name,
                None,
                format!("{} columns", a.columns.len()),
                String::new(),
                2,
            );
            continue;
        };
        for (column, x) in &a.columns {
            let Some(y) = b.columns.get(column) else {
                add(
                    "column-missing",
                    name,
                    Some(column),
                    x.kind.clone(),
                    String::new(),
                    2,
                );
                continue;
            };
            if x.kind != y.kind {
                let lossy = narrows(&x.kind, &y.kind);
                add(
                    if lossy {
                        "type-narrowed"
                    } else {
                        "type-changed"
                    },
                    name,
                    Some(column),
                    x.kind.clone(),
                    y.kind.clone(),
                    if lossy && !fresh { 2 } else { 3 },
                );
            }
            if x.nullable && !y.nullable {
                add(
                    "now-not-null",
                    name,
                    Some(column),
                    "NULL allowed".into(),
                    "NOT NULL".into(),
                    3,
                );
            } else if !x.nullable && y.nullable {
                add(
                    "now-nullable",
                    name,
                    Some(column),
                    "NOT NULL".into(),
                    "NULL allowed".into(),
                    4,
                );
            }
            if x.default != y.default {
                let show = |d: &Option<String>| d.clone().unwrap_or_else(|| "none".into());
                add(
                    "default-changed",
                    name,
                    Some(column),
                    show(&x.default),
                    show(&y.default),
                    3,
                );
            }
            if x.extra.contains("auto_increment") != y.extra.contains("auto_increment") {
                add(
                    "auto-increment",
                    name,
                    Some(column),
                    x.extra.clone(),
                    y.extra.clone(),
                    2,
                );
            }
            if x.collation.is_some() && y.collation.is_some() && x.collation != y.collation {
                add(
                    "collation",
                    name,
                    Some(column),
                    x.collation.clone().unwrap_or_default(),
                    y.collation.clone().unwrap_or_default(),
                    4,
                );
            }
        }
        for (index, unique) in &a.indexes {
            match b.indexes.get(index) {
                None if *unique => add(
                    "unique-missing",
                    name,
                    Some(index),
                    "unique".into(),
                    String::new(),
                    if fresh { 3 } else { 2 },
                ),
                None => add(
                    "index-missing",
                    name,
                    Some(index),
                    "index".into(),
                    String::new(),
                    3,
                ),
                Some(false) if *unique => add(
                    "unique-missing",
                    name,
                    Some(index),
                    "unique".into(),
                    "plain index".into(),
                    if fresh { 3 } else { 2 },
                ),
                _ => {}
            }
        }
        for key in a.foreign_keys.difference(&b.foreign_keys) {
            add(
                "foreign-key-missing",
                name,
                None,
                key.clone(),
                String::new(),
                if fresh { 4 } else { 3 },
            );
        }
    }
    for (name, b) in new.iter().filter(|(n, _)| !ignored(n)) {
        if let Some(a) = old.get(name) {
            for column in b.columns.keys().filter(|c| !a.columns.contains_key(*c)) {
                let y = &b.columns[column];
                // a new NOT NULL column without a default refuses the old code's inserts
                let priority =
                    if !y.nullable && y.default.is_none() && !y.extra.contains("auto_increment") {
                        3
                    } else {
                        4
                    };
                add(
                    "column-added",
                    name,
                    Some(column),
                    String::new(),
                    y.kind.clone(),
                    priority,
                );
            }
        } else {
            add(
                "table-added",
                name,
                None,
                String::new(),
                format!("{} columns", b.columns.len()),
                4,
            );
        }
    }
    changes
}

/// A mysqldump file as single statements (quotes, comments and DELIMITER lines handled).
pub fn statements(sql: &str) -> Vec<String> {
    let mut out = vec![];
    let mut current = String::new();
    let mut delimiter = ";".to_string();
    for line in sql.lines() {
        let trimmed = line.trim();
        if let Some(d) = trimmed
            .strip_prefix("DELIMITER ")
            .or_else(|| trimmed.strip_prefix("delimiter "))
        {
            delimiter = d.trim().to_string();
            continue;
        }
        if current.is_empty()
            && (trimmed.is_empty() || trimmed.starts_with("--") || trimmed.starts_with('#'))
        {
            continue;
        }
        current.push_str(line);
        current.push('\n');
        if trimmed.ends_with(delimiter.as_str()) && !inside_quote(&current) {
            let statement = current
                .trim()
                .trim_end_matches(delimiter.as_str())
                .trim()
                .to_string();
            if !statement.is_empty() {
                out.push(statement);
            }
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn inside_quote(text: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(_), '\\') => {
                chars.next();
            }
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"' | '`') => quote = Some(c),
            _ => {}
        }
    }
    quote.is_some()
}

/// The schema dump Laravel loads before migrating an empty database (database/schema/<connection>-schema.sql).
fn schema_dump(side: &Side) -> Option<std::path::PathBuf> {
    let dir = side.repo.join("database/schema");
    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with("-schema.sql"))
        .collect();
    found.sort_by_key(|p| !p.to_string_lossy().ends_with("mysql-schema.sql"));
    found.into_iter().next()
}

/// The new app's migrations on an empty database: the dump (if the repo has one) loaded by the tool, then
/// `migrate`. Returns the error text when the migrations fail.
fn build_fresh(config: &Config, fresh: &Side) -> Res<Option<String>> {
    let mut conn = db::connect(config, fresh)?;
    let name = format!("`{}`", fresh.database.replace('`', "``"));
    conn.query_drop(format!("DROP DATABASE IF EXISTS {name}"))
        .and_then(|_| conn.query_drop(format!("CREATE DATABASE {name}")))
        .and_then(|_| conn.query_drop(format!("USE {name}")))
        .map_err(|e| e.to_string())?;
    if let Some(dump) = schema_dump(fresh) {
        let text =
            std::fs::read_to_string(&dump).map_err(|e| format!("{}: {e}", dump.display()))?;
        let mut failed = 0;
        let list = statements(&text);
        for statement in &list {
            if conn.query_drop(statement).is_err() {
                failed += 1;
            }
        }
        say(&format!(
            "  loaded {} ({} statements{})",
            dump.strip_prefix(&fresh.repo).unwrap_or(&dump).display(),
            list.len(),
            if failed > 0 {
                format!(", {failed} failed")
            } else {
                String::new()
            }
        ));
    }
    drop(conn);
    // the pair file's migrate commands, without what only makes sense on the copy
    let mut commands: Vec<Value> = fresh
        .setup
        .iter()
        .filter(|c| c[0].as_str().is_some_and(|n| n.starts_with("migrate")))
        .cloned()
        .collect();
    if commands.is_empty() {
        commands.push(json!(["migrate", {"--force": true}]));
    }
    for command in &mut commands {
        // the tool loaded the dump itself (Laravel would need a mysql client in the container)
        if let Some(options) = command.get_mut(1).and_then(Value::as_object_mut) {
            options.insert(
                "--schema-path".into(),
                json!("/nonexistent/no-schema-dump.sql"),
            );
            options.insert("--force".into(), json!(true));
        } else if let Some(list) = command.as_array_mut() {
            list.push(json!({"--force": true, "--schema-path": "/nonexistent/no-schema-dump.sql"}));
        }
    }
    for command in commands {
        let rows = runner::run_job(
            config,
            fresh,
            &json!({"mode": "artisan", "commands": [command]}),
            0,
        )?;
        for row in rows.iter().filter(|r| r.get("command").is_some()) {
            if row["exit"].as_i64() != Some(0) {
                let output = row["output"].as_str().unwrap_or_default().trim();
                let tail: String = output
                    .chars()
                    .rev()
                    .take(1500)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                return Ok(Some(tail));
            }
        }
    }
    Ok(None)
}

/// The fresh database's name: the new test database's, with `_fresh`.
pub fn fresh_name(config: &Config) -> String {
    format!("{}_fresh", config.new.database)
}

pub fn check(config: &Config) -> Res<Value> {
    say(&format!(
        "Reading the schema of {} (the old app's database) ...",
        config.source_db
    ));
    let mut source_conn = db::connect(config, &config.old)?;
    let source = snapshot(&mut source_conn, &config.source_db)?;
    let mut new_conn = db::connect(config, &config.new)?;
    let upgraded = snapshot(&mut new_conn, &config.new.database)?;
    if upgraded.is_empty() {
        return Err(format!(
            "{} is empty: run setup-db first",
            config.new.database
        ));
    }
    let upgrade = diff(&source, &upgraded, false);

    let mut fresh_side = config.new.clone();
    fresh_side.database = fresh_name(config);
    say(&format!(
        "Building {} from the new app's migrations alone ...",
        fresh_side.database
    ));
    runner::ensure_ready(&fresh_side)?;
    let failure = build_fresh(config, &fresh_side)?;
    let (fresh, fresh_changes) = match &failure {
        Some(_) => (Schema::new(), vec![]),
        None => {
            let built = snapshot(&mut new_conn, &fresh_side.database)?;
            let changes = diff(&source, &built, true);
            (built, changes)
        }
    };
    let count = |list: &[Change], p: u8| list.iter().filter(|c| c.priority <= p).count();
    let line = format!(
        "Database: the upgrade's migrations change {} things in the old database ({} to fix or check); a database \
         built from the migrations alone {}",
        upgrade.len(),
        count(&upgrade, 3),
        match &failure {
            Some(_) => "cannot be built: the migrations fail on an empty database".to_string(),
            None => format!(
                "differs in {} things ({} to fix or check; {} tables)",
                fresh_changes.len(),
                count(&fresh_changes, 3),
                fresh.len()
            ),
        }
    );
    say(&line);
    let result = json!({
        "source": config.source_db,
        "upgraded": config.new.database,
        "fresh_database": fresh_side.database,
        "tables": {"source": source.len(), "upgraded": upgraded.len(), "fresh": fresh.len()},
        "upgrade": upgrade,
        "fresh": fresh_changes,
        "fresh_failed": failure,
        "summary": line,
    });
    write(&config.work.join("schema.json"), &result)?;
    // the fix list picks this up (when there are results to build it from)
    let _ = crate::fixlist::write_all(config);
    Ok(result)
}

pub fn last(config: &Config) -> Option<Value> {
    read(&config.work.join("schema.json"))
}

/// What each kind of change means, for the fix list: (title for the upgrade, title for the migrations alone, hint).
fn wording(kind: &str) -> (&'static str, &'static str, &'static str) {
    match kind {
        "table-missing" => (
            "tables the upgrade's migrations drop: their data is gone after the release",
            "tables of the old database that the migrations never create",
            "Keep the table (or migrate its data first). For the migrations alone: add a create migration, or regenerate the schema dump.",
        ),
        "column-missing" => (
            "columns the upgrade's migrations drop: their data is gone after the release",
            "columns of the old database that the migrations never create",
            "Code that reads or writes them fails with Unknown column. Add them to a migration, or remove the code that uses them.",
        ),
        "type-narrowed" => (
            "columns get a narrower type: existing values can be cut or refused",
            "columns the migrations create narrower than the old database has them",
            "Values that fit before (long text, big numbers, an enum value, negative numbers) are truncated or refused. Widen the column in the migration.",
        ),
        "type-changed" => (
            "columns change type",
            "columns the migrations create with another type",
            "Usually harmless (a wider type), but comparisons and JSON output can change: check the code that reads them.",
        ),
        "now-not-null" => (
            "columns become NOT NULL: inserts that leave them out fail",
            "columns the migrations make NOT NULL where the old database allows NULL",
            "Give the column a default or ->nullable(), or make every insert fill it.",
        ),
        "now-nullable" => (
            "columns now allow NULL",
            "columns the migrations make nullable",
            "Code that assumed a value can now get null.",
        ),
        "default-changed" => (
            "columns get another default value",
            "columns the migrations give another default",
            "Rows inserted without the column get a different value than before.",
        ),
        "auto-increment" => (
            "columns gain or lose AUTO_INCREMENT",
            "columns whose AUTO_INCREMENT differs in the migrations",
            "Inserts without an id fail (or ids are no longer generated): match the old column.",
        ),
        "collation" => (
            "columns change collation",
            "columns the migrations create with another collation",
            "Sorting and case-insensitive comparisons can change; utf8 -> utf8mb4 is usually intended.",
        ),
        "unique-missing" => (
            "unique keys removed: duplicates are now accepted",
            "unique keys of the old database the migrations never create",
            "Add ->unique() back, or the app can store duplicate rows it used to refuse.",
        ),
        "index-missing" => (
            "indexes removed: queries filtering on these columns get slower",
            "indexes of the old database the migrations never create",
            "Add ->index() in a migration; production data is large where test data is not.",
        ),
        "foreign-key-missing" => (
            "foreign keys removed",
            "foreign keys of the old database the migrations never create",
            "Deletes no longer cascade or get refused the way they did. Add ->foreign() / ->constrained() if the app relied on them.",
        ),
        "column-added" => (
            "new columns",
            "columns only the migrations have",
            "A NOT NULL column without a default makes the old inserts fail; give it a default or ->nullable().",
        ),
        _ => (
            "new tables",
            "tables only the migrations create",
            "Expected for new features.",
        ),
    }
}

/// The schema check as fix-list items (area Database).
pub fn fixes(config: &Config) -> Vec<crate::fixlist::Fix> {
    let Some(found) = last(config) else {
        return vec![];
    };
    let mut out = vec![];
    if let Some(error) = found["fresh_failed"].as_str() {
        out.push(crate::fixlist::Fix {
            priority: 2,
            area: "Database".into(),
            title: "The new app's migrations fail on an empty database (a new install, CI or a RefreshDatabase test cannot build it)".into(),
            location: None,
            hint: Some("Run `php artisan migrate` on an empty database to see it; often a migration that expects a table the old database already had.".into()),
            routes: vec![],
            details: vec![error.chars().take(1200).collect()],
        });
    }
    for (which, fresh) in [("upgrade", false), ("fresh", true)] {
        let changes: Vec<Change> = serde_json::from_value(found[which].clone()).unwrap_or_default();
        let mut groups: BTreeMap<&str, Vec<&Change>> = BTreeMap::new();
        for change in &changes {
            groups.entry(change.kind.as_str()).or_default().push(change);
        }
        for (kind, list) in groups {
            let priority = list.iter().map(|c| c.priority).min().unwrap_or(4);
            let (upgrade, alone, hint) = wording(kind);
            let title = if fresh {
                format!("Migrations alone: {} {alone}", list.len())
            } else {
                format!("{} {upgrade}", list.len())
            };
            let details = list
                .iter()
                .map(|c| {
                    let place = match &c.column {
                        Some(col) => format!("{}.{col}", c.table),
                        None => c.table.clone(),
                    };
                    match (c.old.is_empty(), c.new.is_empty()) {
                        (false, false) => format!("{place}: {} -> {}", c.old, c.new),
                        (false, true) => format!("{place} ({})", c.old),
                        (true, false) => format!("{place} ({})", c.new),
                        _ => place,
                    }
                })
                .collect();
            out.push(crate::fixlist::Fix {
                priority,
                area: "Database".into(),
                title,
                location: None,
                hint: Some(hint.into()),
                routes: vec![],
                details,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types() {
        assert_eq!(normal_type("INT(11) unsigned"), "int unsigned");
        assert_eq!(normal_type("tinyint(1)"), "tinyint(1)");
        assert!(narrows("varchar(255)", "varchar(100)"));
        assert!(!narrows("varchar(100)", "varchar(255)"));
        assert!(!narrows("varchar(255)", "text"));
        assert!(narrows("text", "varchar(255)"));
        assert!(narrows("bigint", "int"));
        assert!(!narrows("int", "bigint"));
        assert!(narrows("int unsigned", "int"));
        assert!(narrows("decimal(10,2)", "decimal(8,2)"));
        assert!(narrows("enum('a','b')", "enum('a')"));
        assert!(!narrows("datetime", "timestamp"));
    }

    #[test]
    fn dump_statements() {
        let sql = "-- c\n/*!40101 SET x */;\nCREATE TABLE `a` (\n `b` varchar(5) DEFAULT 'x;y'\n);\nDELIMITER ;;\nCREATE TRIGGER t BEGIN SET @a = 1; END ;;\nDELIMITER ;\nINSERT INTO m VALUES (1);\n";
        let list = statements(sql);
        assert_eq!(list.len(), 4);
        assert!(list[1].contains("'x;y'"));
        assert!(list[2].contains("SET @a = 1; END"));
    }

    #[test]
    fn findings() {
        let mut old = Schema::new();
        let mut table = Table::default();
        table.columns.insert(
            "name".into(),
            Column {
                kind: "varchar(255)".into(),
                ..Default::default()
            },
        );
        table.columns.insert(
            "gone".into(),
            Column {
                kind: "int".into(),
                ..Default::default()
            },
        );
        table.indexes.insert("name".into(), true);
        old.insert("units".into(), table.clone());
        let mut new = Schema::new();
        table.columns.remove("gone");
        table.columns.get_mut("name").unwrap().kind = "varchar(100)".into();
        table.indexes.clear();
        new.insert("units".into(), table);
        let kinds: Vec<String> = diff(&old, &new, false)
            .into_iter()
            .map(|c| c.kind)
            .collect();
        assert_eq!(
            kinds,
            vec!["column-missing", "type-narrowed", "unique-missing"]
        );
    }
}
