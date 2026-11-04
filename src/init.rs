//! `init`: a first pair file for an old and a new repo, filled in from what the tool can find (the Docker container
//! that mounts each repo, the database settings, the sign-in guards), with TODO marks for the rest.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;

use crate::config::{env_value, expand};

struct Found {
    container: Option<(String, String)>,
    /// The container exists but is stopped.
    stopped: bool,
    php: Option<String>,
}

/// The container that mounts this folder (prefer a running one named backend/app/php), the path inside it, and
/// whether it is running.
fn container_for(repo: &Path) -> Option<(String, String, bool)> {
    let names = Command::new("docker")
        .args(["ps", "-a", "--format", "{{.Names}}|{{.State}}"])
        .output()
        .ok()?;
    if !names.status.success() {
        return None;
    }
    let repo = crate::paths::real(repo).ok()?;
    let mut candidates = vec![];
    for line in String::from_utf8_lossy(&names.stdout).lines() {
        let Some((name, state)) = line.split_once('|') else {
            continue;
        };
        let Ok(mounts) = Command::new("docker")
            .args([
                "inspect",
                "-f",
                "{{range .Mounts}}{{.Source}}|{{.Destination}};{{end}}",
                name,
            ])
            .output()
        else {
            continue;
        };
        for mount in String::from_utf8_lossy(&mounts.stdout).split(';') {
            let Some((source, destination)) = mount.split_once('|') else {
                continue;
            };
            if crate::paths::real(Path::new(source)).ok().as_deref() == Some(repo.as_path()) {
                candidates.push((
                    name.to_string(),
                    destination.to_string(),
                    state == "running",
                ));
            }
        }
    }
    let score = |name: &str| {
        ["backend", "app", "php", "api"]
            .iter()
            .position(|k| name.contains(k))
            .unwrap_or(9)
    };
    candidates.sort_by_key(|(name, _, running)| (!running, score(name), name.contains("worker")));
    // a running one that has PHP; else the best-named stopped one (it cannot be asked)
    candidates
        .iter()
        .find(|(name, _, running)| {
            *running
                && Command::new("docker")
                    .args(["exec", name, "php", "-v"])
                    .output()
                    .is_ok_and(|o| o.status.success())
        })
        .or_else(|| {
            candidates.iter().find(|(name, _, running)| {
                !running && !name.contains("frontend") && !name.contains("web")
            })
        })
        .cloned()
}

fn find(repo: &Path) -> Found {
    match container_for(repo) {
        Some((name, path, running)) => Found {
            container: Some((name, path)),
            stopped: !running,
            php: None,
        },
        None => Found {
            container: None,
            stopped: false,
            php: Some(if cfg!(windows) {
                "php.exe".into()
            } else {
                "php".into()
            }),
        },
    }
}

/// Passport guards in config/auth.php: (guard, provider, model).
fn guards(repo: &Path) -> Vec<(String, String, String)> {
    static GUARD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"'(\w+)'\s*=>\s*\[\s*'driver'\s*=>\s*'passport'\s*,\s*'provider'\s*=>\s*'(\w+)'"#,
        )
        .unwrap()
    });
    let text = std::fs::read_to_string(repo.join("config/auth.php")).unwrap_or_default();
    GUARD
        .captures_iter(&text)
        .map(|c| {
            let provider = c[2].to_string();
            let model = Regex::new(&format!(
                r#"'{provider}'\s*=>\s*\[[^\]]*?'model'\s*=>\s*([\w\\]+)::class"#
            ))
            .ok()
            .and_then(|r| r.captures(&text))
            .map(|m| m[1].trim_start_matches('\\').to_string())
            .unwrap_or_default();
            (c[1].to_string(), provider, model)
        })
        .collect()
}

fn side_block(out: &mut String, label: &str, repo: &Path, found: &Found, setup: &[String]) {
    let _ = writeln!(out, "[{label}]\nrepo = {:?}", repo.to_string_lossy());
    match (&found.container, &found.php) {
        (Some((container, path)), _) => {
            let _ = writeln!(out, "container = {container:?}\napp_path = {path:?}");
            let _ = writeln!(
                out,
                "# without Docker: php = \"/path/to/php\" instead of container/app_path"
            );
        }
        (None, Some(php)) => {
            let _ = writeln!(
                out,
                "# TODO: no running Docker container mounts this repo; using a PHP on this machine."
            );
            let _ = writeln!(
                out,
                "# Point it at this app's PHP version, or start the app's Docker stack and run init again."
            );
            let _ = writeln!(out, "php = {php:?}");
        }
        _ => {}
    }
    if !setup.is_empty() {
        let _ = writeln!(
            out,
            "setup = [\n{}]",
            setup
                .iter()
                .map(|s| format!("    {s},\n"))
                .collect::<String>()
        );
    }
    out.push('\n');
}

/// Write the pair file; returns its path and the TODO lines for the person (or the AI) to finish.
pub fn init(
    old: &Path,
    new: &Path,
    name: Option<&str>,
    out: Option<&Path>,
    force: bool,
) -> Result<(PathBuf, Vec<String>), String> {
    for repo in [old, new] {
        if !repo.join("artisan").exists() {
            return Err(format!(
                "{} is not a Laravel app (no artisan)",
                repo.display()
            ));
        }
    }
    let old = crate::paths::real(old).map_err(|e| e.to_string())?;
    let new = crate::paths::real(new).map_err(|e| e.to_string())?;
    let name = name
        .map(String::from)
        .unwrap_or_else(|| {
            new.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or("app".into())
        })
        .to_lowercase()
        .replace(|c: char| !c.is_alphanumeric() && c != '_', "_");
    let path = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| tool_root().join("pairs").join(format!("{name}.toml")));
    if path.exists() && !force {
        return Err(format!(
            "{} exists already (pass force to replace it)",
            path.display()
        ));
    }

    let (old_found, new_found) = (find(&old), find(&new));
    let mut todo = vec![];
    let passport = |repo: &Path| {
        std::fs::read_to_string(repo.join("composer.json"))
            .is_ok_and(|c| c.contains("laravel/passport"))
    };
    let new_guards = guards(&new);
    let first = new_guards
        .iter()
        .find(|(g, _, _)| g != "api")
        .or(new_guards.first())
        .cloned();

    let mut old_setup = vec![];
    if passport(&old) {
        old_setup.push(r#"["passport:client", { "--personal" = true, "--name" = "laravel-parity", "--no-interaction" = true }]"#.to_string());
    }
    let mut new_setup = vec![
        r#"["migrate", { "--force" = true, "--schema-path" = "/nonexistent/no-schema-dump.sql" }]"#
            .to_string(),
    ];
    if passport(&new) {
        new_setup.push(r#"["passport:hash", { "--force" = true }]"#.to_string());
        let provider = first
            .as_ref()
            .map(|(_, p, _)| p.clone())
            .unwrap_or_else(|| "users".into());
        new_setup.push(format!(
            r#"["passport:client", {{ "--personal" = true, "--name" = "laravel-parity", "--provider" = "{provider}", "--no-interaction" = true }}]"#
        ));
    }

    let source = env_value(&old, "DB_DATABASE");
    let db_host = env_value(&old, "DB_HOST").unwrap_or_else(|| "127.0.0.1".into());
    let port = env_value(&old, "DB_PORT").unwrap_or_else(|| "3306".into());
    let docker_host = db_host == "host.docker.internal";
    if source.is_none() {
        todo.push("[database] source: the old app's database name (not found in its .env)".into());
    }

    let mut text = String::new();
    let _ = writeln!(
        text,
        "# {} -> {}\n# Made by `laravel-parity init`. Lines marked TODO need you.\n",
        old.display(),
        new.display()
    );
    let _ = writeln!(text, "name = {name:?}\n");
    side_block(&mut text, "old", &old, &old_found, &old_setup);
    side_block(&mut text, "new", &new, &new_found, &new_setup);
    for (label, found) in [("old", &old_found), ("new", &new_found)] {
        match &found.container {
            None => todo.push(format!(
                "[{label}] php: the PHP for the {label} app (no Docker container mounts it)"
            )),
            Some((name, _)) if found.stopped => todo.push(format!(
                "start the {label} app's Docker stack: container {name} is stopped"
            )),
            _ => {}
        }
    }

    let _ = writeln!(
        text,
        "[database]\nsource = {:?}   # the old app's database: copied, never written",
        source.unwrap_or_default()
    );
    let _ = writeln!(
        text,
        "old = \"{name}_parity_old\"\nnew = \"{name}_parity_new\""
    );
    let _ = writeln!(
        text,
        "host = {:?}\nport = {port}",
        if docker_host {
            "127.0.0.1".to_string()
        } else {
            db_host.clone()
        }
    );
    if docker_host || old_found.container.is_some() {
        let _ = writeln!(
            text,
            "container_host = \"host.docker.internal\"   # MySQL as the containers reach it"
        );
    }
    let _ = writeln!(
        text,
        "# the password comes from PARITY_DB_PASSWORD, else from the old repo's .env\n"
    );

    let _ = writeln!(text, "[auth]");
    match &first {
        Some((guard, _, model)) => {
            let _ = writeln!(
                text,
                "# TODO: the id of an admin user ({model}, guard {guard}) in the old database"
            );
            let _ = writeln!(text, "users = {{ {guard} = 1 }}");
            todo.push(format!(
                "[auth] users: the id of an admin user ({model}, guard {guard}) in the old database"
            ));
        }
        None => {
            let _ = writeln!(
                text,
                "# TODO: guard = user id; no Passport guard found in config/auth.php"
            );
            let _ = writeln!(text, "users = {{}}");
            todo.push("[auth] users: guard = user id to make the requests as".into());
        }
    }
    let _ = writeln!(
        text,
        "# headers every request sends, e.g. the project the web app has selected\nheaders = {{}}\n"
    );

    let _ = writeln!(
        text,
        "[run]\ninclude = [\"api/*\"]\nexclude = [\"api/oauth/*\", \"POST api/*/logout*\", \"* api/*/import*\"]"
    );
    let _ = writeln!(
        text,
        "workers = 4\n# what the web app sends with almost every call, e.g. {{ project_id = \"1\" }}\nquery = {{}}\nbody = {{}}\n"
    );
    let _ = writeln!(
        text,
        "[compare]\nignore_keys = [\"created_at\", \"updated_at\", \"deleted_at\", \"*token*\", \"expires_at\", \"expires_in\", \"$.meta.links[*].page\"]\nmask = []\n"
    );
    let _ = writeln!(
        text,
        "# route parameters the tool cannot fill: by name, by \"METHOD uri\", or sql:SELECT ...\n[params]\n"
    );

    let base = if new.join("tests/TestCase.php").exists() {
        "Tests\\TestCase"
    } else {
        "PHPUnit\\Framework\\TestCase"
    };
    let _ = writeln!(
        text,
        "[tests]\n# next to the app's own tests, following app/ (\"folder\" puts the feature tests in one dir)\nlayout = \"mirror\"\nbase_class = {base:?}\nphpunit = \"php vendor/bin/phpunit\"\ntarget = 80"
    );
    if let Some((guard, _, model)) = &first {
        // {:?} escapes backslashes and quotes the way a TOML basic string wants them
        let snippet =
            format!("$user = \\{model}::factory()->create(); $this->actingAs($user, '{guard}')");
        let _ = writeln!(text, "acting_as = {{ {guard} = {snippet:?} }}");
    }
    let _ = writeln!(
        text,
        "headers = {{}}\nquery = {{}}\nbody = {{}}\n# factory overrides by column pattern, e.g. {{ \"*project_id\" = \"$this->project->getKey()\" }}\nscope = {{}}"
    );

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    // what it wrote must load
    crate::config::load(&path)
        .map_err(|e| format!("wrote {}, but it does not load: {e}", path.display()))?;
    Ok((path, todo))
}

/// The tool's folder: LARAVEL_PARITY_HOME, else the folder holding target/ when run from a build, else the
/// binary's folder.
pub fn tool_root() -> PathBuf {
    if let Ok(home) = std::env::var("LARAVEL_PARITY_HOME") {
        return expand(&home);
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|e| crate::paths::real(&e).ok())
        .unwrap_or_default();
    let mut dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    for _ in 0..3 {
        if dir.join("Cargo.toml").exists() || dir.join("pairs").exists() {
            return dir;
        }
        if !dir.pop() {
            break;
        }
    }
    exe.parent().map(Path::to_path_buf).unwrap_or_default()
}
