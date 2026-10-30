//! MySQL straight from the tool (no mysql / mysqldump client needed): copy the old app's database into the two test
//! databases, and look up sample ids for route parameters.

use std::collections::BTreeMap;

use mysql::prelude::Queryable;
use mysql::{Conn, OptsBuilder, Value};

use crate::config::{Config, Side};

/// A view's or trigger's DEFINER, which may not exist on the target server.
static DEFINER: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"DEFINER=\S+ ").unwrap());
use crate::say;

pub fn connect(config: &Config, side: &Side) -> Result<Conn, String> {
    let opts = OptsBuilder::new()
        .ip_or_hostname(Some(side.db_host.clone()))
        .tcp_port(side.db_port)
        .user(Some(config.db_user.clone()))
        .pass(Some(config.db_password.clone()));
    Conn::new(opts).map_err(|e| format!("MySQL {}:{}: {e}", side.db_host, side.db_port))
}

fn quote(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// A value as text; NULL is None.
pub fn text(value: &Value) -> Option<String> {
    match value {
        Value::NULL => None,
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::Int(i) => Some(i.to_string()),
        Value::UInt(u) => Some(u.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Double(d) => Some(d.to_string()),
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

pub fn rows(conn: &mut Conn, sql: &str) -> Result<Vec<Vec<Option<String>>>, String> {
    let result: Vec<mysql::Row> = conn.query(sql).map_err(|e| format!("{e} (in: {sql})"))?;
    Ok(result
        .into_iter()
        .map(|row| row.unwrap().iter().map(text).collect())
        .collect())
}

fn same_server(a: &Side, b: &Side) -> bool {
    a.db_host == b.db_host && a.db_port == b.db_port
}

/// Copy the old app's database into both test databases. The source is only read.
pub fn copy_source(config: &Config) -> Result<(), String> {
    let mut source = connect(config, &config.old)?;
    let src = quote(&config.source_db);
    let tables = rows(
        &mut source,
        &format!(
            "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = '{}' ORDER BY TABLE_NAME",
            config.source_db.replace('\'', "''")
        ),
    )?;
    let base: Vec<String> = tables
        .iter()
        .filter(|r| r[1].as_deref() == Some("BASE TABLE"))
        .filter_map(|r| r[0].clone())
        .collect();
    let views: Vec<String> = tables
        .iter()
        .filter(|r| r[1].as_deref() == Some("VIEW"))
        .filter_map(|r| r[0].clone())
        .collect();
    if base.is_empty() {
        return Err(format!(
            "{} has no tables (or the user cannot see them)",
            config.source_db
        ));
    }

    for side in [&config.old, &config.new] {
        say(&format!(
            "  {} <- {} ({} tables)",
            side.database,
            config.source_db,
            base.len()
        ));
        let mut target = connect(config, side)?;
        let dst = quote(&side.database);
        let session = "SET SESSION FOREIGN_KEY_CHECKS = 0, SESSION UNIQUE_CHECKS = 0, SESSION sql_mode = 'NO_AUTO_VALUE_ON_ZERO'";
        target.query_drop(session).map_err(|e| e.to_string())?;
        target
            .query_drop(format!("DROP DATABASE IF EXISTS {dst}"))
            .map_err(|e| e.to_string())?;
        target
            .query_drop(format!("CREATE DATABASE {dst}"))
            .map_err(|e| e.to_string())?;
        target
            .query_drop(format!("USE {dst}"))
            .map_err(|e| e.to_string())?;
        let server_side = same_server(&config.old, side);

        for (i, table) in base.iter().enumerate() {
            let create = rows(
                &mut source,
                &format!("SHOW CREATE TABLE {src}.{}", quote(table)),
            )?;
            let ddl = create
                .first()
                .and_then(|r| r.get(1).cloned().flatten())
                .ok_or(format!("no DDL for {table}"))?;
            target
                .query_drop(&ddl)
                .map_err(|e| format!("{table}: {e}"))?;
            let columns = insertable_columns(&mut source, &config.source_db, table)?;
            if server_side {
                // one statement on the server: no rows travel through the tool
                target
                    .query_drop(format!(
                        "INSERT INTO {dst}.{t} ({c}) SELECT {c} FROM {src}.{t}",
                        t = quote(table),
                        c = columns
                    ))
                    .map_err(|e| format!("{table}: {e}"))?;
            } else {
                stream_rows(&mut source, &mut target, &src, table, &columns)?;
            }
            if (i + 1) % 100 == 0 {
                say(&format!("    {}/{} tables", i + 1, base.len()));
            }
        }

        // views can depend on other views: create them until no more can be made
        let mut pending: Vec<String> = views.clone();
        while !pending.is_empty() {
            let before = pending.len();
            let mut still = vec![];
            for view in pending {
                let create = rows(
                    &mut source,
                    &format!("SHOW CREATE VIEW {src}.{}", quote(&view)),
                )?;
                let ddl = create
                    .first()
                    .and_then(|r| r.get(1).cloned().flatten())
                    .unwrap_or_default();
                // the DEFINER may not exist on the target server
                let ddl = DEFINER
                    .clone()
                    .replace(&ddl, "")
                    .replace(&format!("{src}."), "");
                if target.query_drop(&ddl).is_err() {
                    still.push(view);
                }
            }
            if still.len() == before {
                say(&format!("    views not copied: {}", still.join(", ")));
                break;
            }
            pending = still;
        }

        for trigger in rows(&mut source, &format!("SHOW TRIGGERS FROM {src}"))? {
            let name = trigger[0].clone().unwrap_or_default();
            let create = rows(
                &mut source,
                &format!("SHOW CREATE TRIGGER {src}.{}", quote(&name)),
            )?;
            if let Some(ddl) = create.first().and_then(|r| r.get(2).cloned().flatten()) {
                let ddl = DEFINER.clone().replace(&ddl, "").into_owned();
                if let Err(e) = target.query_drop(&ddl) {
                    say(&format!("    trigger {name} not copied: {e}"));
                }
            }
        }
    }
    Ok(())
}

/// The columns an INSERT may fill (generated columns are computed by the server).
fn insertable_columns(conn: &mut Conn, schema: &str, table: &str) -> Result<String, String> {
    let cols = rows(
        conn,
        &format!(
            "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}' \
             AND EXTRA NOT LIKE '%GENERATED%' ORDER BY ORDINAL_POSITION",
            schema.replace('\'', "''"),
            table.replace('\'', "''")
        ),
    )?;
    Ok(cols
        .into_iter()
        .filter_map(|r| r[0].clone())
        .map(|c| quote(&c))
        .collect::<Vec<_>>()
        .join(", "))
}

/// Rows from one server to another, 500 per INSERT.
fn stream_rows(
    source: &mut Conn,
    target: &mut Conn,
    src: &str,
    table: &str,
    columns: &str,
) -> Result<(), String> {
    let count = columns.split(", ").count();
    let row_marks = format!("({})", vec!["?"; count].join(", "));
    let select = format!("SELECT {columns} FROM {src}.{}", quote(table));
    let result = source
        .query_iter(&select)
        .map_err(|e| format!("{table}: {e}"))?;
    let mut batch: Vec<Value> = vec![];
    let mut in_batch = 0;
    let mut pending: Vec<(String, Vec<Value>)> = vec![];
    for row in result {
        let row = row.map_err(|e| format!("{table}: {e}"))?;
        batch.extend(row.unwrap());
        in_batch += 1;
        if in_batch == 500 {
            let marks = vec![row_marks.as_str(); in_batch].join(", ");
            pending.push((
                format!("INSERT INTO {} ({columns}) VALUES {marks}", quote(table)),
                std::mem::take(&mut batch),
            ));
            in_batch = 0;
        }
    }
    if in_batch > 0 {
        let marks = vec![row_marks.as_str(); in_batch].join(", ");
        pending.push((
            format!("INSERT INTO {} ({columns}) VALUES {marks}", quote(table)),
            batch,
        ));
    }
    for (sql, values) in pending {
        target
            .exec_drop(sql, values)
            .map_err(|e| format!("{table}: {e}"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct TableInfo {
    pub columns: Vec<String>,
    pub pk: Option<String>,
}

/// Table name -> columns and primary key, of a test database.
pub fn tables(config: &Config, side: &Side) -> Result<BTreeMap<String, TableInfo>, String> {
    let mut conn = connect(config, side)?;
    let mut found: BTreeMap<String, TableInfo> = BTreeMap::new();
    for row in rows(
        &mut conn,
        &format!(
            "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_KEY FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = '{}' \
             ORDER BY TABLE_NAME, ORDINAL_POSITION",
            side.database.replace('\'', "''")
        ),
    )? {
        let (Some(table), Some(column)) = (row[0].clone(), row[1].clone()) else {
            continue;
        };
        let entry = found.entry(table).or_default();
        if row[2].as_deref() == Some("PRI") && entry.pk.is_none() {
            entry.pk = Some(column.clone());
        }
        entry.columns.push(column);
    }
    Ok(found)
}
