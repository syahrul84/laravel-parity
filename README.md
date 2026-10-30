# laravel-parity

**You upgraded a Laravel app. Does it still work the way it did?**

laravel-parity is for any Laravel upgrade: Laravel 5.x, 6, 7, 8, 9 or 10 to the latest, a new PHP version, a new
MySQL, done by hand or with [Laravel Shift](https://laravelshift.com). After the upgrade you have two copies of the
app: the **old code** (still the way it ran in production) and the **upgraded code**. laravel-parity treats the old
code as the truth and checks that the upgraded code behaves the same:

1. it runs both apps side by side, on copies of the old database, and sends every route the same requests (signed in,
   not signed in, real and missing records, valid and invalid input)
2. it compares what each app answered and did (status, JSON, rows written, jobs, mail, notifications, broadcasts)
3. it checks the upgraded code without running it, for the problems upgrades typically leave behind (methods that no
   longer exist, date fields that lost their casting, Carbon 3, removed helpers, PHP 8.x changes)
4. it reads your web app, if there is one (Vue 2, Vue 3, Nuxt), for the screens whose API calls the upgrade breaks

What you get is **a list of what to fix before you release**, worst first, each with the file and line and the usual
cause; a report of every difference; and **PHPUnit tests** for the routes that had none, so the new code stays the way
the old one behaved. An AI assistant can do the whole loop through the built-in MCP server: read the list, fix the
code, re-check the route, and add test scenarios of its own.

One binary for macOS, Linux and Windows. Each app runs in its Docker container or with a PHP on the machine, so
Docker is optional. Nothing is written to your real database. Built on
[codemap](https://github.com/syahrul84/codemap), which reads the routes, controllers, validation rules and web calls.

## What you get

- **`fixes.md`**: what to fix, worst first, as a checklist:
  1. *Fix before release*: routes that crash on the new app (grouped by cause, with the file:line and the usual
     reason, e.g. "a return type added by Laravel Shift does not match"), routes now open without signing in,
     web app calls to routes or methods that no longer exist
  2. *Fix soon*: access the user lost, validation rules lost or now stricter, response fields the web app reads
     that are gone
  3. *Check*: lost response fields, changed status codes, different rows written / jobs / mail / broadcasts
  4. *Know about*: what could not be checked, and why
  Plus a **Code** area, found without running anything: routes pointing to controller methods that no longer
  exist (codemap `doctor`, new vs already there), upgrade traps picked by your version jump (models whose `$dates`
  fields lost their casting in Laravel 10, Carbon 3 `diffIn*` now signed and fractional, removed helpers and facades,
  PHP 8.x removals and deprecations) with file:line and the fix, files the new PHP cannot parse, and PHPStan /
  Larastan findings when the repo has it
- **`report.md`**: every route and case, old answer vs new answer
- **PHPUnit tests** in the new repo for routes without one: one test per route with a data provider of its cases
  (happy path, not signed in, missing record, empty and incomplete bodies), each expecting what the old app did
- For AI assistants: an **MCP server**, so Claude Code (or any MCP client) can run it, read the fix list, look at one
  route's old and new answer side by side, fix the code and re-check just that route

## How it checks

Each route is sent several ways, on both apps, and the old app's answer is the expected one:

| Case | Request |
| --- | --- |
| happy | signed in, real ids from the data, for writes a body that passes the route's validation rules |
| guest | the same, not signed in |
| missing | ids that do not exist |
| empty / without-*field* | a save with an empty body, and with each required field left out |

Compared: status code, JSON body (timestamps, tokens and URLs ignored), rows written, queued jobs, mail,
notifications, broadcasts (event, channel) and outgoing HTTP calls.

## Install

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/syahrul84/laravel-parity/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/syahrul84/laravel-parity/main/install.ps1 | iex
```

Both download the release for your system, check it against SHA256SUMS, and register the MCP server with Claude Code.
Later: `laravel-parity update`. From source: `cargo install --git https://github.com/syahrul84/laravel-parity`.

Needs: [codemap](https://github.com/syahrul84/codemap) on the PATH, MySQL reachable from the machine, and for each app
either its Docker stack running or its PHP version installed (Laragon, Herd, XAMPP, php.net builds). No `mysql`
or `mysqldump` client is needed.

## Quick start

```sh
laravel-parity guide                                   # the walkthrough
laravel-parity init ~/code/shop-api-old ~/code/shop-api --name shop
#   finds the containers (or local PHP), the database and the sign-in guards, writes pairs/shop.toml
#   and lists what to fill in (usually the id of an admin user)
laravel-parity check pairs/shop.toml                   # both apps boot on the test databases, sign-in works
laravel-parity all pairs/shop.toml                     # everything; then open runs/shop/fixes.md
```

| Step | What it does |
| --- | --- |
| `setup-db` | copy the old database into two test databases; the new side runs its migrations, as the real upgrade will |
| `check` | PHP, Laravel and MySQL of each app, sign-in, and whether every Laravel internal the tool reads exists |
| `routes` | both route lists and what was removed, added or changed |
| `code` | problems in the new code without running it (codemap doctor, upgrade traps, syntax, PHPStan) |
| `web` | the web app's API calls the upgrade breaks (with `[web]` in the pair file; optional) |
| `plan` | the requests and their cases, in `runs/<name>/requests.json` (edit a body there to test a real save) |
| `run` | every request on both apps; writes `report.md` and `fixes.md` (`--only "api/orders"` re-runs just those) |
| `tests` | PHPUnit tests for routes without one, run in the new app |
| `reports` | problems with the tool itself, which you can send to this repo (you see exactly what is sent first) |

## For AI assistants (MCP)

The installer registers `laravel-parity serve` with Claude Code for all projects. The server tells the assistant when
to use it (upgrades, Laravel Shift, "does the new code behave the same"). The loop it follows:
`fix_list` → `show` a route (old and new answer, exception, first frames) → fix the code → `rerun` that route.

Tools: `guide`, `list_pairs`, `init_pair`, `check`, `setup_db`, `plan`, `run`, `rerun`, `code_check`, `web_check`, `status`,
`summary`, `fix_list`, `results`, `show`, `set_request`, `generate_tests`, `test_results`, `test_gaps`, `add_case`,
`report_problem`. Long steps run as background jobs polled with `status`.

### More test scenarios, written by the AI

Without an AI, the tool generates what it can work out by itself: the happy path, not signed in, a missing record,
an empty body and each required field left out. With an AI assistant on the MCP server, coverage goes further:

1. `test_gaps` lists the routes with the fewest tested scenarios, with the controller's file:line, the cases already
   sent and what the old app answered, and the route's validation rules.
2. The AI reads the controller and adds the scenarios worth checking with `add_case`: boundary values, each value of
   an `in:` rule, the filters and paging the controller reads, wrong types, another project's id, duplicates for a
   `unique` rule. `"{unit}"` (a route parameter) or `"{user}"` in a value stands for that row's id or the signed-in
   user, so the scenario also works on the test database.
3. `rerun` sends the new scenarios to **both** apps. The **old app's answer is the expected one**, so the AI adds
   inputs, not opinions about the right output.
4. `generate_tests` adds them as rows to the route's data provider. A scenario that crashes on both apps is not a
   test but a finding: it goes in the fix list as a bug the old code already had.

Added scenarios are kept by every later plan. Tests that need special setup go in `tests/Feature/Parity/Extra/`,
which the tool never touches.

## Safety

- Both apps run on **test databases** (`<name>_parity_old`, `<name>_parity_new`) copied from the old app's database,
  which is only read. Names without `parity` are refused, and the runner stops if an app is not on its test database.
- Every request runs in a transaction that is **rolled back**.
- Mail, notifications, queued jobs, broadcasts and file storage are **faked** (recorded, never sent or written).
- **Outgoing internet traffic is blocked** (Laravel's HTTP client faked; Guzzle, curl and PHP streams sent to a proxy
  that does not exist).
- Problem reports never leave the machine unless you send one, and contain no response bodies, ids, hosts or names.

## New Laravel versions

The PHP runner uses Laravel's public API, plus a few internals to record side effects (the queue, mail and
notification fakes, the rate limiter, broadcast payloads). Every run checks that each of them exists. If a new Laravel
renames one, the report says that kind of side effect was **not compared** (it never silently calls it "same"), and
the tool files a problem report. CI runs the runner against Laravel 9 to 13 on every change and against Laravel's
development branch every night, so a new version's breaking changes show up before its release; the fix arrives
through `laravel-parity update`.

## Pair file

One TOML file per old/new pair; [`pairs/example.toml`](pairs/example.toml) explains every setting. Passwords never go
in it: the database password comes from `PARITY_DB_PASSWORD`, else from the old repo's `.env`.

## License

MIT
