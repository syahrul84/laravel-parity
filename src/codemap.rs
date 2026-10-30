//! Talks to codemap over MCP stdio: one `codemap serve` per repo, so the index is scanned once, not once per question.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

use crate::config::expand;

/// CODEMAP_BIN, else codemap on the PATH, else ~/.local/bin/codemap.
pub fn binary() -> PathBuf {
    if let Ok(bin) = std::env::var("CODEMAP_BIN") {
        return PathBuf::from(bin);
    }
    let exe = if cfg!(windows) {
        "codemap.exe"
    } else {
        "codemap"
    };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    expand(&format!("~/.local/bin/{exe}"))
}

pub struct Codemap {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Codemap {
    pub fn start(project: &Path) -> Result<Self, String> {
        Self::spawn(project, &[])
    }

    /// A frontend linked to a backend (`backend=/path/to/api`), under its own index id so it never mixes with the
    /// frontend's usual index.
    pub fn start_linked(project: &Path, id: &str, link: &str) -> Result<Self, String> {
        Self::spawn(project, &["--project-id", id, "--link", link])
    }

    fn spawn(project: &Path, extra: &[&str]) -> Result<Self, String> {
        let bin = binary();
        let mut child = Command::new(&bin)
            .args(["serve", "--project"])
            .arg(project)
            .args(["--log-level", "warn"])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start codemap ({}): {e}", bin.display()))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut codemap = Codemap {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        codemap.rpc(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "laravel-parity", "version": "2"}}),
        )?;
        codemap.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        Ok(codemap)
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        writeln!(self.stdin, "{message}")
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("codemap: {e}"))
    }

    fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let mut line = String::new();
        loop {
            line.clear();
            if self
                .stdout
                .read_line(&mut line)
                .map_err(|e| e.to_string())?
                == 0
            {
                return Err(format!("codemap stopped while answering {method}"));
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(format!("codemap {method}: {error}"));
                }
                return Ok(message["result"].clone());
            }
        }
    }

    pub fn call(&mut self, tool: &str, arguments: Value) -> Result<Value, String> {
        let result = self.rpc("tools/call", json!({"name": tool, "arguments": arguments}))?;
        let text: String = result["content"]
            .as_array()
            .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect())
            .unwrap_or_default();
        serde_json::from_str(&text).map_err(|e| format!("codemap {tool}: {e}"))
    }

    /// Every route, following the pagination cursor.
    pub fn routes(&mut self) -> Result<Vec<Value>, String> {
        let mut routes = vec![];
        let mut cursor: Option<String> = None;
        loop {
            let mut args = json!({"limit": 500});
            if let Some(c) = &cursor {
                args["cursor"] = json!(c);
            }
            let answer = self.call("list_routes", args)?;
            if let Some(list) = answer["data"]["routes"].as_array() {
                routes.extend(list.iter().cloned());
            }
            cursor = answer["meta"]["next_cursor"].as_str().map(String::from);
            if cursor.is_none() {
                return Ok(routes);
            }
        }
    }

    /// The test files (repo-relative) that call this route.
    pub fn route_test_files(&mut self, method: &str, uri: &str) -> Result<Vec<String>, String> {
        let answer = self.call("find_usages", json!({"symbol": format!("{method} {uri}")}))?;
        let mut files: Vec<String> = answer["data"]["test_files"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if files.is_empty() {
            // older codemap answers list "file:line  call" only
            files = answer["data"]["tests"]
                .as_array()
                .map(|l| {
                    l.iter()
                        .filter_map(Value::as_str)
                        .map(|t| t.split(':').next().unwrap_or(t).to_string())
                        .collect()
                })
                .unwrap_or_default();
        }
        Ok(files)
    }
}

impl Drop for Codemap {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
