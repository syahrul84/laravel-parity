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
  console, console-run, channels; artisan, routes and lint are used by setup-db, web and code).
- Each check writes its own `runs/<name>/<check>.json` and its `fixes()` turns it into fix-list items
  (`fixlist::build` collects them); `gate.rs` (CI) and `history.rs` (since last run, overview) read only those files.
- `src/ops.rs` holds every step and returns `Result`; never exit the process or print to stdout there (the MCP
  server uses stdout for the protocol). Progress goes through `crate::say`.
- `pairs/` and `runs/` are git-ignored except `pairs/example.toml`: real pair files name private hosts and databases.
- Releases: bump `version` in Cargo.toml, tag `vX.Y.Z`; the release workflow builds five targets and SHA256SUMS,
  which `laravel-parity update` and the install scripts check.
