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
    /// every route, side (old / new app) and tool version the same problem was seen on
    #[serde(default)]
    pub routes: Vec<String>,
    #[serde(default)]
    pub sides: Vec<String>,
    #[serde(default)]
    pub versions: Vec<String>,
    /// ids this report had under an earlier fingerprint (an issue sent then names one of them)
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// Kept per report: enough to see how widespread a problem is without growing the file.
const MAX_ROUTES: usize = 50;

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

/// "old app: X" -> ("old", "X"); the side is where it was seen, not part of the problem.
fn side_of(summary: &str) -> (Option<&'static str>, &str) {
    for side in ["old", "new"] {
        if let Some(rest) = summary.strip_prefix(&format!("{side} app: ")) {
            return (Some(if side == "old" { "old" } else { "new" }), rest);
        }
    }
    (None, summary)
}

/// The error line a report is about, the same however it was cut and wherever it happened: the first line, without
/// the side, paths and hashes as placeholders, a quote the cut left open dropped, numbers as #.
fn error_key(summary: &str) -> String {
    static OPEN_QUOTE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"('[^']*|"[^"]*)$"#).unwrap());
    static HASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9a-f]{8,}\b").unwrap());
    static NUMBERS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
    let (_, rest) = side_of(summary.trim());
    let rest = rest
        .strip_prefix("PHP stopped on this request: ")
        .unwrap_or(rest);
    let line = rest.lines().next().unwrap_or(rest);
    let line = scrub(line);
    let line = HASH.replace_all(&line, "<hash>");
    let line: String = line.chars().take(160).collect();
    let line = OPEN_QUOTE.replace(&line, "");
    NUMBERS.replace_all(line.trim(), "#").into_owned()
}

fn fingerprint(kind: &str, step: &str, summary: &str) -> String {
    // the same problem on another route, side or version, or cut at another point, is the same report
    crate::plan::request_id(&format!("{kind}|{step}|{}", error_key(summary)))
}

fn push_unique(list: &mut Vec<String>, value: &str, max: usize) {
    if !value.is_empty() && !list.iter().any(|v| v == value) && list.len() < max {
        list.push(value.to_string());
    }
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
    let (side, rest) = side_of(new.summary);
    let summary = scrub(rest);
    let id = format!("p_{}", &fingerprint(new.kind, new.step, &summary)[..8]);
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    let version = crate::update::LONG_VERSION;
    let mut all = load_unlocked();
    if let Some(existing) = all.iter_mut().find(|r| r.id == id) {
        existing.count += 1;
        existing.last_seen = now;
        push_unique(
            &mut existing.routes,
            route.as_deref().unwrap_or(""),
            MAX_ROUTES,
        );
        push_unique(&mut existing.sides, side.unwrap_or(""), 2);
        push_unique(&mut existing.versions, version, 20);
        for (side, version) in new.laravel {
            existing.laravel.entry(side).or_insert(version);
        }
    } else {
        all.push(Report {
            id: id.clone(),
            kind: new.kind.into(),
            source: new.source.into(),
            step: new.step.into(),
            route: route.clone(),
            summary,
            expected: new.expected.map(scrub),
            actual: new.actual.map(scrub),
            tool_version: crate::update::LONG_VERSION.into(),
            laravel: new.laravel,
            os: std::env::consts::OS.into(),
            count: 1,
            first_seen: now.clone(),
            last_seen: now,
            sent: None,
            routes: route.into_iter().collect(),
            sides: side.map(String::from).into_iter().collect(),
            versions: vec![version.into()],
            aliases: vec![],
        });
    }
    save_unlocked(&all);
    id
}

fn load_unlocked() -> Vec<Report> {
    merge(
        std::fs::read_to_string(path())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect(),
    )
}

/// Reports saved under an earlier fingerprint (one per route, side and version) folded into one per problem.
fn merge(reports: Vec<Report>) -> Vec<Report> {
    let mut out: Vec<Report> = vec![];
    for mut r in reports {
        let id = format!("p_{}", &fingerprint(&r.kind, &r.step, &r.summary)[..8]);
        if let Some(route) = r.route.clone() {
            push_unique(&mut r.routes, &route, MAX_ROUTES);
        }
        let (side, rest) = side_of(&r.summary);
        let rest = rest.to_string();
        push_unique(&mut r.sides, side.unwrap_or(""), 2);
        let version = r.tool_version.clone();
        push_unique(&mut r.versions, &version, 20);
        if r.id != id {
            let old = std::mem::replace(&mut r.id, id.clone());
            push_unique(&mut r.aliases, &old, 100);
            r.summary = rest;
        }
        match out.iter_mut().find(|o| o.id == id) {
            None => out.push(r),
            Some(o) => {
                o.count += r.count;
                o.first_seen = o.first_seen.clone().min(r.first_seen);
                o.last_seen = o.last_seen.clone().max(r.last_seen);
                o.sent = o.sent.take().or(r.sent);
                for (list, more, max) in [
                    (&mut o.routes, r.routes, MAX_ROUTES),
                    (&mut o.sides, r.sides, 2),
                    (&mut o.versions, r.versions, 20),
                    (&mut o.aliases, r.aliases, 100),
                ] {
                    for v in more {
                        push_unique(list, &v, max);
                    }
                }
                for (side, version) in r.laravel {
                    o.laravel.entry(side).or_insert(version);
                }
            }
        }
    }
    out
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
    if report.routes.len() > 1 {
        let shown: Vec<String> = report
            .routes
            .iter()
            .take(10)
            .map(|r| format!("`{r}`"))
            .collect();
        body += &format!(
            "**Routes:** {} ({}{})\n",
            report.routes.len(),
            shown.join(", "),
            if report.routes.len() > 10 {
                ", …"
            } else {
                ""
            }
        );
    } else if let Some(route) = report.routes.first().or(report.route.as_ref()) {
        body += &format!("**Route:** `{route}`\n");
    }
    if !report.sides.is_empty() {
        body += &format!("**Apps:** {}\n", report.sides.join(", "));
    }
    if report.versions.len() > 1 {
        body += &format!(
            "**laravel-parity versions:** {}\n",
            report.versions.join(", ")
        );
    }
    body += &format!("\n{}\n", report.summary);
    if let Some(e) = &report.expected {
        body += &format!("\n**Expected:** {e}\n");
    }
    if let Some(a) = &report.actual {
        body += &format!("**Actual:** {a}\n");
    }
    body += &format!("\n<!-- laravel-parity-report: {} -->\n", report.id);
    for alias in &report.aliases {
        body += &format!("<!-- laravel-parity-report: {alias} -->\n");
    }
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
    // the open issue for this problem: sent under its id, or under an id it had before reports were merged
    let mut number = String::new();
    for id in std::iter::once(&report.id).chain(report.aliases.iter()) {
        let marker = format!("laravel-parity-report: {id}");
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
        number = String::from_utf8_lossy(&existing.stdout).trim().to_string();
        if !number.is_empty() {
            break;
        }
    }
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

    fn report(id: &str, route: &str, summary: &str, version: &str, count: u64) -> Report {
        Report {
            id: id.into(),
            kind: "runner-crash".into(),
            source: "auto".into(),
            step: "run".into(),
            route: Some(route.into()),
            summary: summary.into(),
            expected: None,
            actual: None,
            tool_version: version.into(),
            laravel: BTreeMap::new(),
            os: "macos".into(),
            count,
            first_seen: "2026-11-01 10:00".into(),
            last_seen: "2026-11-02 10:00".into(),
            sent: None,
            routes: vec![],
            sides: vec![],
            versions: vec![],
            aliases: vec![],
        }
    }

    #[test]
    fn one_report_per_problem() {
        let env = "The environment file is invalid! Failed to parse dotenv file. Encountered an invalid name at [APP_NAME].";
        // the same error on two routes, both sides, two versions, and cut at different points
        let merged = merge(vec![
            report("p_a", "POST api/a", &format!("old app: {env}"), "0.2.0", 3),
            report("p_b", "POST api/b", &format!("new app: {env}"), "0.2.5", 2),
            report(
                "p_c",
                "GET api/c",
                "old app: Unknown column 'user_id' in 'where clause' (SQL: select * from 'units where user_id",
                "0.2.5",
                1,
            ),
            report(
                "p_d",
                "GET api/d",
                "new app: Unknown column 'user_id' in 'where clause' (SQL: select * from 'units where us",
                "0.2.5",
                1,
            ),
        ]);
        assert_eq!(merged.len(), 2);
        let first = &merged[0];
        assert_eq!(first.count, 5);
        assert_eq!(first.routes, ["POST api/a", "POST api/b"]);
        assert_eq!(first.sides, ["old", "new"]);
        assert_eq!(first.versions, ["0.2.0", "0.2.5"]);
        assert_eq!(first.aliases, ["p_a", "p_b"]);
        assert_eq!(first.summary, env);
        assert_eq!(merged[1].count, 2);
        // merging again changes nothing
        let again = merge(merged.clone());
        assert_eq!(again.len(), 2);
        assert_eq!(again[0].count, 5);
        assert_eq!(again[0].aliases, ["p_a", "p_b"]);
        let (_, body) = issue(&again[0]);
        assert!(body.contains("**Routes:** 2 (`POST api/a`, `POST api/b`)"));
        assert!(body.contains("**Apps:** old, new"));
        assert!(body.contains("<!-- laravel-parity-report: p_a -->"));
        assert_eq!(
            error_key(
                "new app: PHP stopped on this request: PHP Fatal error: x in /a/b/c.php on line 12\n#0 trace"
            ),
            "PHP Fatal error: x in …/c.php on line #"
        );
    }
}
