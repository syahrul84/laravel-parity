//! Factories for the models that use HasFactory but have none (Laravel 8+ factory classes), so the generated tests
//! can make the rows a route or a method needs. Each required column (NOT NULL, no default, not auto-increment) gets a
//! value of its type; a column that points at another model (belongsTo, or a foreign key) gets that model's newest
//! row, else one from its factory. Every factory is checked by making one row on the new app, rolled back; a database
//! error ("Field 'x' doesn't have a default value", "Data too long", a duplicate) fixes that column and the check runs
//! again. A factory that still cannot make a row is not kept. Factories the tool did not write are only checked.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::config::Config;
use crate::ops::{Res, write};
use crate::schema::{self, Column, Table};
use crate::testgen::{self, MARKER, NoFactory, php_string};
use crate::{db, say, units};

/// Rounds of "make a row, fix the column the database named".
const ROUNDS: usize = 4;

pub struct Outcome {
    /// model class -> factory file (in the new repo), made and checked
    pub written: Vec<(String, PathBuf)>,
    /// model class -> why its factory could not make a row (not kept)
    pub failed: Vec<(String, String)>,
    /// the app's own factories that cannot make a row: model class -> error
    pub broken: Vec<(String, String)>,
    pub note: Option<String>,
}

pub fn last(config: &Config) -> Option<Value> {
    crate::ops::read(&config.work.join("factories.json"))
}

/// The factory class of a model: App\Models\Sub\Unit -> (Database\Factories\Sub, UnitFactory, database/factories/Sub).
fn factory_place(model: &str) -> (String, String, PathBuf) {
    let rest = model
        .strip_prefix("App\\Models\\")
        .or_else(|| model.strip_prefix("App\\"))
        .unwrap_or(model);
    let mut parts: Vec<&str> = rest.split('\\').collect();
    let short = parts.pop().unwrap_or(rest);
    let mut namespace = "Database\\Factories".to_string();
    let mut dir = PathBuf::from("database/factories");
    for p in &parts {
        namespace.push('\\');
        namespace.push_str(p);
        dir.push(p);
    }
    (namespace, format!("{short}Factory"), dir)
}

/// The value of a required column of this type.
fn value(name: &str, column: &Column, unique: bool) -> String {
    static LENGTH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\w+\((\d+)").unwrap());
    static OPTION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:enum|set)\('((?:[^']|'')*)'").unwrap());
    let kind = column.kind.as_str();
    let base = kind.split(['(', ' ']).next().unwrap_or(kind);
    let length = LENGTH
        .captures(kind)
        .and_then(|c| c[1].parse::<usize>().ok())
        .unwrap_or(255);
    let name = name.to_lowercase();
    match base {
        _ if kind.starts_with("tinyint(1)") || base == "boolean" || base == "bool" => {
            "false".into()
        }
        "tinyint" | "smallint" | "mediumint" | "int" | "bigint" | "decimal" | "numeric"
        | "float" | "double" | "real" => {
            if unique {
                let top = if base == "tinyint" { 127 } else { 30000 };
                format!("$this->faker->unique()->numberBetween(1, {top})")
            } else if base == "decimal" || base == "float" || base == "double" || base == "real" {
                "0".into()
            } else {
                "1".into()
            }
        }
        "char" | "varchar" if length == 36 || name.ends_with("uuid") => {
            "(string) \\Illuminate\\Support\\Str::uuid()".into()
        }
        "char" | "varchar" if name.contains("email") && length >= 40 => {
            if unique {
                "$this->faker->unique()->safeEmail()".into()
            } else {
                "$this->faker->safeEmail()".into()
            }
        }
        "char" | "varchar" => {
            if unique {
                format!("\\Illuminate\\Support\\Str::random({})", length.min(16))
            } else {
                format!("substr($this->faker->word(), 0, {length})")
            }
        }
        "tinytext" | "text" | "mediumtext" | "longtext" => "$this->faker->sentence()".into(),
        "date" => "now()->toDateString()".into(),
        "datetime" | "timestamp" => "now()".into(),
        "time" => "'00:00:00'".into(),
        "year" => "now()->year".into(),
        "enum" | "set" => OPTION
            .captures(kind)
            .map(|c| php_string(&c[1].replace("''", "'")))
            .unwrap_or_else(|| "''".into()),
        "json" => "'[]'".into(),
        _ => "''".into(),
    }
}

/// The model a column points at: (related class, related table) from the model's belongsTo relations, else the
/// foreign key's table.
fn relations(model: &NoFactory) -> HashMap<String, String> {
    static BELONGS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"function\s+(\w+)\s*\([^)]*\)[^{]*\{[^}]*?belongsTo\(\s*(?:\\?([\w\\]+)::class|['"]\\?([\w\\]+)['"])\s*(?:,\s*['"](\w+)['"])?"#,
        )
        .unwrap()
    });
    let text = std::fs::read_to_string(&model.file).unwrap_or_default();
    let namespace = model.class.rsplit_once('\\').map_or("", |p| p.0);
    let mut out = HashMap::new();
    for cap in BELONGS.captures_iter(&text) {
        let named = cap.get(2).or(cap.get(3)).map_or("", |m| m.as_str());
        let class = if named.contains('\\') {
            named.trim_start_matches('\\').to_string()
        } else if let Some(full) = model.uses.get(named) {
            full.clone()
        } else {
            format!("{namespace}\\{named}")
        };
        let column = cap
            .get(4)
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| format!("{}_id", crate::plan::snake(&cap[1])));
        out.entry(column).or_insert(class);
    }
    out
}

/// The expression for a column that points at another model: its first row, else a new one from its factory.
fn related(class: &str, own: &str, has_factory: bool) -> String {
    let row = format!("optional(\\{class}::query()->first())->getKey()");
    if class == own || !has_factory {
        format!("function () {{ return {row} ?? 1; }}")
    } else {
        format!("function () {{ return {row} ?? \\{class}::factory()->create()->getKey(); }}")
    }
}

/// column -> PHP expression for the columns a row cannot be saved without.
fn definition(
    model: &NoFactory,
    table: &Table,
    links: &HashMap<String, String>,
    factory_models: &HashSet<String>,
    by_table: &HashMap<String, String>,
) -> BTreeMap<String, String> {
    let unique: HashSet<&str> = table
        .indexes
        .iter()
        .filter(|(cols, unique)| **unique && !cols.starts_with("PRIMARY") && !cols.contains(','))
        .map(|(cols, _)| cols.as_str())
        .collect();
    let foreign: HashMap<String, String> = table
        .foreign_keys
        .iter()
        .filter_map(|fk| {
            // "column -> table(column) on delete X on update Y"
            let (column, rest) = fk.split_once(" -> ")?;
            let target = rest.split('(').next()?.trim();
            Some((column.trim().to_string(), target.to_string()))
        })
        .collect();
    let mut out = BTreeMap::new();
    for (name, column) in &table.columns {
        let generated = column.extra.contains("auto_increment")
            || column.extra.contains("generated")
            || column.extra.contains("virtual")
            || column.extra.contains("stored");
        let required = !column.nullable && column.default.is_none() && !generated;
        if !required || ["created_at", "updated_at", "deleted_at"].contains(&name.as_str()) {
            continue;
        }
        let points_at = links
            .get(name)
            .cloned()
            .or_else(|| foreign.get(name).and_then(|t| by_table.get(t).cloned()));
        let expression = match points_at {
            Some(class) => related(&class, &model.class, factory_models.contains(&class)),
            None => value(name, column, unique.contains(name.as_str())),
        };
        out.insert(name.clone(), expression);
    }
    out
}

fn render(model: &str, columns: &BTreeMap<String, String>) -> (PathBuf, String) {
    let (namespace, class, dir) = factory_place(model);
    let mut out = vec![
        "<?php".to_string(),
        String::new(),
        format!("namespace {namespace};"),
        String::new(),
        "use Illuminate\\Database\\Eloquent\\Factories\\Factory;".into(),
        String::new(),
        "/**".into(),
        format!(" * {MARKER}: the columns a {model} row cannot be saved without (NOT NULL, no default), checked by"),
        " * making a row. Regenerating replaces this file; once you edit it, remove this marker so the tool leaves it".into(),
        " * alone.".into(),
        " */".into(),
        format!("class {class} extends Factory"),
        "{".into(),
        format!("    protected $model = \\{model}::class;"),
        String::new(),
        "    public function definition()".into(),
        "    {".into(),
        "        return [".into(),
    ];
    for (column, expression) in columns {
        out.push(format!(
            "            {} => {expression},",
            php_string(column)
        ));
    }
    out.push("        ];".into());
    out.push("    }".into());
    out.push("}".into());
    (dir.join(format!("{class}.php")), out.join("\n") + "\n")
}

/// Change the definition by what the database said; false when the error is not one a column change fixes.
fn fix(columns: &mut BTreeMap<String, String>, table: &Table, error: &str) -> bool {
    static FIELD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:Field|Column) '(\w+)' (doesn't have a default value|cannot be null)")
            .unwrap()
    });
    static LONG: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"Data too long for column '(\w+)'").unwrap());
    static INCORRECT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"Incorrect (\w+) value: .*? for column (?:`[^`]*`\.)*[`'](\w+)[`']").unwrap()
    });
    static FOREIGN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"FOREIGN KEY \(`(\w+)`\) REFERENCES `(\w+)` \(`(\w+)`\)").unwrap()
    });
    static DUPLICATE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"Duplicate entry '.*' for key '([\w.]+)'").unwrap());
    let set = |columns: &mut BTreeMap<String, String>, name: &str, expression: String| {
        if columns.get(name) == Some(&expression) {
            return false;
        }
        columns.insert(name.to_string(), expression);
        true
    };
    if let Some(c) = FIELD.captures(error) {
        let Some(column) = table.columns.get(&c[1]) else {
            return false;
        };
        return set(columns, &c[1], value(&c[1], column, false));
    }
    if let Some(c) = LONG.captures(error) {
        return set(columns, &c[1], "'a'".into());
    }
    if let Some(c) = INCORRECT.captures(error) {
        let expression = match &c[1] {
            "date" | "datetime" | "time" => "now()",
            "integer" | "decimal" | "double" => "0",
            _ => "''",
        };
        return set(columns, &c[2], expression.into());
    }
    if let Some(c) = FOREIGN.captures(error) {
        return set(
            columns,
            &c[1],
            format!(
                "function () {{ return \\Illuminate\\Support\\Facades\\DB::table({})->value({}); }}",
                php_string(&c[2]),
                php_string(&c[3])
            ),
        );
    }
    if let Some(c) = DUPLICATE.captures(error) {
        // the key is named after its columns (table_column_unique); make those unique
        let key = c[1].rsplit('.').next().unwrap_or(&c[1]).to_string();
        let mut changed = false;
        for (name, column) in &table.columns {
            if key.contains(name.as_str()) && columns.contains_key(name) {
                changed |= set(columns, name, value(name, column, true));
            }
        }
        return changed;
    }
    false
}

fn short(error: &str) -> String {
    let error = error.lines().next().unwrap_or(error);
    // the SQL after the message says nothing more and can be long
    let error = error.split(" (Connection: ").next().unwrap_or(error);
    error.chars().take(240).collect()
}

/// Make one row with each factory on the new app (rolled back): model class -> error, for the ones that failed.
fn check(config: &Config, models: &[String]) -> HashMap<String, String> {
    let clock = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let items: Vec<Value> = models
        .iter()
        .map(|m| {
            json!({"id": format!("factory:{m}"), "class": "", "method": "",
                   "code": format!("return \\{m}::factory()->create()->getKey();")})
        })
        .collect();
    let mut failed = HashMap::new();
    for row in units::run_side(config, &config.new, &items, &clock) {
        let id = row["id"].as_str().unwrap_or_default();
        let Some(model) = id.strip_prefix("factory:") else {
            continue;
        };
        if row["status"] != "ok" {
            let error = row["error"].as_str().unwrap_or("no answer").to_string();
            failed.insert(model.to_string(), short(&error));
        }
    }
    failed
}

/// Write and check the missing factories; check the app's own. Writes runs/<name>/factories.json.
pub fn write_missing(config: &Config) -> Res<Outcome> {
    let repo = &config.new.repo;
    let mut outcome = Outcome {
        written: vec![],
        failed: vec![],
        broken: vec![],
        note: None,
    };
    let laravel = testgen::package_major(repo, "laravel/framework");
    if laravel.is_none_or(|v| v < 8) {
        outcome.note = Some(format!(
            "no factories written: the new app is Laravel {}, and factory classes came with Laravel 8",
            laravel.map_or("?".into(), |v| v.to_string())
        ));
        save(config, &outcome)?;
        return Ok(outcome);
    }
    let composer = std::fs::read_to_string(repo.join("composer.json")).unwrap_or_default();
    if !composer.contains("Database\\\\Factories\\\\") {
        outcome.note = Some(
            "no factories written: composer.json does not autoload Database\\Factories\\ (PSR-4), so a new factory class would not load"
                .into(),
        );
        save(config, &outcome)?;
        return Ok(outcome);
    }
    // a factory this tool wrote earlier is written again: the schema or the model may have changed
    let missing: Vec<NoFactory> = testgen::without_factory(repo)
        .into_iter()
        .chain(ours(repo))
        .collect();
    let mut seen = HashSet::new();
    let missing: Vec<NoFactory> = missing
        .into_iter()
        .filter(|m| seen.insert(m.class.clone()))
        .collect();
    let mut conn = db::connect(config, &config.new)?;
    let tables = schema::snapshot(&mut conn, &config.new.database)?;
    drop(conn);
    let factory_models: HashSet<String> = testgen::models_with_factory(repo)
        .into_values()
        .chain(missing.iter().map(|m| m.class.clone()))
        .collect();
    let by_table: HashMap<String, String> = testgen::models_with_factory(repo)
        .into_iter()
        .chain(missing.iter().map(|m| (m.table.clone(), m.class.clone())))
        .collect();

    let mut pending: BTreeMap<String, (PathBuf, BTreeMap<String, String>)> = BTreeMap::new();
    for model in &missing {
        let Some(table) = tables.get(&model.table) else {
            outcome.failed.push((
                model.class.clone(),
                format!("no table {} in the test database (the name the model gives, or its guess from the class name)", model.table),
            ));
            continue;
        };
        let columns = definition(model, table, &relations(model), &factory_models, &by_table);
        let (file, _) = render(&model.class, &columns);
        pending.insert(model.class.clone(), (file, columns));
    }
    let table_of: HashMap<&str, &str> = missing
        .iter()
        .map(|m| (m.class.as_str(), m.table.as_str()))
        .collect();
    if !missing.is_empty() {
        let no_table = missing.len() - pending.len();
        say(&format!(
            "{} models use HasFactory but have no factory{}; writing one for each of the {} others and checking it makes a row ...",
            missing.len(),
            if no_table > 0 {
                format!(" ({no_table} have no table in the test database)")
            } else {
                String::new()
            },
            pending.len()
        ));
    }
    for round in 1..=ROUNDS {
        if pending.is_empty() {
            break;
        }
        for (model, (file, columns)) in &pending {
            let (_, text) = render(model, columns);
            let path = repo.join(file);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        let names: Vec<String> = pending.keys().cloned().collect();
        let failed = check(config, &names);
        let mut next = BTreeMap::new();
        for (model, (file, mut columns)) in std::mem::take(&mut pending) {
            match failed.get(&model) {
                None => outcome.written.push((model, file)),
                Some(error) => {
                    let table = &tables[table_of[model.as_str()]];
                    if round < ROUNDS && fix(&mut columns, table, error) {
                        next.insert(model, (file, columns));
                    } else {
                        let _ = std::fs::remove_file(repo.join(&file));
                        outcome.failed.push((model, error.clone()));
                    }
                }
            }
        }
        pending = next;
    }

    // the app's own factories: only checked, so a failing generated test can be traced to its factory
    let written: HashSet<&str> = outcome.written.iter().map(|(m, _)| m.as_str()).collect();
    let own: Vec<String> = testgen::models_with_factory(repo)
        .into_values()
        .filter(|m| !written.contains(m.as_str()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if !own.is_empty() {
        say(&format!(
            "Checking the app's own {} factories make a row ...",
            own.len()
        ));
        let mut broken: Vec<(String, String)> = check(config, &own).into_iter().collect();
        broken.sort();
        outcome.broken = broken;
    }
    outcome.written.sort();
    outcome.failed.sort();
    save(config, &outcome)?;
    Ok(outcome)
}

/// The models whose factory this tool wrote (its marker): written again each run.
fn ours(repo: &Path) -> Vec<NoFactory> {
    let mut files = vec![];
    collect(&repo.join("database/factories"), &mut files);
    let all: HashMap<String, NoFactory> = testgen::all_models(repo)
        .into_iter()
        .map(|m| (m.class.clone(), m))
        .collect();
    let mut out = vec![];
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        if !text.contains(MARKER) {
            continue;
        }
        if let Some(model) = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("protected $model = \\"))
            .and_then(|l| l.strip_suffix("::class;"))
            && let Some(m) = all.get(model)
        {
            out.push(NoFactory {
                class: m.class.clone(),
                table: m.table.clone(),
                file: m.file.clone(),
                uses: m.uses.clone(),
            });
        }
    }
    out
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|e| e == "php") {
            files.push(path);
        }
    }
}

fn save(config: &Config, outcome: &Outcome) -> Res<()> {
    let pairs = |list: &[(String, String)]| -> Vec<Value> {
        list.iter()
            .map(|(m, e)| json!({"model": m, "error": e}))
            .collect()
    };
    write(
        &config.work.join("factories.json"),
        &json!({
            "written": outcome.written.iter().map(|(m, f)| json!({"model": m, "file": f.to_string_lossy().replace('\\', "/")})).collect::<Vec<_>>(),
            "failed": pairs(&outcome.failed),
            "broken": pairs(&outcome.broken),
            "note": outcome.note,
        }),
    )
}

/// One line for the tests step and the report.
pub fn line(found: &Value) -> Option<String> {
    if let Some(note) = found["note"].as_str() {
        return Some(format!("Factories: {note}."));
    }
    let count = |k: &str| found[k].as_array().map_or(0, Vec::len);
    let (written, failed, broken) = (count("written"), count("failed"), count("broken"));
    if written + failed + broken == 0 {
        return None;
    }
    let no_table = found["failed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| {
            f["error"]
                .as_str()
                .is_some_and(|e| e.starts_with("no table "))
        })
        .count();
    let mut parts = vec![format!("{written} factories written and checked")];
    if failed > no_table {
        parts.push(format!(
            "{} could not make a row (not kept)",
            failed - no_table
        ));
    }
    if no_table > 0 {
        parts.push(format!(
            "{no_table} models have no table in the test database (none written)"
        ));
    }
    if broken > 0 {
        parts.push(format!(
            "{broken} of the app's own factories cannot make a row"
        ));
    }
    Some(format!("Factories: {}.", parts.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(kind: &str, nullable: bool, default: Option<&str>, extra: &str) -> Column {
        Column {
            kind: kind.into(),
            nullable,
            default: default.map(String::from),
            extra: extra.into(),
            collation: None,
        }
    }

    #[test]
    fn values_by_type() {
        assert_eq!(
            value("active", &column("tinyint(1)", false, None, ""), false),
            "false"
        );
        assert_eq!(
            value("qty", &column("int unsigned", false, None, ""), false),
            "1"
        );
        assert_eq!(
            value("code", &column("int", false, None, ""), true),
            "$this->faker->unique()->numberBetween(1, 30000)"
        );
        assert_eq!(
            value("price", &column("decimal(10,2)", false, None, ""), false),
            "0"
        );
        assert_eq!(
            value("name", &column("varchar(20)", false, None, ""), false),
            "substr($this->faker->word(), 0, 20)"
        );
        assert_eq!(
            value("ref", &column("varchar(100)", false, None, ""), true),
            "\\Illuminate\\Support\\Str::random(16)"
        );
        assert_eq!(
            value("email", &column("varchar(255)", false, None, ""), true),
            "$this->faker->unique()->safeEmail()"
        );
        assert_eq!(
            value("token", &column("char(36)", false, None, ""), false),
            "(string) \\Illuminate\\Support\\Str::uuid()"
        );
        assert_eq!(
            value("kind", &column("enum('it''s','b')", false, None, ""), false),
            "'it\\'s'"
        );
        assert_eq!(
            value("on", &column("date", false, None, ""), false),
            "now()->toDateString()"
        );
        assert_eq!(
            value("at", &column("timestamp", false, None, ""), false),
            "now()"
        );
        assert_eq!(
            value("notes", &column("text", false, None, ""), false),
            "$this->faker->sentence()"
        );
    }

    #[test]
    fn definitions_and_fixes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Unit.php");
        std::fs::write(
            &file,
            "<?php\nnamespace App\\Models;\nuse App\\Models\\Property\\Block;\nclass Unit extends Model {\n    public function block() { return $this->belongsTo(Block::class, 'unit_block_id'); }\n    public function owner()\n    {\n        return $this->belongsTo(Owner::class);\n    }\n}\n",
        )
        .unwrap();
        let model = NoFactory {
            class: "App\\Models\\Unit".into(),
            table: "units".into(),
            file,
            uses: HashMap::from([("Block".into(), "App\\Models\\Property\\Block".into())]),
        };
        let links = relations(&model);
        assert_eq!(links["unit_block_id"], "App\\Models\\Property\\Block");
        assert_eq!(links["owner_id"], "App\\Models\\Owner");

        let mut table = Table::default();
        for (name, col) in [
            (
                "id",
                column("bigint unsigned", false, None, "auto_increment"),
            ),
            ("unit_block_id", column("int", false, None, "")),
            ("owner_id", column("int", false, None, "")),
            ("project_id", column("int", false, None, "")),
            ("unit_no", column("varchar(10)", false, None, "")),
            ("status", column("int", false, Some("1"), "")),
            ("note", column("text", true, None, "")),
            ("created_at", column("timestamp", false, None, "")),
        ] {
            table.columns.insert(name.into(), col);
        }
        table.indexes.insert("unit_no".into(), true);
        table.foreign_keys.insert(
            "project_id -> projects(project_id) on delete CASCADE on update NO ACTION".into(),
        );
        let factory_models = HashSet::from(["App\\Models\\Property\\Block".to_string()]);
        let by_table =
            HashMap::from([("projects".to_string(), "App\\Models\\Project".to_string())]);
        let mut columns = definition(&model, &table, &links, &factory_models, &by_table);
        assert_eq!(
            columns.keys().collect::<Vec<_>>(),
            ["owner_id", "project_id", "unit_block_id", "unit_no"]
        );
        assert_eq!(
            columns["unit_block_id"],
            "function () { return optional(\\App\\Models\\Property\\Block::query()->first())->getKey() ?? \\App\\Models\\Property\\Block::factory()->create()->getKey(); }"
        );
        // no factory for the related model: its first row, else 1
        assert_eq!(
            columns["project_id"],
            "function () { return optional(\\App\\Models\\Project::query()->first())->getKey() ?? 1; }"
        );
        assert_eq!(columns["unit_no"], "\\Illuminate\\Support\\Str::random(10)");

        let (path, php) = render(&model.class, &columns);
        assert_eq!(path, PathBuf::from("database/factories/UnitFactory.php"));
        assert!(php.contains("namespace Database\\Factories;"));
        assert!(php.contains("    protected $model = \\App\\Models\\Unit::class;"));
        assert!(php.contains("            'unit_no' => \\Illuminate\\Support\\Str::random(10),"));
        assert!(php.contains(MARKER));
        assert_eq!(
            factory_place("App\\Models\\Property\\Block").2,
            PathBuf::from("database/factories/Property")
        );

        table
            .columns
            .insert("remark".into(), column("varchar(5)", true, None, ""));
        assert!(fix(
            &mut columns,
            &table,
            "SQLSTATE[HY000]: General error: 1364 Field 'remark' doesn't have a default value"
        ));
        assert_eq!(columns["remark"], "substr($this->faker->word(), 0, 5)");
        assert!(fix(
            &mut columns,
            &table,
            "SQLSTATE[22001]: 1406 Data too long for column 'remark' at row 1"
        ));
        assert_eq!(columns["remark"], "'a'");
        // the same fix twice: it did not help, give up
        assert!(!fix(
            &mut columns,
            &table,
            "SQLSTATE[22001]: 1406 Data too long for column 'remark' at row 1"
        ));
        assert!(fix(
            &mut columns,
            &table,
            "1452 Cannot add or update a child row: a foreign key constraint fails (`db`.`units`, CONSTRAINT `x` FOREIGN KEY (`owner_id`) REFERENCES `owners` (`owner_id`))"
        ));
        assert!(columns["owner_id"].contains("DB::table('owners')->value('owner_id')"));
        assert!(fix(
            &mut columns,
            &table,
            "1062 Duplicate entry '1' for key 'units.units_owner_id_unique'"
        ));
        assert!(!fix(&mut columns, &table, "Class \"X\" not found"));
        let found = json!({"written": [{}, {}], "broken": [{}],
            "failed": [{"error": "no table x in the test database"}, {"error": "1364 Field 'y'"}]});
        assert_eq!(
            line(&found).unwrap(),
            "Factories: 2 factories written and checked; 1 could not make a row (not kept); 1 models have no table in the test database (none written); 1 of the app's own factories cannot make a row."
        );
        assert_eq!(
            short(
                "SQLSTATE[HY000]: 1364 Field 'x' doesn't have a default value (Connection: mysql, SQL: insert ...)\n#0 trace"
            ),
            "SQLSTATE[HY000]: 1364 Field 'x' doesn't have a default value"
        );
    }
}
