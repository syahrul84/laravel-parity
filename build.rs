// Stamps the build with the commit it was made from, so two builds of the same version can be told apart
// (`laravel-parity --version`: "0.2.5 (a329bc7 2026-11-02)", "+changes" when built from a modified checkout).

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    for path in [
        ".git/HEAD",
        ".git/index",
        "src",
        "php",
        "Cargo.toml",
        "build.rs",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let stamp = match (
        git(&["rev-parse", "--short=7", "HEAD"]),
        git(&["log", "-1", "--format=%cs"]),
    ) {
        (Some(commit), Some(date)) => {
            // tracked files changed since that commit: not exactly that commit's program
            let changed = Command::new("git")
                .args([
                    "diff",
                    "--quiet",
                    "HEAD",
                    "--",
                    "src",
                    "php",
                    "Cargo.toml",
                    "Cargo.lock",
                    "build.rs",
                ])
                .status()
                .is_ok_and(|s| s.code() == Some(1));
            format!("{commit}{} {date}", if changed { "+changes" } else { "" })
        }
        // a build without git (a source archive): say so rather than pretend
        _ => "build without git".to_string(),
    };
    println!("cargo:rustc-env=LARAVEL_PARITY_BUILD={stamp}");
}
