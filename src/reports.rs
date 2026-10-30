//! Problems with the tool itself, kept on this machine so the tool can be improved: reported by the AI through the
//! MCP tool `report_problem`, or found by the tool (a runner crash, a missing Laravel internal, a panic). Nothing
//! leaves the machine unless a person sends one (`laravel-parity reports --send <id>`) and says yes.
//!
//! What may be sent is scrubbed: no response bodies, ids, hosts, paths or user names; only the tool and Laravel
//! versions, the step, the route pattern and the error with its details replaced by placeholders.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::init::tool_root;

pub const REPO: &str = "syahrul84/laravel-parity";
const MAX_BYTES: u64 = 2 * 1024 * 1024;
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub id: String,
    /// wrong-verdict, confusing-answer, missing-feature, fix-did-not-work, runner-crash, missing-capability, panic ...
    pub kind: String,
    /// "ai" or "auto"
    pub source: String,
    pub step: String,
    /// A route pattern (GET api/admin/units/{unit}), never a concrete URL.
    #[serde(default)]
    pub route: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub expected: Option<String>,
    #[serde(default)]
    pub actual: Option<String>,
    pub tool_version: String,
    #[serde(default)]
    pub laravel: BTreeMap<String, String>,
    pub os: String,
    pub count: u64,
    pub first_seen: String,
    pub last_seen: String,
    #[serde(default)]
    pub sent: Option<String>,
}

fn path() -> PathBuf {
    tool_root().join("runs").join("reports.jsonl")
}

/// Paths, hosts, emails, long numbers and quoted values reduced to placeholders.
pub fn scrub(text: &str) -> String {
    static RULES: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
        [
            (r"[\w.+-]+@[\w-]+(\.[\w-]+)+", "<email>"),
            (r"https?://[^\s'\x22)]+", "<url>"),
            (r"\b\d{1,3}(\.\d{1,3}){3}(:\d+)?\b", "<ip>"),
            (r"(/[\w.@-]+){2,}/([\w.-]+\.php)", "…/$2"),
            (r"([A-Za-z]:)?(\\[\w.@ -]+){2,}\\([\w.-]+)", "…\\$3"),
            (r"(/[\w.@-]+){2,}", "<path>"),
            (r#"'[^']{24,}'|"[^"]{24,}""#, "'<text>'"),
            (r"\b\d{3,}\b", "<n>"),
        ]
        .into_iter()
        .map(|(p, r)| (Regex::new(p).unwrap(), r))
        .collect()
    });
    let mut out = text.chars().take(600).collect::<String>();
    for (pattern, replacement) in RULES.iter() {
        out = pattern.replace_all(&out, *replacement).into_owned();
    }
    out
}

/// A route pattern from a "METHOD uri" key; concrete URLs (ids filled in) are refused.
fn pattern(route: Option<&str>) -> Option<String> {
    let route = route?.trim();
    static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/\d+(/|$)").unwrap());
    if DIGITS.is_match(route) {
        return Some(DIGITS.replace_all(route, "/{id}$1").into_owned());
    }
    Some(route.chars().take(200).collect())
}

fn fingerprint(kind: &str, step: &str, route: Option<&str>, summary: &str) -> String {
    // the same problem on another route or with other numbers is the same report
    static NUMBERS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
    let base = format!(
        "{kind}|{step}|{}|{}",
        route.unwrap_or(""),
        NUMBERS.replace_all(summary, "#")
    );
    crate::plan::request_id(&base)
}

pub struct New<'a> {
    pub kind: &'a str,
    pub source: &'a str,
    pub step: &'a str,
    pub route: Option<&'a str>,
    pub summary: &'a str,
    pub expected: Option<&'a str>,
    pub actual: Option<&'a str>,
    pub laravel: BTreeMap<String, String>,
}

/// Add a report, or count it again when the same problem was reported before. Returns its id.
pub fn add(new: New) -> String {
    let _guard = LOCK.lock().unwrap();
    let route = pattern(new.route);
    let summary = scrub(new.summary);
    let id = format!(
        "p_{}",
        &fingerprint(new.kind, new.step, route.as_deref(), &summary)[..8]
    );
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    let mut all = load_unlocked();
    if let Some(existing) = all.iter_mut().find(|r| r.id == id) {
        existing.count += 1;
        existing.last_seen = now;
    } else {
        all.push(Report {
            id: id.clone(),
            kind: new.kind.into(),
            source: new.source.into(),
            step: new.step.into(),
            route,
            summary,
            expected: new.expected.map(scrub),
            actual: new.actual.map(scrub),
            tool_version: env!("CARGO_PKG_VERSION").into(),
            laravel: new.laravel,
            os: std::env::consts::OS.into(),
            count: 1,
            first_seen: now.clone(),
            last_seen: now,
            sent: None,
        });
    }
    save_unlocked(&all);
    id
}

fn load_unlocked() -> Vec<Report> {
    std::fs::read_to_string(path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn save_unlocked(all: &[Report]) {
    let mut lines: Vec<String> = all
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    // keep the file small: drop the oldest until it fits
    while lines.iter().map(|l| l.len() as u64 + 1).sum::<u64>() > MAX_BYTES && lines.len() > 1 {
        lines.remove(0);
    }
    let file = path();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, lines.join("\n") + "\n");
}

pub fn load() -> Vec<Report> {
    let _guard = LOCK.lock().unwrap();
    load_unlocked()
}

/// What would be sent for a report, as the issue title and body.
pub fn issue(report: &Report) -> (String, String) {
    let title = format!(
        "[{}] {}",
        report.kind,
        report.summary.chars().take(90).collect::<String>()
    );
    let laravel: Vec<String> = report
        .laravel
        .iter()
        .map(|(side, v)| format!("{side}: Laravel {v}"))
        .collect();
    let mut body = format!(
        "**Kind:** {}\n**Found by:** {}\n**Step:** {}\n**Seen:** {} times ({} to {})\n**laravel-parity:** {} on {}\n",
        report.kind,
        report.source,
        report.step,
        report.count,
        report.first_seen,
        report.last_seen,
        report.tool_version,
        report.os
    );
    if !laravel.is_empty() {
        body += &format!("**Apps:** {}\n", laravel.join(", "));
    }
    if let Some(route) = &report.route {
        body += &format!("**Route:** `{route}`\n");
    }
    body += &format!("\n{}\n", report.summary);
    if let Some(e) = &report.expected {
        body += &format!("\n**Expected:** {e}\n");
    }
    if let Some(a) = &report.actual {
        body += &format!("**Actual:** {a}\n");
    }
    body += &format!("\n<!-- laravel-parity-report: {} -->\n", report.id);
    (title, body)
}

fn mark_sent(id: &str, where_: &str) {
    let _guard = LOCK.lock().unwrap();
    let mut all = load_unlocked();
    if let Some(r) = all.iter_mut().find(|r| r.id == id) {
        r.sent = Some(where_.into());
    }
    save_unlocked(&all);
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Send one report as a GitHub issue after the person says yes: comment on the open issue with the same
/// fingerprint if there is one, else open a new issue; without a signed-in `gh`, print a new-issue link.
pub fn send(id: &str) -> Result<String, String> {
    let report = load().into_iter().find(|r| r.id == id).ok_or(format!(
        "no report {id} (laravel-parity reports lists them)"
    ))?;
    let (title, body) = issue(&report);
    println!("This is exactly what would be sent to github.com/{REPO}:\n\n# {title}\n\n{body}");
    print!("Send it? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        return Ok("Not sent.".into());
    }
    let gh_ok = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !gh_ok {
        let link = format!(
            "https://github.com/{REPO}/issues/new?title={}&body={}",
            encode(&title),
            encode(&body)
        );
        return Ok(format!(
            "gh is not signed in; open this link to file it:\n{link}"
        ));
    }
    let marker = format!("laravel-parity-report: {}", report.id);
    let existing = Command::new("gh")
        .args([
            "issue",
            "list",
            "-R",
            REPO,
            "--state",
            "open",
            "--search",
            &marker,
            "--json",
            "number",
            "-q",
            ".[0].number",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    let number = String::from_utf8_lossy(&existing.stdout).trim().to_string();
    let out = if number.is_empty() {
        Command::new("gh")
            .args([
                "issue", "create", "-R", REPO, "--title", &title, "--body", &body,
            ])
            .output()
    } else {
        Command::new("gh")
            .args(["issue", "comment", &number, "-R", REPO, "--body", &body])
            .output()
    }
    .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let link = String::from_utf8_lossy(&out.stdout).trim().to_string();
    mark_sent(id, &link);
    Ok(format!("Sent: {link}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbing() {
        let s = scrub(
            "SQLSTATE: Unknown column 'unit_shop_name' in /opt/localhost/api/app/Http/Foo.php line 1234 for bob@x.com at http://10.0.0.1:8080/x",
        );
        assert!(s.contains("…/Foo.php"), "{s}");
        assert!(
            s.contains("<n>") && s.contains("<email>") && s.contains("<url>"),
            "{s}"
        );
        assert_eq!(
            pattern(Some("GET api/units/42/edit")).unwrap(),
            "GET api/units/{id}/edit"
        );
    }
}
