//! PHPUnit feature tests for shared routes that have none, built from what the old app answered.
//!
//! Each test makes its own rows with the new repo's factories (the test database starts empty), signs in, calls the
//! route and checks the old app's status code plus the shape of its JSON (keys, not values), or the validation fields
//! for a 422. The files carry a @generated marker; a file without it is never overwritten.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::config::Config;
use crate::coverage::{self, Driver};
use crate::db::TableInfo;
use crate::glob;
use crate::layout::{self, Kind, Place};
use crate::plan::{PARAM, Spec, plurals, snake};
use crate::runner;

/// Characters a PHP variable name cannot have.
static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\W").unwrap());

pub const MARKER: &str = "@generated laravel-parity";
/// The PHPUnit group every generated class is in, so the app's own tests can run without them (`--exclude-group`).
pub const GROUP: &str = "laravel-parity";
/// The group of tests an AI assistant writes from the coverage gaps: not the app's own (measured with the generated
/// ones), never overwritten by the tool.
pub const AI_GROUP: &str = "laravel-parity-ai";
/// Generated test files per PHPUnit run when the run of the whole group dies.
const BATCH: usize = 100;
const TESTABLE: [i64; 10] = [200, 201, 202, 204, 400, 401, 403, 404, 409, 422];

struct ClassInfo {
    short: String,
    parent: Option<String>,
    table: Option<String>,
    has_factory: bool,
    uses: HashMap<String, String>,
}

/// Table -> model class with a factory, read from the new repo's source.
pub struct Models {
    by_table: HashMap<String, String>,
}

fn php_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            php_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "php") {
            out.push(path);
        }
    }
}

impl Models {
    pub fn read(repo: &Path) -> Self {
        static CLASS: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?m)^\s*(?:abstract\s+|final\s+)*class\s+(\w+)(?:\s+extends\s+([\w\\]+))?")
                .unwrap()
        });
        static NAMESPACE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?m)^\s*namespace\s+([\w\\]+)\s*;").unwrap());
        static TABLE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"\$table\s*=\s*['"]([\w.]+)['"]"#).unwrap());
        static USE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?m)^\s*use\s+([\w\\]+?\\(\w+))\s*;").unwrap());

        let mut files = vec![];
        php_files(&repo.join("app"), &mut files);
        let mut classes: HashMap<String, ClassInfo> = HashMap::new();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Some(cap) = CLASS.captures(&text) else {
                continue;
            };
            let namespace = NAMESPACE
                .captures(&text)
                .map(|c| format!("{}\\", &c[1]))
                .unwrap_or_default();
            classes.insert(
                format!("{namespace}{}", &cap[1]),
                ClassInfo {
                    short: cap[1].to_string(),
                    parent: cap.get(2).map(|m| m.as_str().to_string()),
                    table: TABLE
                        .captures(&text)
                        .map(|c| c[1].rsplit('.').next().unwrap().to_string()),
                    has_factory: text.contains("HasFactory")
                        || text.contains("function newFactory"),
                    uses: USE
                        .captures_iter(&text)
                        .map(|c| (c[2].to_string(), c[1].to_string()))
                        .collect(),
                },
            );
        }
        let mut factory_files = vec![];
        php_files(&repo.join("database").join("factories"), &mut factory_files);
        let factories: HashSet<String> = factory_files
            .iter()
            .filter_map(|f| {
                f.file_stem()?
                    .to_str()?
                    .strip_suffix("Factory")
                    .map(String::from)
            })
            .collect();

        let mut by_table: HashMap<String, String> = HashMap::new();
        let mut names: Vec<&String> = classes.keys().collect();
        names.sort();
        for full in names {
            let info = &classes[full];
            if !factories.contains(&info.short) || !factory_trait(&classes, full, 0) {
                continue;
            }
            let table = info
                .table
                .clone()
                .unwrap_or_else(|| plurals(&snake(&info.short))[0].clone());
            // prefer App\Models\X over an older App\X for the same table
            if !by_table.contains_key(&table) || full.starts_with("App\\Models\\") {
                by_table.insert(table, full.clone());
            }
        }
        Models { by_table }
    }
}

fn factory_trait(classes: &HashMap<String, ClassInfo>, full: &str, depth: usize) -> bool {
    let Some(info) = classes.get(full) else {
        return false;
    };
    if info.has_factory {
        return true;
    }
    if depth > 6 {
        return false;
    }
    let Some(parent) = &info.parent else {
        return false;
    };
    let parent = if parent.contains('\\') {
        parent.trim_start_matches('\\').to_string()
    } else {
        info.uses
            .get(parent)
            .cloned()
            .unwrap_or_else(|| format!("{}\\{parent}", full.rsplit_once('\\').map_or("", |p| p.0)))
    };
    factory_trait(classes, &parent, depth + 1)
}

enum Shape {
    Key(String),
    Nested(String, Vec<Shape>),
}

/// assertJsonStructure argument: keys only, one level into objects and lists of objects.
fn shape(value: &Value, depth: usize) -> Vec<Shape> {
    const LIMIT: usize = 2;
    let Value::Object(map) = value else {
        return vec![];
    };
    map.iter()
        .map(|(key, child)| match child {
            Value::Object(m) if depth < LIMIT && !m.is_empty() => {
                Shape::Nested(key.clone(), shape(child, depth + 1))
            }
            Value::Array(l) if depth < LIMIT && !l.is_empty() && l.iter().all(Value::is_object) => {
                Shape::Nested(
                    key.clone(),
                    vec![Shape::Nested("*".into(), shape(&l[0], depth + 2))],
                )
            }
            _ => Shape::Key(key.clone()),
        })
        .collect()
}

fn php_shape(shape: &[Shape]) -> String {
    let parts: Vec<String> = shape
        .iter()
        .map(|s| match s {
            Shape::Key(k) => php_string(k),
            Shape::Nested(k, child) => format!("{} => {}", php_string(k), php_shape(child)),
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

pub fn php_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub fn php_value(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => php_string(s),
        Value::Array(l) => format!(
            "[{}]",
            l.iter().map(php_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(m) => {
            format!(
                "[{}]",
                m.iter()
                    .map(|(k, v)| format!("{} => {}", php_string(k), php_value(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn method_name(key: &str, taken: &mut HashSet<String>) -> String {
    static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
    let base: String = format!(
        "test_{}",
        NON_WORD
            .replace_all(&key.to_lowercase(), "_")
            .trim_matches('_')
    )
    .chars()
    .take(120)
    .collect();
    let mut candidate = base.clone();
    let mut n = 2;
    while taken.contains(&candidate) {
        candidate = format!("{base}_{n}");
        n += 1;
    }
    taken.insert(candidate.clone());
    candidate
}

/// One provider row: a way of sending the route and what the old app answered.
struct Row {
    case: String,
    sign_in: bool,
    missing: bool,
    body: Value,
    without: Vec<String>,
    status: i64,
    structure: String,
    errors: Vec<String>,
}

/// A test method for one route with a data provider of its cases (a function and the provider), or why not.
fn build_route(
    config: &Config,
    cases: &[&Spec],
    old_rows: &HashMap<String, Value>,
    models: &Models,
    tables: &BTreeMap<String, TableInfo>,
    attribute: bool,
    name: &str,
) -> Result<(String, Vec<String>), String> {
    let tests = &config.tests;
    let happy = cases
        .iter()
        .find(|s| s.case == "happy")
        .copied()
        .unwrap_or(cases[0]);

    // the rows each case expects, from the old app's answers
    let mut rows = vec![];
    let mut left_out = vec![];
    for spec in cases {
        let Some(old) = old_rows.get(&spec.id) else {
            continue;
        };
        let Some(status) = old["status"].as_i64().filter(|s| TESTABLE.contains(s)) else {
            left_out.push(format!("{}: old app answered {}", spec.case, old["status"]));
            continue;
        };
        let payload = &old["json"];
        let (structure, errors) =
            if status == 422 && payload["errors"].as_object().is_some_and(|e| !e.is_empty()) {
                (
                    String::from("[]"),
                    payload["errors"]
                        .as_object()
                        .unwrap()
                        .keys()
                        .cloned()
                        .collect(),
                )
            } else if status < 300
                && old["kind"] == "json"
                && payload.as_object().is_some_and(|m| !m.is_empty())
            {
                (php_shape(&shape(payload, 0)), vec![])
            } else {
                (String::from("[]"), vec![])
            };
        let body = if spec.method == "GET" {
            Value::Object(Default::default())
        } else {
            spec.body
                .clone()
                .unwrap_or(Value::Object(Default::default()))
        };
        rows.push(Row {
            case: spec.case.clone(),
            sign_in: spec.guard.is_some(),
            missing: spec.case == "missing",
            body,
            without: spec.without.clone(),
            status,
            structure,
            errors,
        });
    }
    if rows.is_empty() {
        return Err(left_out
            .first()
            .cloned()
            .unwrap_or_else(|| "no answer from the old app".into()));
    }

    // the rows the route needs, made with factories; a missing case swaps their keys for an id that does not exist
    let mut lines: Vec<String> = vec![];
    let mut uri = happy.template.clone();
    for cap in PARAM.captures_iter(&happy.template) {
        let param = &cap[1];
        let source = happy.sources.get(param).cloned().unwrap_or_default();
        let table = source.split_once('.').map(|(t, _)| t.to_string());
        let model = table.as_ref().and_then(|t| models.by_table.get(t));
        let Some(model) = model else {
            if cap.get(2).is_some() {
                uri = uri.replace(&cap[0], "");
                continue;
            }
            let why = if source.is_empty() {
                "value typed by hand".to_string()
            } else {
                source
            };
            return Err(format!("no factory for {{{param}}} ({why})"));
        };
        let var = format!("${}", NON_WORD.replace_all(param, "_"));
        let info = tables
            .get(table.as_deref().unwrap())
            .cloned()
            .unwrap_or_default();
        // scope keys are column patterns: "*_project_id" matches unit_project_id
        let overrides: Vec<String> = info
            .columns
            .iter()
            .filter(|c| info.pk.as_ref() != Some(c))
            .filter_map(|c| {
                tests
                    .scope
                    .iter()
                    .find(|(p, _)| glob::matches(c, p))
                    .map(|(_, e)| format!("{} => {e}", php_string(c)))
            })
            .collect();
        lines.push(format!(
            "{var} = \\{model}::factory()->create([{}]);",
            overrides.join(", ")
        ));
        lines.push(format!(
            "{var}Key = $missing ? {} : {var}->getKey();",
            php_value(&json_id(&config.missing_id))
        ));
        uri = uri.replace(&cap[0], &format!("{{{var}Key}}"));
    }
    let uri = Regex::new("/+")
        .unwrap()
        .replace_all(&uri, "/")
        .trim_end_matches('/')
        .to_string();

    lines.push("$user = null;".into());
    if let Some(guard) = &happy.guard {
        let acting = tests
            .acting_as
            .get(guard)
            .ok_or(format!("no [tests.acting_as] {guard} in the pair file"))?;
        lines.push(format!(
            "if ($signIn) {{\n    {};\n}}",
            acting.trim_end_matches(';')
        ));
    }
    if !tests.headers.is_empty() {
        let rendered: Vec<String> = tests
            .headers
            .iter()
            .map(|(k, v)| format!("{} => (string) {v}", php_string(k)))
            .collect();
        lines.push(format!("$this->withHeaders([{}]);", rendered.join(", ")));
    }
    // the inputs the web app always sends ([tests] query/body), on the test's own rows; $user is null when the case
    // is not signed in
    let defaults = if happy.method == "GET" {
        &tests.query
    } else {
        &tests.body
    };
    let data: Vec<String> = defaults
        .iter()
        .map(|(k, v)| format!("{} => {}", php_string(k), v.replace("$user->", "$user?->")))
        .collect();
    lines.push(format!(
        "$data = array_merge([{}], $body);",
        data.join(", ")
    ));
    if cases.iter().any(|s| s.custom) {
        // a case someone added may say "{unit}" for the row the test made, "{user}" for the signed-in user
        let mut fills = vec!["'{user}' => $user?->getKey()".to_string()];
        for cap in PARAM.captures_iter(&happy.template) {
            let var = format!("${}Key", NON_WORD.replace_all(&cap[1], "_"));
            if lines.iter().any(|l| l.starts_with(&var)) {
                fills.push(format!("'{{{}}}' => {var}", &cap[1]));
            }
        }
        lines.push(format!(
            "$fill = [{}];\narray_walk_recursive($data, function (&$value) use ($fill) {{\n    if (is_string($value) && array_key_exists($value, $fill)) {{\n        $value = $fill[$value];\n    }}\n}});",
            fills.join(", ")
        ));
    }
    lines.push("foreach ($without as $field) {\n    unset($data[$field]);\n}".into());
    lines.push(String::new());
    lines.push(format!(
        "$response = $this->json({}, \"{uri}\", $data);",
        php_string(&happy.method)
    ));
    lines.push(String::new());
    lines.push("$response->assertStatus($status);".into());
    lines.push(
        "if ($structure !== []) {\n    $response->assertJsonStructure($structure);\n}".into(),
    );
    lines.push(
        "if ($errors !== []) {\n    $response->assertJsonValidationErrors($errors);\n}".into(),
    );

    let provider = format!("cases_{}", name.trim_start_matches("test_"));
    let mut function = vec![];
    if attribute {
        function.push(format!(
            "#[\\PHPUnit\\Framework\\Attributes\\DataProvider('{provider}')]"
        ));
    } else {
        function.push(format!("/** @dataProvider {provider} */"));
    }
    function.push(format!(
        "public function {name}(bool $signIn, bool $missing, array $body, array $without, int $status, array $structure, array $errors): void"
    ));
    function.push("{".into());
    function.extend(lines.join("\n").lines().map(|l| {
        if l.is_empty() {
            String::new()
        } else {
            format!("    {l}")
        }
    }));
    function.push("}".into());
    function.push(String::new());
    function.push(format!("public static function {provider}(): array"));
    function.push("{".into());
    function.push("    // case => [sign in, missing record, body, fields left out, status, JSON keys, validation errors]".into());
    function.push("    return [".into());
    for row in &rows {
        let without: Vec<String> = row.without.iter().map(|f| php_string(f)).collect();
        let errors: Vec<String> = row.errors.iter().map(|f| php_string(f)).collect();
        function.push(format!(
            "        {} => [{}, {}, {}, [{}], {}, {}, [{}]],",
            php_string(&row.case),
            row.sign_in,
            row.missing,
            php_value(&row.body),
            without.join(", "),
            row.status,
            row.structure,
            errors.join(", ")
        ));
    }
    function.push("    ];".into());
    function.push("}".into());
    Ok((
        function.join("\n"),
        rows.iter().map(|r| r.case.clone()).collect(),
    ))
}

fn json_id(id: &str) -> Value {
    id.parse::<i64>()
        .map(Value::from)
        .unwrap_or_else(|_| Value::from(id))
}

/// The controller's full class name (App\\Http\\Controllers\\X), or Closure.
fn controller_fqcn(action: Option<&str>) -> String {
    match action.map(|a| a.split('@').next().unwrap_or(a).trim_start_matches('\\')) {
        Some(class) if !class.is_empty() && class.chars().any(char::is_alphanumeric) => {
            class.to_string()
        }
        _ => "Closure".into(),
    }
}

#[cfg(test)]
fn controller_class(action: Option<&str>) -> String {
    let Some(action) = action else {
        return "Closure".into();
    };
    let controller = action
        .split('@')
        .next()
        .unwrap_or(action)
        .rsplit('\\')
        .next()
        .unwrap_or(action);
    let clean: String = controller
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if clean.is_empty() {
        "Closure".into()
    } else {
        clean
    }
}

fn render_file(
    config: &Config,
    place: &Place,
    methods: &[(String, String, String)],
    attribute: bool,
) -> String {
    let tests = &config.tests;
    let namespace = &place.namespace;
    let base = tests
        .base_class
        .as_deref()
        .unwrap_or("Tests\\TestCase")
        .trim_start_matches('\\');
    let old_name = config
        .old
        .repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = vec![
        "<?php".to_string(),
        String::new(),
        format!("namespace {namespace};"),
        String::new(),
        "/**".into(),
        format!(" * {MARKER}: made from the old app's answers ({old_name}). One test per route; its data provider"),
        " * holds the ways the route was sent (happy, not signed in, missing record, empty or incomplete body) and the".into(),
        " * status, JSON keys or validation errors the old app gave. Regenerating replaces this file; once you edit it,".into(),
        " * remove this marker so the tool leaves it alone.".into(),
    ];
    if attribute {
        out.push(" */".into());
        out.push(format!(
            "#[\\PHPUnit\\Framework\\Attributes\\Group('{GROUP}')]"
        ));
    } else {
        out.push(" *".into());
        out.push(format!(" * @group {GROUP}"));
        out.push(" */".into());
    }
    out.push(format!("class {} extends \\{base}", place.class));
    out.push("{".into());
    for (i, (_, key, body)) in methods.iter().enumerate() {
        if i > 0 {
            out.push(String::new());
        }
        out.push(format!("    /** {key} */"));
        out.extend(body.lines().map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("    {l}")
            }
        }));
    }
    out.push("}".into());
    out.join("\n") + "\n"
}

/// PHPUnit 10+ reads data providers from attributes; 9 from @dataProvider.
pub fn phpunit_attributes(repo: &Path) -> bool {
    phpunit_major(repo).is_some_and(|m| m >= 10)
}

/// The PHPUnit major version in the repo's composer.lock.
pub fn phpunit_major(repo: &Path) -> Option<u32> {
    let lock: Value = std::fs::read_to_string(repo.join("composer.lock"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let version = ["packages-dev", "packages"]
        .iter()
        .flat_map(|k| lock[*k].as_array().cloned().unwrap_or_default())
        .find(|p| p["name"] == "phpunit/phpunit")
        .and_then(|p| p["version"].as_str().map(String::from))
        .unwrap_or_default();
    version
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
}

pub struct Made {
    pub class: String,
    /// "test_x#case": one provider row
    pub method: String,
    pub key: String,
}

pub struct Generated {
    pub files: Vec<PathBuf>,
    pub made: Vec<Made>,
    pub cannot: Vec<(String, String)>,
}

/// Write the test files: one test per untested route, one provider row per case. `skip` holds (class,
/// "method#case") rows to leave out (failing ones, with --drop-failing).
pub fn generate(
    config: &Config,
    specs: &[Spec],
    old_rows: &HashMap<String, Value>,
    untested: &BTreeSet<String>,
    tables: &BTreeMap<String, TableInfo>,
    skip: &HashSet<(String, String)>,
) -> Generated {
    let models = Models::read(&config.new.repo);
    let attribute = phpunit_attributes(&config.new.repo);
    let mut routes: BTreeMap<&str, Vec<&Spec>> = BTreeMap::new();
    for spec in specs.iter().filter(|s| untested.contains(&s.key)) {
        routes.entry(spec.key.as_str()).or_default().push(spec);
    }
    let mut by_controller: BTreeMap<String, Vec<(String, Vec<&Spec>)>> = BTreeMap::new();
    for (key, cases) in routes {
        by_controller
            .entry(controller_fqcn(cases[0].action.as_deref()))
            .or_default()
            .push((key.to_string(), cases));
    }

    let mut generated = Generated {
        files: vec![],
        made: vec![],
        cannot: vec![],
    };
    let mut used = HashSet::new();
    for (controller, entries) in by_controller {
        let mut place = layout::place(config, Kind::Feature, &controller);
        // two controllers with the same name in one folder (the folder layout): the second gets a number
        let mut n = 1;
        while !used.insert(place.file.clone()) {
            n += 1;
            let base = layout::place(config, Kind::Feature, &controller);
            place.class = format!("{}{n}", base.class);
            place.file = base.file.with_file_name(format!("{}.php", place.class));
        }
        let class = place.fqcn();
        let file = place.file.clone();
        let file_name = format!("{}.php", place.class);
        if file.exists()
            && !std::fs::read_to_string(&file)
                .unwrap_or_default()
                .contains(MARKER)
        {
            generated.cannot.extend(
                entries
                    .into_iter()
                    .map(|(k, _)| (k, format!("{file_name} was edited by hand (no marker)"))),
            );
            continue;
        }
        let mut taken = HashSet::new();
        let mut methods = vec![];
        for (key, cases) in entries {
            let name = method_name(&key, &mut taken);
            let kept: Vec<&Spec> = cases
                .into_iter()
                .filter(|s| !skip.contains(&(class.clone(), format!("{name}#{}", s.case))))
                .collect();
            if kept.is_empty() {
                continue;
            }
            match build_route(config, &kept, old_rows, &models, tables, attribute, &name) {
                Ok((code, rows)) => {
                    generated.made.extend(rows.into_iter().map(|case| Made {
                        class: class.clone(),
                        method: format!("{name}#{case}"),
                        key: key.clone(),
                    }));
                    methods.push((name, key, code));
                }
                Err(why) => generated.cannot.push((key, why)),
            }
        }
        if methods.is_empty() {
            let _ = std::fs::remove_file(&file);
            continue;
        }
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&file, render_file(config, &place, &methods, attribute));
        generated.files.push(file);
    }
    generated
}

/// (class, method) -> failure message, or None when it passed.
pub type Outcomes = HashMap<(String, String), Option<String>>;

/// Runs the generated tests: the whole folder in one PHPUnit run; only if that
/// dies without a report, one run per file, so a fatal error in one file cannot hide the rest. With a coverage
/// driver and the path of coverage.php in the app (`cover`), also the coverage of all the generated tests
/// (runs/<name>/coverage/generated.cov).
pub fn run_phpunit(
    config: &Config,
    files: &[PathBuf],
    cover: Option<(Driver, &str)>,
) -> (Outcomes, Option<PathBuf>) {
    let mut results = HashMap::new();
    if files.is_empty() {
        return (results, None);
    }
    let side = &config.new;
    runner::prepare_scratch(side);
    let total = runner::scratch(side, "generated.cov");
    runner::remove(side, &total);
    // one test that kills PHP (exit, a fatal error) loses the whole run: run batches, and split a batch that gets
    // no report in two until the file that does it runs alone
    let mut queue: Vec<&[PathBuf]> = files.chunks(BATCH).rev().collect();
    let mut parts = vec![];
    let mut done = 0;
    while let Some(batch) = queue.pop() {
        let part = runner::scratch(side, &format!("part-{}.cov", parts.len()));
        let cover_part = cover.map(|(driver, _)| (driver, part.as_str()));
        let classes: Vec<String> = batch.iter().filter_map(|f| file_class(f)).collect();
        let ran = if batch.len() == 1 || classes.len() < batch.len() {
            batch.iter().all(|file| {
                let rel = file
                    .strip_prefix(&side.repo)
                    .unwrap_or(file)
                    .to_string_lossy()
                    .replace('\\', "/");
                phpunit(config, &[&rel], &mut results, cover_part)
            })
        } else {
            let filter = format!(
                "(?:{})::",
                classes
                    .iter()
                    .map(|c| c.replace('\\', "\\\\"))
                    .collect::<Vec<_>>()
                    .join("|")
            );
            phpunit(
                config,
                &["--group", GROUP, "--filter", &filter],
                &mut results,
                cover_part,
            ) && classes
                .iter()
                .all(|c| results.keys().any(|(class, _)| class == c))
        };
        if ran || batch.len() == 1 {
            if cover.is_some() {
                parts.push(part);
            }
            done += batch.len();
            crate::say(&format!("  {done}/{} test files", files.len()));
        } else {
            runner::remove(side, &part);
            let (a, b) = batch.split_at(batch.len() / 2);
            queue.push(b);
            queue.push(a);
        }
    }
    if let Some((_, script)) = cover {
        coverage::merge(side, script, &total, &parts);
    }
    let file = cover.and_then(|_| coverage::fetch(config, &total, "generated.cov"));
    runner::remove(side, &total);
    (results, file)
}

/// The class a generated test file declares, with its namespace.
fn file_class(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let mut namespace = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("namespace ") {
            namespace = Some(rest.trim_end_matches(';').trim().to_string());
        } else if let Some(rest) = line
            .strip_prefix("final class ")
            .or_else(|| line.strip_prefix("class "))
        {
            let class = rest.split_whitespace().next()?;
            return Some(match namespace {
                Some(n) => format!("{n}\\{class}"),
                None => class.to_string(),
            });
        }
    }
    None
}

/// One PHPUnit run (`target`: a file, or --group ...), its results added to `results`; with `cover`, coverage goes
/// to that scratch path. False when PHPUnit wrote no report.
pub fn phpunit(
    config: &Config,
    target: &[&str],
    results: &mut HashMap<(String, String), Option<String>>,
    cover: Option<(Driver, &str)>,
) -> bool {
    let side = &config.new;
    let junit = runner::scratch(side, "junit.xml");
    let command = coverage::command(config, cover, phpunit_attributes(&side.repo));
    let mut args: Vec<&str> = command.iter().map(String::as_str).collect();
    args.extend(["--log-junit", &junit]);
    args.extend(target);
    let _ = runner::run_in_app(side, &args);
    let local = config.work.join("junit.xml");
    let _ = std::fs::remove_file(&local);
    if !runner::fetch(side, &junit, &local) {
        return false;
    }
    let text = std::fs::read_to_string(&local).unwrap_or_default();
    let _ = std::fs::remove_file(&local);
    let Ok(doc) = roxmltree::Document::parse(&text) else {
        return false;
    };
    let mut any = false;
    for case in doc.descendants().filter(|n| n.has_tag_name("testcase")) {
        any = true;
        // the full class name: PHPUnit 10+ writes it in class, older ones in classname with dots
        let class = case
            .attribute("class")
            .map(String::from)
            .or_else(|| case.attribute("classname").map(|c| c.replace('.', "\\")))
            .unwrap_or_default()
            .trim_start_matches('\\')
            .to_string();
        let problem = case
            .children()
            .find(|n| n.has_tag_name("failure") || n.has_tag_name("error"));
        let message = problem.map(|p| {
            let text = p.text().or(p.attribute("message")).unwrap_or_default();
            let mut lines: Vec<&str> = text.trim().lines().collect();
            // JUnit starts the text with the test's own name
            if lines.first().is_some_and(|l| l.contains("::")) {
                lines.remove(0);
            }
            lines
                .iter()
                .take(3)
                .map(|l| l.trim())
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(300)
                .collect()
        });
        let name = case.attribute("name").unwrap_or_default();
        static DATA_SET: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"^(\w+)(?: with data set "(.+)"| with data set #?(\S+)|#(.+))"#).unwrap()
        });
        let name = match DATA_SET.captures(name) {
            Some(c) => format!(
                "{}#{}",
                &c[1],
                c.get(2)
                    .or(c.get(3))
                    .or(c.get(4))
                    .map(|m| m.as_str())
                    .unwrap_or("")
            ),
            None => name.to_string(),
        };
        results.insert((class, name), message);
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn class_of_a_test_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("UnitParityTest.php");
        std::fs::write(
            &file,
            "<?php\n\nnamespace Tests\\Unit\\Models;\n\nuse Tests\\TestCase;\n\nclass UnitParityTest extends TestCase\n{\n}\n",
        )
        .unwrap();
        assert_eq!(
            file_class(&file).as_deref(),
            Some("Tests\\Unit\\Models\\UnitParityTest")
        );
        std::fs::write(&file, "<?php\nfinal class Bare extends X {}\n").unwrap();
        assert_eq!(file_class(&file).as_deref(), Some("Bare"));
        assert_eq!(file_class(&dir.path().join("missing.php")), None);
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A pair file for two fake repos in a temp folder, with `extra` appended (a [tests] section).
    fn config(dir: &Path, extra: &str) -> Config {
        let (old, new) = (dir.join("shop-old"), dir.join("shop"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let pair = dir.join("pairs").join("shop.toml");
        put(
            &pair,
            &format!(
                "name = \"shop\"\nwork = {:?}\n[old]\nrepo = {:?}\n[new]\nrepo = {:?}\n[database]\nsource = \"shop\"\n{extra}",
                dir.join("runs").display().to_string(),
                old.display().to_string(),
                new.display().to_string()
            ),
        );
        crate::config::load(&pair).unwrap()
    }

    /// Shop (own HasFactory), Order (factory through its parent, own $table), Item (no factory file).
    fn models_repo(repo: &Path) {
        put(
            &repo.join("app/Models/Shop.php"),
            "<?php\nnamespace App\\Models;\nuse Illuminate\\Database\\Eloquent\\Factories\\HasFactory;\nclass Shop extends Model\n{\n    use HasFactory;\n}\n",
        );
        put(
            &repo.join("app/Models/Base.php"),
            "<?php\nnamespace App\\Models;\nabstract class Base extends Model\n{\n    use HasFactory;\n}\n",
        );
        put(
            &repo.join("app/Models/Order.php"),
            "<?php\nnamespace App\\Models;\nclass Order extends Base\n{\n    protected $table = 'shop_orders';\n}\n",
        );
        put(
            &repo.join("app/Models/Item.php"),
            "<?php\nnamespace App\\Models;\nclass Item extends Model\n{\n    use HasFactory;\n}\n",
        );
        for f in ["ShopFactory", "OrderFactory"] {
            put(&repo.join(format!("database/factories/{f}.php")), "<?php\n");
        }
    }

    #[test]
    fn php_text() {
        assert_eq!(php_string(r"it's C:\shop $5"), r"'it\'s C:\\shop $5'");
        assert_eq!(
            php_value(&json!({"a": null, "b": [1, true, "x'y"], "c": {}})),
            r"['a' => null, 'b' => [1, true, 'x\'y'], 'c' => []]"
        );
        let mut taken = HashSet::new();
        assert_eq!(
            method_name("GET api/shops/{shop}", &mut taken),
            "test_get_api_shops_shop"
        );
        assert_eq!(
            method_name("GET api/shops/{shop?}", &mut taken),
            "test_get_api_shops_shop_2"
        );
        assert_eq!(method_name(&"a".repeat(200), &mut taken).len(), 120);
        assert_eq!(
            controller_class(Some("App\\Http\\Controllers\\ShopController@show")),
            "ShopController"
        );
        assert_eq!(controller_class(Some("App\\Http\\Invokable")), "Invokable");
        assert_eq!(controller_class(Some("@@")), "Closure");
        assert_eq!(controller_class(None), "Closure");
        assert_eq!(json_id("999"), json!(999));
        assert_eq!(json_id("nope"), json!("nope"));
    }

    #[test]
    fn json_structure() {
        let body = json!({"data": [{"id": 1, "shop": {"name": "a", "deep": {"x": 1}}}], "meta": {"total": 1}, "ok": true, "tags": [1, 2], "empty": {}});
        assert_eq!(
            php_shape(&shape(&body, 0)),
            "['data' => ['*' => ['id', 'shop']], 'meta' => ['total'], 'ok', 'tags', 'empty']"
        );
        assert_eq!(
            php_shape(&shape(&json!({"a": {"b": {"c": {"d": 1}}}}), 0)),
            "['a' => ['b' => ['c']]]"
        );
        assert!(shape(&json!([1]), 0).is_empty());
    }

    #[test]
    fn models_and_phpunit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        models_repo(repo);
        let models = Models::read(repo);
        assert_eq!(models.by_table["shops"], "App\\Models\\Shop");
        assert_eq!(models.by_table["shop_orders"], "App\\Models\\Order");
        assert!(!models.by_table.contains_key("items"));
        assert!(!models.by_table.contains_key("bases"));

        assert!(!phpunit_attributes(repo));
        let lock = |v: &str| {
            json!({"packages": [], "packages-dev": [{"name": "phpunit/phpunit", "version": v}]})
                .to_string()
        };
        put(&repo.join("composer.lock"), &lock("9.6.19"));
        assert!(!phpunit_attributes(repo));
        put(&repo.join("composer.lock"), &lock("v10.5.1"));
        assert!(phpunit_attributes(repo));
        put(&repo.join("composer.lock"), &lock("11.0.0"));
        assert!(phpunit_attributes(repo));
    }

    fn spec(id: &str, key: &str, case: &str, action: Option<&str>) -> Spec {
        let (method, template) = key.split_once(' ').unwrap();
        Spec {
            id: id.into(),
            key: key.into(),
            case: case.into(),
            method: method.into(),
            template: template.into(),
            uri: template.into(),
            guard: (case != "guest").then(|| "api".to_string()),
            action: action.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn generates_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(
            dir.path(),
            "[tests]\nlayout = 'folder'\nacting_as = { api = '$user = \\App\\Models\\User::factory()->create(); $this->actingAs($user, \"api\");' }\nheaders = { X-Shop = '$user?->shop_id' }\nscope = { '*_shop_id' = '$shop->id' }\nquery = { page = '1' }\nbody = { locale = \"'en'\", owner = '$user->id' }\n",
        );
        models_repo(&config.new.repo);
        let show = "App\\Http\\Controllers\\ShopController@show";
        let mut specs = vec![];
        for (id, case) in [
            ("s1", "happy"),
            ("s2", "guest"),
            ("s3", "missing"),
            ("s4", "broken"),
        ] {
            let mut s = spec(id, "GET api/shops/{shop}", case, Some(show));
            s.sources.insert("shop".into(), "shops.id".into());
            s.body = Some(json!({"ignored": "for GET"}));
            specs.push(s);
        }
        let mut store = spec("o1", "PUT api/orders/{order}/{note?}", "happy", Some(show));
        store
            .sources
            .insert("order".into(), "shop_orders.id".into());
        store.body = Some(json!({"name": "it's \\ $5", "shop": "{order}"}));
        store.custom = true;
        let mut invalid = spec(
            "o2",
            "PUT api/orders/{order}/{note?}",
            "without-name",
            Some(show),
        );
        invalid.sources = store.sources.clone();
        invalid.without = vec!["name".into()];
        specs.extend([store, invalid]);
        specs.push(spec("t1", "GET api/things/{thing}", "happy", Some(show)));
        specs.push(spec("c1", "GET api/ping", "happy", None));
        specs.push(spec("x1", "GET api/tested", "happy", Some("App\\X@y")));
        specs.push(spec("h1", "GET api/hand", "happy", Some("App\\Hand@y")));
        put(
            &config
                .new
                .repo
                .join("tests/Feature/Parity/HandParityTest.php"),
            "<?php // mine\n",
        );

        let old_rows: HashMap<String, Value> = [
            (
                "s1",
                json!({"status": 200, "kind": "json", "json": {"data": {"id": 1, "name": "a"}}}),
            ),
            (
                "s2",
                json!({"status": 401, "kind": "json", "json": {"message": "no"}}),
            ),
            ("s3", json!({"status": 404, "kind": "html", "json": null})),
            ("s4", json!({"status": 500, "kind": "json", "json": {}})),
            ("o1", json!({"status": 204, "kind": "empty", "json": null})),
            (
                "o2",
                json!({"status": 422, "kind": "json", "json": {"errors": {"name": ["required"]}}}),
            ),
            ("t1", json!({"status": 200, "kind": "json", "json": {}})),
            ("c1", json!({"status": 500, "kind": "json", "json": {}})),
            ("h1", json!({"status": 200, "kind": "json", "json": {}})),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let untested: BTreeSet<String> = specs
            .iter()
            .map(|s| s.key.clone())
            .filter(|k| k != "GET api/tested")
            .collect();
        let tables = BTreeMap::from([(
            "shop_orders".to_string(),
            TableInfo {
                columns: vec!["id".into(), "owner_shop_id".into(), "name".into()],
                pk: Some("id".into()),
            },
        )]);
        let skip = HashSet::from([(
            "Tests\\Feature\\Parity\\ShopControllerParityTest".to_string(),
            "test_get_api_shops_shop#missing".to_string(),
        )]);

        let out = generate(&config, &specs, &old_rows, &untested, &tables, &skip);
        let dir_tests = config.new.repo.join("tests/Feature/Parity");
        assert_eq!(
            out.files,
            vec![dir_tests.join("ShopControllerParityTest.php")]
        );
        assert!(!dir_tests.join("ClosureParityTest.php").exists());
        assert_eq!(
            std::fs::read_to_string(dir_tests.join("HandParityTest.php")).unwrap(),
            "<?php // mine\n"
        );
        let made: Vec<&str> = out.made.iter().map(|m| m.method.as_str()).collect();
        assert_eq!(
            made,
            vec![
                "test_get_api_shops_shop#happy",
                "test_get_api_shops_shop#guest",
                "test_put_api_orders_order_note#happy",
                "test_put_api_orders_order_note#without-name",
            ]
        );
        let cannot: BTreeMap<&str, &str> = out
            .cannot
            .iter()
            .map(|(k, w)| (k.as_str(), w.as_str()))
            .collect();
        assert_eq!(cannot["GET api/ping"], "happy: old app answered 500");
        assert_eq!(
            cannot["GET api/things/{thing}"],
            "no factory for {thing} (value typed by hand)"
        );
        assert_eq!(
            cannot["GET api/hand"],
            "HandParityTest.php was edited by hand (no marker)"
        );
        assert!(!cannot.contains_key("GET api/tested"));

        let php = std::fs::read_to_string(&out.files[0]).unwrap();
        for line in [
            "namespace Tests\\Feature\\Parity;",
            MARKER,
            " * @group laravel-parity",
            "class ShopControllerParityTest extends \\Tests\\TestCase",
            "    /** GET api/shops/{shop} */",
            "    /** @dataProvider cases_get_api_shops_shop */",
            "    public function test_get_api_shops_shop(bool $signIn, bool $missing, array $body, array $without, int $status, array $structure, array $errors): void",
            "        $shop = \\App\\Models\\Shop::factory()->create([]);",
            "        $shopKey = $missing ? 999999999 : $shop->getKey();",
            "            $user = \\App\\Models\\User::factory()->create(); $this->actingAs($user, \"api\");",
            "        $this->withHeaders(['X-Shop' => (string) $user?->shop_id]);",
            "        $data = array_merge(['page' => 1], $body);",
            "        $response = $this->json('GET', \"api/shops/{$shopKey}\", $data);",
            "            'happy' => [true, false, [], [], 200, ['data' => ['id', 'name']], []],",
            "            'guest' => [false, false, [], [], 401, [], []],",
            "        $order = \\App\\Models\\Order::factory()->create(['owner_shop_id' => $shop->id]);",
            "        $data = array_merge(['locale' => 'en', 'owner' => $user?->id], $body);",
            "        $fill = ['{user}' => $user?->getKey(), '{order}' => $orderKey];",
            "        $response = $this->json('PUT', \"api/orders/{$orderKey}\", $data);",
            r"            'happy' => [true, false, ['name' => 'it\'s \\ $5', 'shop' => '{order}'], [], 204, [], []],",
            "            'without-name' => [true, false, [], ['name'], 422, [], ['name']],",
        ] {
            assert!(php.contains(line), "missing {line:?} in\n{php}");
        }
        assert!(!php.contains("'missing' =>"));
        assert!(!php.contains("'broken' =>"));
        assert!(!php.contains("ignored"));
        assert!(php.ends_with("}\n"));
    }

    #[test]
    fn attributes_and_missing_acting_as() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(
            dir.path(),
            "[tests]\nlayout = 'folder'\nnamespace = 'Tests\\Shop'\nbase_class = '\\Tests\\ShopCase'\ndir = 'tests/Shop'\n",
        );
        put(
            &config.new.repo.join("composer.lock"),
            &json!({"packages-dev": [{"name": "phpunit/phpunit", "version": "10.0.0"}]})
                .to_string(),
        );
        let mut guest = spec("g", "GET api/ping", "guest", None);
        guest.custom = true;
        let signed = spec("p", "POST api/pong", "happy", None);
        let old_rows: HashMap<String, Value> = HashMap::from([
            (
                "g".to_string(),
                json!({"status": 200, "kind": "json", "json": {}}),
            ),
            (
                "p".to_string(),
                json!({"status": 201, "kind": "json", "json": {}}),
            ),
        ]);
        let untested: BTreeSet<String> = ["GET api/ping", "POST api/pong"].map(String::from).into();
        let out = generate(
            &config,
            &[guest, signed],
            &old_rows,
            &untested,
            &BTreeMap::new(),
            &HashSet::new(),
        );
        assert_eq!(
            out.cannot,
            vec![(
                "POST api/pong".to_string(),
                "no [tests.acting_as] api in the pair file".to_string()
            )]
        );
        let php = std::fs::read_to_string(config.new.repo.join("tests/Shop/ClosureParityTest.php"))
            .unwrap();
        assert!(php.contains("namespace Tests\\Shop;"));
        assert!(php.contains(
            " */\n#[\\PHPUnit\\Framework\\Attributes\\Group('laravel-parity')]\nclass ClosureParityTest extends \\Tests\\ShopCase"
        ));
        assert!(
            php.contains("#[\\PHPUnit\\Framework\\Attributes\\DataProvider('cases_get_api_ping')]")
        );
        assert!(php.contains("$fill = ['{user}' => $user?->getKey()];"));
        assert!(!php.contains("$this->withHeaders"));
        assert!(!php.contains("if ($signIn)"));
        assert!(php.contains("$response = $this->json('GET', \"api/ping\", $data);"));
    }
}
