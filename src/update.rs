//! `laravel-parity update`: replaces this program with the latest GitHub release, after checking it against the
//! release's SHA256SUMS. Also the once-a-day check behind the MCP server's "new version" notice. Uses the system's
//! `curl` and `tar`, which macOS, Linux and Windows 10 and later have.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::reports::REPO;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The version with the commit and date it was built from (build.rs): two builds of one version differ here.
pub const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("LARAVEL_PARITY_BUILD"),
    ")"
);

/// The release file for this machine.
fn asset() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("laravel-parity-aarch64-apple-darwin.tar.gz")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("laravel-parity-x86_64-apple-darwin.tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("laravel-parity-x86_64-unknown-linux-gnu.tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("laravel-parity-aarch64-unknown-linux-gnu.tar.gz")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("laravel-parity-x86_64-pc-windows-msvc.zip")
    } else {
        None
    }
}

/// A system tool: on Windows the one in System32 (its tar.exe reads .zip; a GNU tar from Git for Windows does not).
fn tool(name: &str) -> PathBuf {
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        let p = PathBuf::from(root)
            .join("System32")
            .join(format!("{name}.exe"));
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from(name)
}

fn curl(url: &str, timeout_s: u32) -> Result<Vec<u8>, String> {
    let out = Command::new(tool("curl"))
        .args(["-fsSL", "--max-time", &timeout_s.to_string()])
        .args(["-H", "Accept: application/vnd.github+json"])
        .args(["-H", &format!("User-Agent: laravel-parity/{VERSION}")])
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "`curl` is needed to download updates and was not found".to_string())?;
    if !out.status.success() {
        return Err(format!(
            "downloading {url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// The latest release's tag, e.g. `v0.1.0`.
pub fn latest(timeout_s: u32) -> Result<String, String> {
    let body = curl(
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        timeout_s,
    )?;
    let v: Value =
        serde_json::from_slice(&body).map_err(|_| "unexpected reply from GitHub".to_string())?;
    v["tag_name"]
        .as_str()
        .map(str::to_owned)
        .ok_or("GitHub's reply has no release tag".into())
}

fn parts(version: &str) -> Vec<u64> {
    version
        .trim_start_matches('v')
        .split(['.', '-'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

pub fn is_newer(tag: &str, current: &str) -> bool {
    parts(tag) > parts(current)
}

fn cache() -> PathBuf {
    crate::init::tool_root()
        .join("runs")
        .join("update-check.json")
}

fn disabled() -> bool {
    std::env::var_os("LARAVEL_PARITY_NO_UPDATE_CHECK").is_some()
}

/// The newer version a check found (at most a day old), if any.
pub fn known_newer() -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(cache()).ok()?).ok()?;
    let tag = v["latest"].as_str()?;
    is_newer(tag, VERSION).then(|| tag.trim_start_matches('v').to_string())
}

/// Ask GitHub when the last check is a day old, without delaying the caller; an offline machine is not asked
/// again for a day.
pub fn check_in_background() {
    if disabled() {
        return;
    }
    let due = std::fs::read_to_string(cache())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["checked"].as_i64())
        .is_none_or(|t| chrono::Utc::now().timestamp() - t > 86_400);
    if !due {
        return;
    }
    std::thread::spawn(|| {
        let previous = std::fs::read_to_string(cache())
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let latest = latest(10)
            .ok()
            .or_else(|| previous.and_then(|p| p["latest"].as_str().map(String::from)));
        let _ = std::fs::create_dir_all(cache().parent().unwrap());
        let _ = std::fs::write(
            cache(),
            json!({"checked": chrono::Utc::now().timestamp(), "latest": latest}).to_string(),
        );
    });
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `C:\x` for `\\?\C:\x` (what canonicalize gives on Windows).
fn shown(p: &Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(s)
}

/// The previous program, kept beside the new one on Windows (a running program can be renamed there, not replaced).
fn old_copy(exe: &Path) -> PathBuf {
    exe.with_extension("old.exe")
}

/// Removes the copy a previous update left (Windows). Fails quietly while that copy is still running.
pub fn clean_old_copy() {
    if cfg!(windows)
        && let Ok(exe) = std::env::current_exe()
    {
        let _ = std::fs::remove_file(old_copy(&exe));
    }
}

/// Puts `new` where `exe` is: on Unix a rename over the running program (it keeps running until it exits); on
/// Windows the running program is moved aside first.
fn replace_exe(new: &Path, exe: &Path) -> std::io::Result<()> {
    let staged = exe.with_extension("new");
    let result = (|| {
        std::fs::copy(new, &staged)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        if cfg!(windows) {
            let old = old_copy(exe);
            let _ = std::fs::remove_file(&old);
            std::fs::rename(exe, &old)?;
            if let Err(e) = std::fs::rename(&staged, exe) {
                // put the working program back
                let _ = std::fs::rename(&old, exe);
                return Err(e);
            }
            Ok(())
        } else {
            std::fs::rename(&staged, exe)
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

/// The first `laravel-parity` on PATH, when it is not this program: running `laravel-parity` starts that one.
fn shadowed_by(exe: &Path, path: &std::ffi::OsStr) -> Option<PathBuf> {
    let name = format!("laravel-parity{}", std::env::consts::EXE_SUFFIX);
    let first = std::env::split_paths(path)
        .map(|dir| dir.join(&name))
        .find(|p| p.is_file())?;
    let first = first.canonicalize().unwrap_or(first);
    let exe = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    (first != exe).then_some(first)
}

/// A warning when another laravel-parity earlier on PATH runs instead of `exe`, with the version it says.
fn path_warning(exe: &Path) -> String {
    let Some(other) = std::env::var_os("PATH").and_then(|path| shadowed_by(exe, &path)) else {
        return String::new();
    };
    let said = Command::new(&other)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let fix = if other.components().any(|c| c.as_os_str() == ".cargo") {
        "remove it with `cargo uninstall laravel-parity`".to_string()
    } else {
        "remove it".to_string()
    };
    format!(
        "\nWarning: `laravel-parity` on PATH starts {} ({}), not {}. {fix}, or put {} earlier on PATH.",
        shown(&other),
        if said.is_empty() {
            "version unknown"
        } else {
            &said
        },
        shown(exe),
        exe.parent().map(shown).unwrap_or_default()
    )
}

pub fn run(check_only: bool) -> Result<String, String> {
    clean_old_copy();
    let here = std::env::current_exe()
        .and_then(|e| e.canonicalize())
        .map_err(|e| e.to_string())?;
    let tag = latest(20)?;
    let _ = std::fs::create_dir_all(cache().parent().unwrap());
    let _ = std::fs::write(
        cache(),
        json!({"checked": chrono::Utc::now().timestamp(), "latest": tag}).to_string(),
    );
    let new = tag.trim_start_matches('v').to_string();
    if !is_newer(&tag, VERSION) {
        return Ok(format!(
            "laravel-parity {LONG_VERSION} is the latest version.{}",
            path_warning(&here)
        ));
    }
    let notes = format!("Release notes: https://github.com/{REPO}/releases/tag/{tag}");
    if check_only {
        return Ok(format!(
            "laravel-parity {new} is available (this is {LONG_VERSION}). {notes}\nRun `laravel-parity update` to install it.{}",
            path_warning(&here)
        ));
    }
    let Some(asset) = asset() else {
        return Err(format!(
            "there is no ready-made laravel-parity for this system; build it with `cargo install --locked --git https://github.com/{REPO} --tag {tag}`"
        ));
    };
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let archive = curl(&format!("{base}/{asset}"), 300)?;
    let sums =
        String::from_utf8(curl(&format!("{base}/SHA256SUMS"), 30)?).map_err(|e| e.to_string())?;
    let expected = sums
        .lines()
        .find_map(|l| l.split_once("  ").filter(|(_, f)| f.trim() == asset))
        .map(|(h, _)| h.trim().to_owned())
        .ok_or(format!("SHA256SUMS has no line for {asset}"))?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        return Err(format!(
            "the download does not match its checksum ({actual} != {expected}); nothing was changed"
        ));
    }

    let work = std::env::temp_dir().join(format!("laravel-parity-update-{}", std::process::id()));
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let tarball = work.join(asset);
    std::fs::write(&tarball, &archive).map_err(|e| e.to_string())?;
    let ok = Command::new(tool("tar"))
        .arg("-xf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work)
        .status()
        .map_err(|_| "`tar` is needed to unpack the update and was not found".to_string())?
        .success();
    let unpacked = work.join(format!("laravel-parity{}", std::env::consts::EXE_SUFFIX));
    if !ok || !unpacked.is_file() {
        return Err(format!("could not unpack {asset}"));
    }
    let said = Command::new(&unpacked)
        .arg("--version")
        .output()
        .map_err(|e| e.to_string())?;
    let said = String::from_utf8_lossy(&said.stdout).to_string();
    if !said.contains(&new) {
        return Err(format!(
            "the downloaded program reports `{}`, expected {new}; nothing was changed",
            said.trim()
        ));
    }
    let exe = here;
    let result = replace_exe(&unpacked, &exe);
    let _ = std::fs::remove_dir_all(&work);
    if let Err(e) = result {
        let advice = if cfg!(windows) {
            "run the terminal as administrator"
        } else {
            "run it with sudo"
        };
        return Err(format!(
            "cannot replace {}: {e} ({advice}, or install into a folder you own)",
            shown(&exe)
        ));
    }
    let kept = if cfg!(windows) {
        format!(
            "\nThe previous program is kept as {} until laravel-parity next starts, which removes it.",
            shown(&old_copy(&exe))
        )
    } else {
        String::new()
    };
    Ok(format!(
        "Updated {} to {new}. {notes}\nAI sessions use it from their next start (or restart the MCP server).{kept}{}",
        shown(&exe),
        path_warning(&exe)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_another_copy_first_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let name = format!("laravel-parity{}", std::env::consts::EXE_SUFFIX);
        let (cargo, mine, empty) = (root.join("cargo-bin"), root.join("bin"), root.join("other"));
        for d in [&cargo, &mine, &empty] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(cargo.join(&name), "old").unwrap();
        std::fs::write(mine.join(&name), "new").unwrap();
        let exe = mine.join(&name);
        let path = |dirs: &[&PathBuf]| std::env::join_paths(dirs.iter()).unwrap();
        assert_eq!(
            shadowed_by(&exe, &path(&[&empty, &cargo, &mine])),
            Some(cargo.join(&name))
        );
        assert_eq!(shadowed_by(&exe, &path(&[&empty, &mine, &cargo])), None);
        assert_eq!(shadowed_by(&exe, &path(&[&empty])), None);
    }

    #[test]
    fn replaces_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir
            .path()
            .join(format!("laravel-parity{}", std::env::consts::EXE_SUFFIX));
        let new = dir.path().join("download.bin");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        replace_exe(&new, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!exe.with_extension("new").exists());
    }

    #[test]
    fn versions() {
        assert!(is_newer("v0.3.0", "0.2.9"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
