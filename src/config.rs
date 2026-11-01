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
    /// rounds of filling the fields the old app's validation errors name, then sending again (0 = off)
    pub learn: usize,
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
    learn: Option<usize>,
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
        learn: raw.run.learn.unwrap_or(4),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `text` as `<dir>/pairs/<file>` in a fresh temp folder and returns the pair file's path.
    fn pair(dir: &str, file: &str, text: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "laravel-parity-config-{}-{dir}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("pairs")).unwrap();
        let path = root.join("pairs").join(file);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn defaults() {
        let path = pair(
            "defaults",
            "shop.toml",
            r#"
            [old]
            repo = "/nowhere/shop-old"
            container = "shop-old-php"
            [new]
            repo = "/nowhere/shop-new"
            [database]
            source = "shop"
            "#,
        );
        let c = load(&path).unwrap();
        assert_eq!(c.name, "shop");
        assert_eq!(c.old.database, "shop_parity_old");
        assert_eq!(c.new.database, "shop_parity_new");
        assert_eq!(c.source_db, "shop");
        assert_eq!(c.db_user, "root");
        let root = path.canonicalize().unwrap();
        let root = root.parent().unwrap().parent().unwrap();
        assert_eq!(c.work, root.join("runs").join("shop"));
        assert_eq!(c.clock, "auto");
        assert_eq!(c.include, vec!["api/*"]);
        assert!(c.exclude.is_empty());
        assert_eq!(c.methods, vec!["GET", "POST", "PUT", "PATCH", "DELETE"]);
        assert_eq!((c.batch, c.workers, c.learn), (100, 4, 4));
        assert_eq!(c.cases, vec!["happy", "guest", "missing", "validation"]);
        assert_eq!(c.missing_id, "999999999");
        assert!(c.ignore_keys.iter().any(|k| k == "*_token"));
        assert!(c.unordered_lists);
        assert!(c.masks.is_empty());
        assert_eq!(c.tests.dir(), "tests/Feature/Parity");
        assert!(c.console.run);
        assert_eq!(c.console.timeout, 120);
        assert!(c.web_old.is_none() && c.web_new.is_none());

        // old runs in Docker: the container's app path and host.docker.internal
        assert!(matches!(&c.old.php, Php::Docker { container } if container == "shop-old-php"));
        assert_eq!(c.old.app_path, "/opt/localhost/api");
        assert_eq!(c.old.db_host, "127.0.0.1");
        assert_eq!(c.old.db_app_host, "host.docker.internal");
        assert_eq!(c.old.db_port, 3306);
        assert_eq!(c.old.label, "old");
        // new runs on local php: the repo is the app path, the app reaches MySQL on the same host
        assert!(matches!(&c.new.php, Php::Local { php } if php == "php"));
        assert_eq!(c.new.app_path, "/nowhere/shop-new");
        assert_eq!(c.new.db_app_host, "127.0.0.1");
        assert_eq!(c.new.label, "new");
    }

    #[test]
    fn overrides() {
        let path = pair(
            "overrides",
            "ignored.toml",
            r#"
            name = "shop"
            work = "/nowhere/runs"
            [old]
            repo = "/nowhere/shop-old"
            container = "shop-old-php"
            app_path = "/var/www/shop"
            setup = ["php artisan cache:clear"]
            [old.database]
            host = "db.example.com"
            port = 3307
            container_host = "mysql"
            [new]
            repo = "/nowhere/shop-new"
            php = "/usr/local/bin/php8"
            app_path = "/ignored/when/local"
            [database]
            source = "shop_live"
            old = "shop_parity_a"
            new = "parity_shop_b"
            user = "tester"
            port = 3308
            app_host = "10.0.0.2"
            [auth.users]
            web = 1
            [auth.headers]
            my-project-id = "7"
            [run]
            clock = "2026-01-01 00:00:00"
            include = ["*"]
            exclude = ["api/health"]
            methods = ["get", "post"]
            batch = 0
            workers = 0
            learn = 0
            cases = ["happy"]
            missing_id = "0"
            query = { project_id = 7 }
            body = { company_id = "3" }
            [compare]
            ignore_keys = ["id"]
            unordered_lists = false
            mask = ["data.*.uuid"]
            [params]
            unit = 5
            [tests]
            dir = "tests/Parity"
            [web]
            old = "/nowhere/web-old"
            [console]
            run = false
            skip = ["queue:*"]
            "#,
        );
        let c = load(&path).unwrap();
        assert_eq!(c.name, "shop");
        assert_eq!(c.work, PathBuf::from("/nowhere/runs"));
        assert_eq!(c.old.database, "shop_parity_a");
        assert_eq!(c.new.database, "parity_shop_b");
        assert_eq!(c.source_db, "shop_live");
        assert_eq!(c.db_user, "tester");
        // a side's own database settings win over the shared ones
        assert_eq!(c.old.db_host, "db.example.com");
        assert_eq!(c.old.db_port, 3307);
        assert_eq!(c.old.db_app_host, "mysql");
        assert_eq!(c.old.app_path, "/var/www/shop");
        assert_eq!(
            c.old.setup,
            vec![serde_json::json!("php artisan cache:clear")]
        );
        assert_eq!(c.new.db_host, "127.0.0.1");
        assert_eq!(c.new.db_port, 3308);
        assert_eq!(c.new.db_app_host, "10.0.0.2");
        assert!(matches!(&c.new.php, Php::Local { php } if php == "/usr/local/bin/php8"));
        assert_eq!(c.new.app_path, "/nowhere/shop-new");
        assert_eq!(c.users["web"], serde_json::json!(1));
        assert_eq!(c.headers["my-project-id"], serde_json::json!("7"));
        assert_eq!(c.clock, "2026-01-01 00:00:00");
        assert_eq!(c.include, vec!["*"]);
        assert_eq!(c.exclude, vec!["api/health"]);
        assert_eq!(c.methods, vec!["GET", "POST"]);
        // 0 batch and workers would never send anything
        assert_eq!((c.batch, c.workers, c.learn), (1, 1, 0));
        assert_eq!(c.cases, vec!["happy"]);
        assert_eq!(c.missing_id, "0");
        assert_eq!(c.default_query["project_id"], serde_json::json!(7));
        assert_eq!(c.default_body["company_id"], serde_json::json!("3"));
        assert_eq!(c.ignore_keys, vec!["id"]);
        assert!(!c.unordered_lists);
        assert_eq!(c.masks, vec!["data.*.uuid"]);
        assert_eq!(c.params["unit"], toml::Value::Integer(5));
        assert_eq!(c.tests.dir(), "tests/Parity");
        assert_eq!(c.web_old, Some(PathBuf::from("/nowhere/web-old")));
        assert!(c.web_new.is_none());
        assert!(!c.console.run);
        assert_eq!(c.console.timeout, 120);
        assert_eq!(c.console.skip, vec!["queue:*"]);
    }

    #[test]
    fn refusals() {
        let sides = "[old]\nrepo = \"/nowhere/a\"\n[new]\nrepo = \"/nowhere/b\"\n";
        // a test database without "parity" in its name is never dropped
        let path = pair(
            "unsafe",
            "shop.toml",
            &format!("{sides}[database]\nsource = \"shop\"\nnew = \"shop_copy\"\n"),
        );
        let err = load(&path).unwrap_err();
        assert!(
            err.contains("'shop_copy'") && err.contains("parity"),
            "{err}"
        );
        // the source cannot be one of the test databases
        let path = pair(
            "same",
            "shop.toml",
            &format!("{sides}[database]\nsource = \"shop_parity_old\"\n"),
        );
        assert!(load(&path).unwrap_err().contains("database.source"));
        // no source and no old .env to read DB_DATABASE from
        let path = pair("nosource", "shop.toml", sides);
        assert!(load(&path).unwrap_err().contains("DB_DATABASE"));
        // bad TOML, a missing side and a missing file name the file
        let path = pair("bad", "shop.toml", "[old\nrepo = 1");
        assert!(load(&path).unwrap_err().contains("shop.toml"));
        let path = pair("noside", "shop.toml", "[old]\nrepo = \"/nowhere/a\"\n");
        assert!(load(&path).unwrap_err().contains("new"));
        assert!(load(Path::new("/nowhere/missing.toml")).is_err());
    }

    #[test]
    fn env_file() {
        let repo =
            std::env::temp_dir().join(format!("laravel-parity-config-{}-env", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(".env"),
            "APP_NAME=Shop\nDB_DATABASE=\"shop\"\nDB_USERNAME='shop_user'\nDB_PASSWORD=  secret  \nDB_HOST=\n",
        )
        .unwrap();
        assert_eq!(env_value(&repo, "DB_DATABASE").as_deref(), Some("shop"));
        assert_eq!(
            env_value(&repo, "DB_USERNAME").as_deref(),
            Some("shop_user")
        );
        assert_eq!(env_value(&repo, "DB_PASSWORD").as_deref(), Some("secret"));
        assert_eq!(env_value(&repo, "DB_HOST").as_deref(), Some(""));
        // a key is matched whole, not as a prefix of another
        assert_eq!(env_value(&repo, "DB"), None);
        assert_eq!(env_value(&repo, "APP_KEY"), None);
        assert_eq!(env_value(Path::new("/nowhere"), "DB_DATABASE"), None);

        // without database.source / user, the old repo's .env fills them
        let path = pair(
            "fromenv",
            "shop.toml",
            &format!(
                "[old]\nrepo = {:?}\n[new]\nrepo = \"/nowhere/b\"\n",
                repo.to_string_lossy()
            ),
        );
        let c = load(&path).unwrap();
        assert_eq!(c.source_db, "shop");
        assert_eq!(c.db_user, "shop_user");
        assert_eq!(c.old.repo, repo.canonicalize().unwrap());
    }

    #[test]
    fn home_paths() {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        assert_eq!(expand("~/code/shop"), Path::new(&home).join("code/shop"));
        assert_eq!(expand("~\\code"), Path::new(&home).join("code"));
        assert_eq!(expand("/srv/shop"), PathBuf::from("/srv/shop"));
        assert_eq!(expand("code/~/shop"), PathBuf::from("code/~/shop"));
        // only "~/" is the home folder, not "~user"
        assert_eq!(expand("~shop"), PathBuf::from("~shop"));
    }
}
