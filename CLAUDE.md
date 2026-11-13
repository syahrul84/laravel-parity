# laravel-parity

Rust CLI + MCP server (`src/`) that compares an old Laravel app with its upgraded copy; `php/replay.php` is the
runner each app's own PHP executes (embedded in the binary with `include_str!`). README.md explains use.

- Build: `cargo build --release`; before a commit: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` (CI runs the same). Windows check without a C toolchain:
  `CARGO_TARGET_DIR=target/wincheck cargo check --release --target x86_64-pc-windows-gnu`. Keep dependencies pure
  Rust (the mysql crate uses feature `minimal-rust`; `minimal` pulls C zlib and breaks Windows builds).
- `replay.php` must stay PHP 7.1 syntax (it runs on Laravel 5.5 / PHP 7.x too): no arrow functions, typed
  properties, match, `str_contains`, nullsafe. Every Laravel internal it reads goes in `parity_capabilities()`;
  `tests/fixtures/check-runner.php` is the CI check of the runner on each Laravel version (every mode: replay,
  console, console-run, channels, units, probe; artisan, routes and lint are used by setup-db, web and code).
- The runner serves many requests in one PHP process: `parity_reset_process()` puts back the ini settings, timezone,
  locale and encoding the process started with after each one (as PHP-FPM does). A request whose app fails to start
  after earlier ones succeeded exits 75 and runs again in a new process. If PHP ends mid-request (the app's `exit`,
  `dd()`, a fatal error), the shutdown handler records it as that request's answer, with `cause: "app"` when the code
  is the app's own; such rows are never saved as problem reports. `runner::crash_summary` keeps the error line, not
  the stack trace.
- Problem reports (`src/reports.rs`) are one per kind + step + normalised error line; routes, sides and versions are
  lists on that entry, and old ids stay as aliases so an issue already sent is found again.
- Resolve paths with `paths::real` (never `canonicalize()` directly): Windows gives `\\?\C:\x`, which PHP cannot open.
- `all` runs `check` first and stops when an app does not start or a guard's user is missing.
- `build.rs` stamps `--version` with the commit and date (`+changes` for a modified checkout).
- Each check writes its own `runs/<name>/<check>.json` and its `fixes()` turns it into fix-list items
  (`fixlist::build` collects them); `gate.rs` (CI) and `history.rs` (since last run, overview) read only those files.
- `php/ParityRecorder.php` is the middleware `laravel-parity recorder` hands out for the OLD app (same PHP 7.1 rule);
  `logs.rs` imports its jsonl as `rec-` cases. `learn.rs` runs after every `run` (`[run] learn` rounds): it fills the
  fields the old app's validation errors name in cases that should pass, and sends them again through `ops::send`.
- A validation refusal (`json.errors` with field names, even with status 500) is an answer, not a failure
  (`compare::rejected`); a happy / log- / rec- case both apps refuse is verdict `rejected`. `Spec.made` is the body
  the tool generated: a body that differs from it was typed by a person and every plan keeps it.
- `tests` measures coverage with the new app's own pcov/Xdebug (`src/coverage.rs`, `php/coverage.php`, same PHP 7.1
  rule): the app's own tests with `--exclude-group laravel-parity` (every generated method is in that group), cached
  by a fingerprint of app/, tests/, database/, routes/, config/; then the generated tests while they run anyway.
- Generated tests mirror app/ next to the app's own (`src/layout.rs`; `[tests] layout = "folder"` keeps one folder):
  feature tests per controller, unit tests per class from `units` (`src/units.rs`, replay.php modes units/probe: every
  public method of app/ classes run on both apps). No "Parity" in file, class or method names. A file the tool made
  has MARKER in its class docblock; in a test file someone wrote, the generated tests sit in a block
  (`src/testfile.rs`: `// <laravel-parity>` ... `// </laravel-parity>`) that each run replaces, and the coverage
  fingerprint and the "route has a test" check ignore. Every generated method carries the group, not the class.
  Every generated test calls `rollBackAfterGeneratedTest()` first (its own transaction, rolled back when it ends: no
  trait added to a class someone wrote), and a feature test sends the case's query string (`array $query` column).
- `src/factories.rs` writes a factory for each HasFactory model without one (Laravel 8+, MARKER, checked by making a
  row through probe mode on the new app; a database error fixes the column it names, else the factory is not kept).
- The AI loop: coverage_gaps -> write a test in group laravel-parity-ai, inside a `// <laravel-parity-ai>` block in
  test_file -> check_tests, with probe_code to ask both apps; target `[tests] target` (default 80).
- Summary lines count routes, test cases and methods apart and must add up; never sum different kinds.
- `src/ops.rs` holds every step and returns `Result`; never exit the process or print to stdout there (the MCP
  server uses stdout for the protocol). Progress goes through `crate::say`.
- `pairs/` and `runs/` are git-ignored except `pairs/example.toml`: real pair files name private hosts and databases.
- Releases: bump `version` in Cargo.toml, tag `vX.Y.Z`; the release workflow builds five targets and SHA256SUMS,
  which `laravel-parity update` and the install scripts check.
