//! Runs php/replay.php with an app's own PHP (in its Docker container, or a PHP binary on this machine) and reads
//! the results back.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};

use crate::config::{Config, Php, Side};

/// The runner ships inside the binary, so the tool is one file to copy.
pub const REPLAY_PHP: &str = include_str!("../php/replay.php");
const REMOTE_DIR: &str = "/tmp/laravel-parity";
/// Outgoing HTTP that bypasses Laravel's client (Guzzle, curl, PHP streams) goes to a proxy that does not exist.
const BLACKHOLE: &str = "http://127.0.0.1:9";
/// replay.php's exit code for "stop me and run the rest in a new process".
const RESTART: i32 = 75;

fn docker(args: &[&str]) -> Result<Output, String> {
    Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| format!("docker: {e}"))
}

fn checked(output: Output, what: &str) -> Result<Output, String> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "{what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub fn ensure_ready(side: &Side) -> Result<(), String> {
    match &side.php {
        Php::Docker { container } => {
            let out = docker(&["inspect", "-f", "{{.State.Running}}", container])?;
            if String::from_utf8_lossy(&out.stdout).trim() != "true" {
                return Err(format!(
                    "the {} app's container '{container}' is not running; start its stack first",
                    side.label
                ));
            }
        }
        Php::Local { php } => {
            let out = Command::new(php)
                .arg("-v")
                .output()
                .map_err(|e| format!("{php}: {e}"))?;
            if !out.status.success() {
                return Err(format!("{php} -v failed"));
            }
            if !Path::new(&side.app_path).join("artisan").exists() {
                return Err(format!(
                    "{} is not a Laravel app (no artisan)",
                    side.app_path
                ));
            }
        }
    }
    Ok(())
}

pub fn database_settings(config: &Config, side: &Side) -> Value {
    json!({
        "host": side.db_app_host,
        "port": side.db_port,
        "database": side.database,
        "username": config.db_user,
        "password": config.db_password,
    })
}

fn proxy_env(config: &Config) -> Vec<(String, String)> {
    let mut no_proxy = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    no_proxy.extend(config.allow_hosts.iter().cloned());
    let mut env: Vec<(String, String)> = [
        "http_proxy",
        "https_proxy",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .map(|n| (n.to_string(), BLACKHOLE.to_string()))
    .collect();
    env.push(("no_proxy".into(), no_proxy.join(",")));
    env.push(("NO_PROXY".into(), no_proxy.join(",")));
    env
}

/// One runner process: the job in, JSON lines out. `slot` keeps parallel workers' files apart.
pub fn run_job(
    config: &Config,
    side: &Side,
    job: &Value,
    slot: usize,
) -> Result<Vec<Value>, String> {
    run_job_timed(config, side, job, slot, None)
}

/// A child's output, killed (and `timed_out`) when it runs longer than `limit`. `stop` kills what the child started
/// elsewhere (the PHP process inside a container, which killing `docker exec` leaves running).
fn wait_output(
    mut command: Command,
    limit: Option<Duration>,
    stop: impl Fn(),
) -> Result<(Output, bool), String> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = vec![];
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = vec![];
        let _ = err.read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if limit.is_some_and(|l| started.elapsed() > l) {
            timed_out = true;
            stop();
            let _ = child.kill();
            break child.wait().map_err(|e| e.to_string())?;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok((
        Output {
            status,
            stdout: reader.join().unwrap_or_default(),
            stderr: err_reader.join().unwrap_or_default(),
        },
        timed_out,
    ))
}

/// `run_job`, stopped after `timeout` seconds (a console command that never ends); the rows written so far come
/// back with a `runner_timeout` row.
pub fn run_job_timed(
    config: &Config,
    side: &Side,
    job: &Value,
    slot: usize,
    timeout: Option<u64>,
) -> Result<Vec<Value>, String> {
    let limit = timeout.map(Duration::from_secs);
    let mut job = job.clone();
    job["database"] = database_settings(config, side);
    let job_text = serde_json::to_string(&job).unwrap();
    let local = tempfile::tempdir().map_err(|e| e.to_string())?;
    let job_file = local.path().join("job.json");
    let out_file = local.path().join("out.jsonl");
    let runner = local.path().join("replay.php");
    std::fs::write(&runner, REPLAY_PHP).map_err(|e| e.to_string())?;
    // the job holds the database password: owner-only, and removed with the temp folder
    write_private(&job_file, &job_text)?;

    let output = match &side.php {
        Php::Docker { container } => {
            let dir = format!("{REMOTE_DIR}/{slot}");
            checked(
                docker(&["exec", container, "mkdir", "-p", &dir])?,
                "docker exec mkdir",
            )?;
            checked(
                docker(&[
                    "cp",
                    &path_str(&runner),
                    &format!("{container}:{dir}/replay.php"),
                ])?,
                "docker cp",
            )?;
            checked(
                docker(&[
                    "cp",
                    &path_str(&job_file),
                    &format!("{container}:{dir}/job.json"),
                ])?,
                "docker cp",
            )?;
            let mut command = Command::new("docker");
            command.arg("exec");
            for (k, v) in proxy_env(config) {
                command.args(["-e", &format!("{k}={v}")]);
            }
            command.args([
                container.as_str(),
                "php",
                &format!("{dir}/replay.php"),
                &format!("--app={}", side.app_path),
                &format!("--job={dir}/job.json"),
                &format!("--out={dir}/out.jsonl"),
            ]);
            let pid_file = format!("{dir}/pid");
            let result = wait_output(command, limit, || {
                let _ = docker(&[
                    "exec",
                    container,
                    "sh",
                    "-c",
                    &format!("kill -9 $(cat {pid_file}) 2>/dev/null"),
                ]);
            });
            let _ = docker(&["exec", container, "rm", "-f", &format!("{dir}/job.json")]);
            let _ = docker(&[
                "cp",
                &format!("{container}:{dir}/out.jsonl"),
                &path_str(&out_file),
            ]);
            let _ = docker(&["exec", container, "rm", "-f", &format!("{dir}/out.jsonl")]);
            result?
        }
        Php::Local { php } => {
            let mut command = Command::new(php);
            command
                .arg(&runner)
                .arg(format!("--app={}", side.app_path))
                .arg(format!("--job={}", path_str(&job_file)))
                .arg(format!("--out={}", path_str(&out_file)))
                .current_dir(&side.app_path);
            for (k, v) in proxy_env(config) {
                command.env(k, v);
            }
            wait_output(command, limit, || {}).map_err(|e| format!("{php}: {e}"))?
        }
    };
    let (output, timed_out) = output;

    let mut rows: Vec<Value> = std::fs::read_to_string(&out_file)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if timed_out {
        rows.push(json!({"runner_timeout": timeout}));
        return Ok(rows);
    }
    if output.status.code() == Some(RESTART) && !rows.is_empty() {
        // the runner stopped on purpose (its output was spoiled): the requests it did not reach run again
        rows.push(json!({"runner_restart": true}));
        return Ok(rows);
    }
    if !output.status.success() || rows.is_empty() {
        // Laravel's console error handler can exit 0 after printing, so no output at all is a failure too
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        let detail = crash_summary(&text, output.status.code(), signal(&output.status));
        if rows.is_empty() {
            return Err(format!(
                "runner failed for the {} app (exit {:?}): {detail}",
                side.label,
                output.status.code()
            ));
        }
        rows.push(json!({"runner_crash": detail, "exit": output.status.code()}));
    }
    Ok(rows)
}

#[cfg(unix)]
fn signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal(_: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// What a PHP process that stopped said: the line that names the error (a fatal error, an uncaught exception) with
/// its file:line, not the stack trace under it; else the first line it printed; else how it ended (exit code or
/// signal: through docker a signal shows as 128 + its number).
pub fn crash_summary(text: &str, code: Option<i32>, signal: Option<i32>) -> String {
    static ERROR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)fatal error|parse error|uncaught|\b\w*(exception|error)\b\s*:").unwrap()
    });
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("Stack trace"))
        .collect();
    if let Some(line) = lines.iter().find(|l| ERROR.is_match(l)).or(lines.first()) {
        return line.chars().take(500).collect();
    }
    match (code, signal) {
        (_, Some(9)) | (Some(137), _) => {
            "PHP printed nothing and was killed (signal 9: usually out of memory)".into()
        }
        (_, Some(11)) | (Some(139), _) => {
            "PHP printed nothing and crashed (segmentation fault)".into()
        }
        (_, Some(s)) => format!("PHP printed nothing and was stopped by signal {s}"),
        (Some(c), _) => format!("PHP printed nothing and exited with code {c}"),
        (None, None) => "PHP printed nothing and stopped".into(),
    }
}

/// A shell command in the app's folder (PHPUnit), with the app's PHP.
pub fn run_in_app(side: &Side, args: &[&str]) -> Result<Output, String> {
    match &side.php {
        Php::Docker { container } => {
            let mut command = Command::new("docker");
            command
                .args(["exec", "-w", &side.app_path, container])
                .args(args);
            command.output().map_err(|e| e.to_string())
        }
        Php::Local { php } => {
            let mut command = if args.first() == Some(&"php") {
                Command::new(php)
            } else {
                Command::new(args[0])
            };
            command.args(&args[1..]).current_dir(&side.app_path);
            command.output().map_err(|e| e.to_string())
        }
    }
}

/// Copy a file the app wrote (a JUnit report) to this machine.
pub fn fetch(side: &Side, remote: &str, local: &Path) -> bool {
    match &side.php {
        Php::Docker { container } => {
            docker(&["cp", &format!("{container}:{remote}"), &path_str(local)])
                .is_ok_and(|o| o.status.success())
        }
        Php::Local { .. } => std::fs::copy(Path::new(&side.app_path).join(remote), local).is_ok(),
    }
}

/// Copy a file from this machine to the app's scratch folder; returns the path the app's PHP sees.
pub fn put(side: &Side, local: &Path, name: &str) -> Result<String, String> {
    let remote = scratch(side, name);
    match &side.php {
        Php::Docker { container } => {
            checked(
                docker(&["cp", &path_str(local), &format!("{container}:{remote}")])?,
                "docker cp",
            )?;
        }
        Php::Local { .. } => {
            std::fs::copy(local, Path::new(&side.app_path).join(&remote))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(remote)
}

/// Remove a scratch file.
pub fn remove(side: &Side, remote: &str) {
    match &side.php {
        Php::Docker { container } => {
            let _ = docker(&["exec", container, "rm", "-f", remote]);
        }
        Php::Local { .. } => {
            let _ = std::fs::remove_file(Path::new(&side.app_path).join(remote));
        }
    }
}

/// Where a scratch file goes, as the app's PHP sees it.
pub fn scratch(side: &Side, name: &str) -> String {
    match side.php {
        Php::Docker { .. } => format!("{REMOTE_DIR}/{name}"),
        Php::Local { .. } => format!("storage/framework/testing/{name}"),
    }
}

pub fn prepare_scratch(side: &Side) {
    if let Php::Docker { container } = &side.php {
        let _ = docker(&["exec", container, "mkdir", "-p", REMOTE_DIR]);
    } else {
        let _ =
            std::fs::create_dir_all(Path::new(&side.app_path).join("storage/framework/testing"));
    }
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn write_private(path: &PathBuf, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_summaries() {
        // the error line, not the stack trace under it
        let fatal = "PHP Fatal error:  Cannot redeclare build_calendar() (previously declared in /app/storage/framework/views/abc.php:12) in /app/storage/framework/views/abc.php on line 12\nStack trace:\n#0 /app/vendor/laravel/framework/src/Illuminate/Pipeline/Pipeline.php(180): x()\n#64 /app/vendor/laravel/framework/src/Illuminate/Foundation/Http/Kernel.php(175): Illuminate\\Pipeline\\Pipeline->then()\n";
        assert!(
            crash_summary(fatal, Some(255), None)
                .starts_with("PHP Fatal error:  Cannot redeclare build_calendar()")
        );
        let uncaught = "\nPHP Warning:  something\nPHP Fatal error:  Uncaught RuntimeException: boom in /app/app/X.php:3\n#0 {main}\n";
        assert_eq!(
            crash_summary(uncaught, Some(255), None),
            "PHP Fatal error:  Uncaught RuntimeException: boom in /app/app/X.php:3"
        );
        assert_eq!(crash_summary("just this\n", Some(1), None), "just this");
        // nothing printed: say how it ended
        assert_eq!(
            crash_summary("\n", Some(137), None),
            "PHP printed nothing and was killed (signal 9: usually out of memory)"
        );
        assert_eq!(
            crash_summary("", None, Some(11)),
            "PHP printed nothing and crashed (segmentation fault)"
        );
        assert_eq!(
            crash_summary("", Some(0), None),
            "PHP printed nothing and exited with code 0"
        );
    }
}
