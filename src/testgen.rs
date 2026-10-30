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
use crate::db::TableInfo;
use crate::glob;
use crate::plan::{PARAM, Spec, plurals, snake};
use crate::runner;

/// Characters a PHP variable name cannot have.
static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\W").unwrap());

pub const MARKER: &str = "@generated laravel-parity";
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

fn php_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn php_value(value: &Value) -> String {
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

fn render_file(config: &Config, controller: &str, methods: &[(String, String, String)]) -> String {
    let tests = &config.tests;
    let namespace = tests
        .namespace
        .as_deref()
        .unwrap_or("Tests\\Feature\\Parity");
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
        " */".into(),
        format!("class {controller}ParityTest extends \\{base}"),
        "{".into(),
    ];
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
fn phpunit_attributes(repo: &Path) -> bool {
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
        .is_some_and(|m| m >= 10)
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
    let target = config.new.repo.join(config.tests.dir());
    let mut routes: BTreeMap<&str, Vec<&Spec>> = BTreeMap::new();
    for spec in specs.iter().filter(|s| untested.contains(&s.key)) {
        routes.entry(spec.key.as_str()).or_default().push(spec);
    }
    let mut by_controller: BTreeMap<String, Vec<(String, Vec<&Spec>)>> = BTreeMap::new();
    for (key, cases) in routes {
        by_controller
            .entry(controller_class(cases[0].action.as_deref()))
            .or_default()
            .push((key.to_string(), cases));
    }

    let _ = std::fs::create_dir_all(&target);
    let mut generated = Generated {
        files: vec![],
        made: vec![],
        cannot: vec![],
    };
    for (controller, entries) in by_controller {
        let class = format!("{controller}ParityTest");
        let file = target.join(format!("{class}.php"));
        if file.exists()
            && !std::fs::read_to_string(&file)
                .unwrap_or_default()
                .contains(MARKER)
        {
            generated.cannot.extend(
                entries
                    .into_iter()
                    .map(|(k, _)| (k, format!("{class}.php was edited by hand (no marker)"))),
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
        let _ = std::fs::write(&file, render_file(config, &controller, &methods));
        generated.files.push(file);
    }
    generated
}

/// (class, method) -> failure message, or None when it passed. The whole folder in one PHPUnit run; only if that
/// dies without a report, one run per file, so a fatal error in one file cannot hide the rest.
pub fn run_phpunit(
    config: &Config,
    files: &[PathBuf],
) -> HashMap<(String, String), Option<String>> {
    let mut results = HashMap::new();
    if files.is_empty() {
        return results;
    }
    let side = &config.new;
    runner::prepare_scratch(side);
    let dir = config.tests.dir().to_string();
    if phpunit(config, &dir, &mut results)
        && files.iter().all(|f| {
            let stem = f.file_stem().unwrap().to_string_lossy();
            results.keys().any(|(c, _)| *c == stem)
        })
    {
        return results;
    }
    for file in files {
        let rel = file
            .strip_prefix(&side.repo)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        phpunit(config, &rel, &mut results);
    }
    results
}

fn phpunit(
    config: &Config,
    target: &str,
    results: &mut HashMap<(String, String), Option<String>>,
) -> bool {
    let side = &config.new;
    let junit = runner::scratch(side, "junit.xml");
    let command = config
        .tests
        .phpunit
        .clone()
        .unwrap_or_else(|| "php vendor/bin/phpunit".into());
    let mut args: Vec<&str> = command.split_whitespace().collect();
    args.extend(["--log-junit", &junit, target]);
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
        let class = case
            .attribute("class")
            .map(|c| c.rsplit('\\').next().unwrap_or(c).to_string())
            .or_else(|| {
                case.attribute("classname")
                    .map(|c| c.rsplit('.').next().unwrap_or(c).to_string())
            })
            .unwrap_or_default();
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
