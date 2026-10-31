//! Reads a pair file (one old app + one new app) and fills in the defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// Where an app's PHP runs: inside a Docker container, or a PHP binary on this machine.
#[derive(Debug, Clone)]
pub enum Php {
    Docker { container: String },
    Local { php: String },
}

/// One app: its repo, how its PHP runs, its test database and the MySQL server holding it.
#[derive(Debug, Clone)]
pub struct Side {
    pub label: &'static str,
    pub repo: PathBuf,
    pub php: Php,
    /// The Laravel root as the app's PHP sees it (inside the container, or the repo itself when local).
    pub app_path: String,
    pub database: String,
    pub db_host: String,
    pub db_port: u16,
    /// The MySQL host as the app's PHP reaches it (host.docker.internal from a container).
    pub db_app_host: String,
    pub setup: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub work: PathBuf,
    pub old: Side,
    pub new: Side,
    pub source_db: String,
    pub db_user: String,
    pub db_password: String,
    pub users: BTreeMap<String, Value>,
    pub headers: BTreeMap<String, Value>,
    pub clock: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub methods: Vec<String>,
    pub params: toml::Table,
    pub batch: usize,
    pub workers: usize,
    pub default_query: serde_json::Map<String, Value>,
    pub default_body: serde_json::Map<String, Value>,
    pub allow_hosts: Vec<String>,
    /// Which ways each route is sent: happy, guest, missing, validation.
    pub cases: Vec<String>,
    /// The id the `missing` case puts in place of real ones.
    pub missing_id: String,
    pub ignore_keys: Vec<String>,
    pub unordered_lists: bool,
    pub masks: Vec<String>,
    pub tests: Tests,
    /// The web app before the upgrade, and the one that ships with it (default: the same).
    pub web_old: Option<PathBuf>,
    pub web_new: Option<PathBuf>,
    pub console: Console,
}

/// `[console]`: the app's commands and jobs, run on both apps.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Console {
    /// false: only compare the lists and the schedule
    pub run: bool,
    /// seconds one command or job may run
    pub timeout: u64,
    /// command names or job classes not to run (globs)
    pub skip: Vec<String>,
    /// command name -> its arguments and options, e.g. { user = 1, "--dry-run" = true }
    pub commands: BTreeMap<String, Value>,
    /// job class -> constructor arguments by name (a model parameter takes an id)
    pub jobs: BTreeMap<String, Value>,
}

impl Default for Console {
    fn default() -> Self {
        Console {
            run: true,
            timeout: 120,
            skip: vec![],
            commands: BTreeMap::new(),
            jobs: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Tests {
    pub dir: Option<String>,
    pub namespace: Option<String>,
    pub base_class: Option<String>,
    pub phpunit: Option<String>,
    pub acting_as: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub scope: BTreeMap<String, String>,
    pub query: BTreeMap<String, String>,
    pub body: BTreeMap<String, String>,
}

impl Tests {
    pub fn dir(&self) -> &str {
        self.dir.as_deref().unwrap_or("tests/Feature/Parity")
    }
}

#[derive(Deserialize)]
struct RawSide {
    repo: String,
    container: Option<String>,
    php: Option<String>,
    app_path: Option<String>,
    #[serde(default)]
    setup: Vec<Value>,
    #[serde(default)]
    database: RawDb,
}

#[derive(Deserialize, Default, Clone)]
struct RawDb {
    source: Option<String>,
    old: Option<String>,
    new: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password_env: Option<String>,
    container_host: Option<String>,
    app_host: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawAuth {
    users: BTreeMap<String, Value>,
    headers: BTreeMap<String, Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawRun {
    clock: Option<String>,
    include: Option<Vec<String>>,
    exclude: Vec<String>,
    methods: Option<Vec<String>>,
    batch: Option<usize>,
    workers: Option<usize>,
    query: serde_json::Map<String, Value>,
    body: serde_json::Map<String, Value>,
    allow_hosts: Vec<String>,
    cases: Option<Vec<String>>,
    missing_id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawCompare {
    ignore_keys: Option<Vec<String>>,
    unordered_lists: Option<bool>,
    mask: Vec<String>,
}

#[derive(Deserialize)]
struct Raw {
    name: Option<String>,
    work: Option<String>,
    old: RawSide,
    new: RawSide,
    #[serde(default)]
    database: RawDb,
    #[serde(default)]
    auth: RawAuth,
    #[serde(default)]
    run: RawRun,
    #[serde(default)]
    compare: RawCompare,
    #[serde(default)]
    params: toml::Table,
    #[serde(default)]
    tests: Tests,
    #[serde(default)]
    web: RawWeb,
    #[serde(default)]
    console: Console,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawWeb {
    old: Option<String>,
    new: Option<String>,
}

/// `~/x` and `~\x` to the home folder (HOME, or USERPROFILE on Windows).
pub fn expand(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        return Path::new(&home).join(rest);
    }
    PathBuf::from(path)
}

/// A value from the repo's .env (DB_PASSWORD and friends), so credentials never go in the pair file.
pub fn env_value(repo: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(repo.join(".env")).ok()?;
    text.lines().find_map(|line| {
        line.strip_prefix(&format!("{key}="))
            .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
    })
}

fn side(label: &'static str, raw: RawSide, shared: &RawDb, database: String) -> Side {
    let own = raw.database;
    let repo = expand(&raw.repo);
    let repo = repo.canonicalize().unwrap_or(repo);
    let php = match (raw.container, raw.php) {
        (Some(container), _) => Php::Docker { container },
        (None, Some(php)) => Php::Local {
            php: expand(&php).to_string_lossy().into_owned(),
        },
        (None, None) => Php::Local { php: "php".into() },
    };
    let docker = matches!(php, Php::Docker { .. });
    let host = own
        .host
        .clone()
        .or(shared.host.clone())
        .unwrap_or_else(|| "127.0.0.1".into());
    let app_host = own
        .app_host
        .or(own.container_host)
        .or(if docker {
            shared.container_host.clone()
        } else {
            shared.app_host.clone()
        })
        .unwrap_or_else(|| {
            if docker {
                "host.docker.internal".into()
            } else {
                host.clone()
            }
        });
    let app_path = raw.app_path.filter(|_| docker).unwrap_or_else(|| {
        if docker {
            "/opt/localhost/api".into()
        } else {
            repo.to_string_lossy().into_owned()
        }
    });
    Side {
        label,
        repo,
        php,
        app_path,
        database,
        db_port: own.port.or(shared.port).unwrap_or(3306),
        db_host: host,
        db_app_host: app_host,
        setup: raw.setup,
    }
}

pub fn load(path: &Path) -> Result<Config, String> {
    let path = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let raw: Raw = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let name = raw
        .name
        .unwrap_or_else(|| path.file_stem().unwrap().to_string_lossy().into_owned());
    let db = raw.database;

    let old_db = db
        .old
        .clone()
        .unwrap_or_else(|| format!("{name}_parity_old"));
    let new_db = db
        .new
        .clone()
        .unwrap_or_else(|| format!("{name}_parity_new"));
    for test_db in [&old_db, &new_db] {
        // the tool drops and reloads these, so it only accepts names that are obviously its own
        if !test_db.contains("parity") {
            return Err(format!(
                "test database '{test_db}' must have 'parity' in its name (it is dropped and reloaded)"
            ));
        }
    }

    let old = side("old", raw.old, &db, old_db.clone());
    let new = side("new", raw.new, &db, new_db.clone());

    let password_env = db
        .password_env
        .clone()
        .unwrap_or_else(|| "PARITY_DB_PASSWORD".into());
    let db_password = std::env::var(&password_env)
        .ok()
        .or_else(|| env_value(&old.repo, "DB_PASSWORD"))
        .unwrap_or_default();
    let source_db = db
        .source
        .clone()
        .or_else(|| env_value(&old.repo, "DB_DATABASE"))
        .ok_or("database.source is not set and the old repo's .env has no DB_DATABASE")?;
    if source_db == old_db || source_db == new_db {
        return Err(
            "database.source must be the old app's real database, not one of the test databases"
                .into(),
        );
    }

    let db_user = db
        .user
        .clone()
        .or_else(|| env_value(&old.repo, "DB_USERNAME"))
        .unwrap_or_else(|| "root".into());
    let base = path
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let work = raw
        .work
        .map(|w| expand(&w))
        .unwrap_or_else(|| base.join("runs").join(&name));

    Ok(Config {
        name,
        work,
        old,
        new,
        source_db,
        db_user,
        db_password,
        users: raw.auth.users,
        headers: raw.auth.headers,
        clock: raw.run.clock.unwrap_or_else(|| "auto".into()),
        include: raw.run.include.unwrap_or_else(|| vec!["api/*".into()]),
        exclude: raw.run.exclude,
        methods: raw
            .run
            .methods
            .unwrap_or_else(|| {
                ["GET", "POST", "PUT", "PATCH", "DELETE"]
                    .map(String::from)
                    .to_vec()
            })
            .into_iter()
            .map(|m| m.to_uppercase())
            .collect(),
        params: raw.params,
        batch: raw.run.batch.unwrap_or(100).max(1),
        workers: raw.run.workers.unwrap_or(4).max(1),
        default_query: raw.run.query,
        default_body: raw.run.body,
        allow_hosts: raw.run.allow_hosts,
        cases: raw.run.cases.unwrap_or_else(|| {
            ["happy", "guest", "missing", "validation"]
                .map(String::from)
                .to_vec()
        }),
        missing_id: raw.run.missing_id.unwrap_or_else(|| "999999999".into()),
        ignore_keys: raw.compare.ignore_keys.unwrap_or_else(|| {
            [
                "created_at",
                "updated_at",
                "deleted_at",
                "*_token",
                "token",
                "access_token",
                "expires_at",
                "expires_in",
            ]
            .map(String::from)
            .to_vec()
        }),
        unordered_lists: raw.compare.unordered_lists.unwrap_or(true),
        masks: raw.compare.mask,
        tests: raw.tests,
        web_old: raw.web.old.as_deref().map(expand),
        web_new: raw.web.new.as_deref().map(expand),
        console: raw.console,
    })
}
