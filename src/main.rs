//! laravel-parity: run the same requests through an old and an upgraded Laravel app and report what differs.

mod channels;
mod code;
mod codemap;
mod compare;
mod config;
mod console;
mod db;
mod fixlist;
mod gate;
mod glob;
mod guide;
mod history;
mod init;
mod learn;
mod log;
mod logs;
mod mcp;
mod ops;
mod plan;
mod report;
mod reports;
mod routes;
mod rules;
mod runner;
mod schema;
mod testgen;
mod update;
mod web;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

pub use crate::log::say;

#[derive(Parser)]
#[command(
    name = "laravel-parity",
    version,
    about = "Run the same requests through an old and an upgraded Laravel app and report what differs",
    after_help = "New here? Run `laravel-parity guide`."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The step-by-step walkthrough: from two repos to a report
    Guide,
    /// Write a first pair file for an old and a new repo (finds containers, database, guards)
    Init {
        old_repo: PathBuf,
        new_repo: PathBuf,
        /// Short name (default: the new repo's folder name)
        #[arg(long)]
        name: Option<String>,
        /// Where to write it (default: pairs/<name>.toml in the tool's folder)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Replace an existing pair file
        #[arg(long)]
        force: bool,
    },
    /// Run as an MCP server on stdio, for AI clients (Claude Code ...)
    Serve,
    /// Copy the old database into the two test databases and prepare the new one (migrate ...)
    SetupDb {
        pair: PathBuf,
        #[arg(short, long)]
        verbose: bool,
    },
    /// Show PHP, Laravel, MySQL and sign-in on both apps
    Check { pair: PathBuf },
    /// List both apps' routes with codemap and diff them
    Routes { pair: PathBuf },
    /// Turn the shared routes into requests, filling route parameters from the test data
    Plan { pair: PathBuf },
    /// Run every request on both apps and write the report
    Run {
        pair: PathBuf,
        /// Only requests whose "METHOD uri" contains this text; replaces just their earlier results
        #[arg(long)]
        only: Option<String>,
        /// Keep earlier results and run only what is missing
        #[arg(long)]
        resume: bool,
        /// PHP processes per app (default: [run] workers, else 4)
        #[arg(long)]
        workers: Option<usize>,
    },
    /// Problems in the upgraded code without running it: codemap doctor, upgrade traps, syntax, PHPStan
    Code { pair: PathBuf },
    /// Which of the web app's API calls the upgrade breaks (needs [web] in the pair file; Vue 2/3, Nuxt, React)
    Web { pair: PathBuf },
    /// The database: what the upgrade's migrations change, and what the migrations alone build on an empty database
    Schema { pair: PathBuf },
    /// The app's artisan commands, queued jobs and schedule, run and compared on both apps
    Console { pair: PathBuf },
    /// Who may join each broadcast channel (Echo / Reverb), the /broadcasting/auth route and the driver
    Channels { pair: PathBuf },
    /// Fill the fields the old app's validation errors name and send those requests again (run does this itself)
    Learn {
        pair: PathBuf,
        #[arg(long)]
        workers: Option<usize>,
    },
    /// Write a middleware that records real saves on the old app (test or staging), for import-log
    Recorder { pair: PathBuf },
    /// Add real requests as extra cases: an access log (successful GETs) or the recorder's file (saves); secrets dropped
    ImportLog {
        pair: PathBuf,
        log: PathBuf,
        /// At most this many different requests per route
        #[arg(long, default_value_t = 5)]
        per_route: usize,
    },
    /// Every pair file: where each upgrade stands, closest to release first
    Overview,
    /// Accept today's findings, so `ci` fails only on new ones
    Baseline {
        pair: PathBuf,
        /// Where to keep it (default: runs/<name>/baseline.json)
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// For CI: run the checks and fail (exit 1) on a new finding; writes junit.xml and a GitHub job summary
    Ci {
        pair: PathBuf,
        /// Fail on new findings of this priority or worse (1 fix before release ... 4 know about)
        #[arg(long, default_value_t = 1)]
        fail_on: u8,
        /// Judge the last results without running anything
        #[arg(long)]
        no_run: bool,
        /// The test databases are already prepared: skip setup-db
        #[arg(long)]
        no_setup: bool,
        /// The baseline file (default: runs/<name>/baseline.json)
        #[arg(long)]
        baseline: Option<PathBuf>,
    },
    /// Rewrite the report from the last run
    Report { pair: PathBuf },
    /// Generate PHPUnit tests for shared routes that have none
    Tests {
        pair: PathBuf,
        /// Do not run the generated tests
        #[arg(long)]
        no_verify: bool,
        /// Remove generated tests that fail
        #[arg(long)]
        drop_failing: bool,
    },
    /// Problems with the tool itself (reported by the AI or found by the tool); --send one as a GitHub issue
    Reports {
        /// Show what would be sent for this report and send it after you say yes
        #[arg(long)]
        send: Option<String>,
    },
    /// Install the latest release (checked against its SHA256SUMS)
    Update {
        /// Only say whether a newer version exists
        #[arg(long)]
        check: bool,
    },
    /// Everything: setup-db, routes, code, schema, web, plan, run, console, channels and tests
    All {
        pair: PathBuf,
        #[arg(long)]
        workers: Option<usize>,
    },
}

fn load(pair: &std::path::Path) -> config::Config {
    let config = config::load(pair).unwrap_or_else(|e| fail(e));
    let _ = std::fs::create_dir_all(&config.work);
    config
}

fn fail(text: impl std::fmt::Display) -> ! {
    eprintln!("error: {text}");
    std::process::exit(1);
}

fn main() {
    let cli = Cli::parse();
    let outcome: Result<(), String> = match cli.command {
        Command::Guide => {
            print!("{}", guide::GUIDE);
            Ok(())
        }
        Command::Serve => {
            mcp::serve();
            Ok(())
        }
        Command::Init {
            old_repo,
            new_repo,
            name,
            out,
            force,
        } => init::init(&old_repo, &new_repo, name.as_deref(), out.as_deref(), force).map(
            |(path, todo)| {
                say(&format!("Wrote {}", path.display()));
                if !todo.is_empty() {
                    say("Fill in before running:");
                    for item in todo {
                        say(&format!("  - {item}"));
                    }
                }
                say(&format!("Then: laravel-parity check {}", path.display()));
            },
        ),
        Command::SetupDb { pair, verbose } => ops::setup_db(&load(&pair), verbose).map(drop),
        Command::Check { pair } => ops::check(&load(&pair)).map(drop),
        Command::Routes { pair } => ops::routes(&load(&pair)).map(drop),
        Command::Plan { pair } => ops::plan(&load(&pair)).map(drop),
        Command::Run {
            pair,
            only,
            resume,
            workers,
        } => ops::run(&load(&pair), only.as_deref(), resume, workers).map(drop),
        Command::Code { pair } => code::check(&load(&pair)).map(drop),
        Command::Web { pair } => web::check(&load(&pair)).map(drop),
        Command::Report { pair } => ops::report(&load(&pair)).map(drop),
        Command::Schema { pair } => schema::check(&load(&pair)).map(drop),
        Command::Console { pair } => console::check(&load(&pair)).map(drop),
        Command::Channels { pair } => channels::check(&load(&pair)).map(drop),
        Command::ImportLog {
            pair,
            log,
            per_route,
        } => logs::import(&load(&pair), &log, per_route).map(drop),
        Command::Learn { pair, workers } => {
            let config = load(&pair);
            learn::improve(&config, workers)
                .and_then(|line| {
                    say(&line);
                    ops::report(&config)
                })
                .map(drop)
        }
        Command::Recorder { pair } => logs::recorder(&load(&pair)).map(drop),
        Command::Overview => history::overview().map(drop),
        Command::Baseline { pair, file } => {
            gate::baseline(&load(&pair), file.as_deref()).map(|line| say(&line))
        }
        Command::Ci {
            pair,
            fail_on,
            no_run,
            no_setup,
            baseline,
        } => gate::ci(
            &load(&pair),
            no_run,
            !no_setup,
            fail_on,
            baseline.as_deref(),
        )
        .map(drop),
        Command::Tests {
            pair,
            no_verify,
            drop_failing,
        } => ops::tests(&load(&pair), !no_verify, drop_failing).map(|line| say(&line)),
        Command::Reports { send: Some(id) } => reports::send(&id).map(|line| say(&line)),
        Command::Reports { send: None } => {
            let mut all = reports::load();
            all.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
            if all.is_empty() {
                say("No problems reported.");
            }
            for r in &all {
                let sent = r
                    .sent
                    .as_deref()
                    .map(|s| format!(" [sent: {s}]"))
                    .unwrap_or_default();
                say(&format!(
                    "{}  {:<18} {}x  {}  ({}, {}){sent}",
                    r.id,
                    r.kind,
                    r.count,
                    r.summary.chars().take(110).collect::<String>(),
                    r.source,
                    r.last_seen
                ));
                if let Some(route) = &r.route {
                    say(&format!("            {route}"));
                }
            }
            if !all.is_empty() {
                say(
                    "\nSend one to the tool's GitHub repo (you see it first): laravel-parity reports --send <id>",
                );
            }
            Ok(())
        }
        Command::Update { check } => update::run(check).map(|line| say(&line)),
        Command::All { pair, workers } => ops::all(&load(&pair), workers).map(|line| say(&line)),
    };
    if let Err(e) = outcome {
        fail(e);
    }
}
