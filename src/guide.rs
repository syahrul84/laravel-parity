//! The walkthrough printed by `laravel-parity guide`, and the instructions the MCP server gives an AI client.

pub const GUIDE: &str = r#"laravel-parity: is the upgraded app still doing what the old one did?

You have an OLD repo (before the upgrade) and a NEW repo (after Laravel Shift: new PHP, Laravel, MySQL).
The tool sends every shared route the same request on both, on copies of the old database, and lists
what differs, worst first. It can also write PHPUnit tests for routes that have none.

1. Start both apps
   Docker: start each app's stack (docker compose up -d). No Docker: install each app's PHP version
   (Laragon / Herd / XAMPP on Windows) and make sure MySQL is reachable.

2. Make a pair file (once per old/new pair)
     laravel-parity init <old repo> <new repo>
   It finds the containers (or local PHP), the database and the sign-in guards, and writes
   pairs/<name>.toml. Open it and fill in what it marks TODO, mostly [auth] users: the id of an admin
   user in the old database to make the requests as.

3. Check
     laravel-parity check pairs/<name>.toml
   Both apps must boot on their *_parity test databases with no sign-in problems listed.

4. Run everything (30 minutes or so for 5,000 routes)
     laravel-parity all pairs/<name>.toml
   = setup-db (copy the old database into two test databases; the new side runs its migrations)
     routes   (both route lists, from codemap)
     code     (problems in the new code without running it: missing controller methods, upgrade traps such
               as $dates and Carbon 3 diffIn*, syntax, PHPStan when installed)
     schema   (the database: what the upgrade's migrations change in it, and what the migrations alone build
               on an empty database: tables, columns, types, indexes, foreign keys)
     plan     (one request per shared route; {id} parameters get real ids from the test data, ids that belong
               together: a unit of the project the requests are made in)
     run      (every request on both apps, rolled back; mail, jobs, HTTP calls faked; queries and time kept)
     console  (the app's artisan commands and queued jobs run on both apps; the schedule compared)
     channels (who may join each broadcast channel, /broadcasting/auth, the driver: Echo -> Reverb)
     units    (every public method of the app's own classes - models, services, exports ... - run on both
               apps and compared: methods that now throw or return something else, model casts that changed)
     tests    (PHPUnit tests next to the app's own, following app/: a feature test per controller for routes
               without one, a unit test per class from units; with pcov or Xdebug in the new app's PHP it
               shows the lines of app/ covered by the app's own tests and with the generated ones, against
               [tests] target, default 80%; the own suite runs once and is reused until code or tests change)

5. Read runs/<name>/fixes.md first: what to fix, worst first, with the file:line and the likely cause,
   including the web app's calls the upgrade breaks (needs [web] in the pair file).
   runs/<name>/report.md has every route, top down:
   new-broken  the new app fails where the old one worked: fix these first
   status      a different status code (401/403/404 often means middleware or scoping changed)
   effects     different rows written, jobs, mail, notifications or broadcasts
   body        a different answer (new fields are usually fine; missing ones are not)
   minor       only "1" vs 1 or list order
   new-fixed   the old app failed, the new one works
   rejected    both apps refuse the input with validation errors, so the save was not checked; the tool already
               filled the fields the old app asked for and sent it again ([run] learn rounds); the note says
               what is still wanted
   both-fail   both crash the same way: a bug the old code had, or test data the route needs (see below)

6. After fixing a route, run just that part again; it replaces only those results:
     laravel-parity run pairs/<name>.toml --only "api/admin/leases"

Many apps: `laravel-parity overview` shows every pair file, closest to release first, and what changed
since each one's last run. report.md and fixes.md start with "Since the last run" (fixed, newly broken).

In CI: `laravel-parity baseline pairs/<name>.toml` accepts today's findings; `laravel-parity ci
pairs/<name>.toml` runs the checks and exits 1 on a new finding of priority 1 (--fail-on 2 for stricter),
with runs/<name>/junit.xml and a GitHub job summary.

Making it more realistic
  - Real requests: `laravel-parity import-log pairs/<name>.toml access.log` adds what users really sent
    (successful GETs from an nginx / Apache log) as extra cases.
  - Real saves: `laravel-parity recorder pairs/<name>.toml` writes a middleware for the OLD app on a test
    or staging server; it records the bodies of successful saves (secrets left out), and import-log reads
    its file to add them as cases.
  - The inputs the web app sends with every call (user_id, project_id ...): [run] query / body.
  - Commands that need arguments, jobs that need ids: [console] commands / jobs in the pair file.
  - A real body for a save: the "body" of that request in runs/<name>/requests.json.
  - A route parameter it could not fill: [params] in the pair file (a value, or sql:SELECT ...).
  - Noise that differs by design: [compare] ignore_keys (a key name, or a path like $.meta.links[*].page).

Safe by design: test databases only (names must contain "parity"), each request rolled back, mail /
notifications / jobs / broadcasts / file storage faked, outgoing internet blocked.
"#;

pub const MCP_INSTRUCTIONS: &str = r#"laravel-parity checks that an upgraded Laravel app (Laravel Shift: new PHP, Laravel, MySQL) still behaves like the old one. It runs every route shared by an OLD and a NEW repo with the same request on copies of the old database (rolled back, with mail, jobs and HTTP calls faked) and lists the differences, worst first. It can also generate PHPUnit tests for routes that have none.

Use it whenever the user upgrades PHP or Laravel, uses Laravel Shift, or asks whether old and new code behave the same.

Workflow:
1. A pair file names the old and new app. If there is none, call init_pair with both repo paths, then show the user the TODO items it lists (mostly the admin user id in [auth] users; find it with codemap get_schema and a query) and fill them in.
2. check: both apps must boot on the *_parity databases with no sign-in problems. Fix those first (the result says what is wrong).
3. setup_db, then plan, then run. setup_db and run take minutes: they return a job id at once; poll status until it is done. Only one job runs at a time.
4. fix_list is the list of what to fix, worst first (file:line in the new code, likely cause, the web app's broken calls); summary gives the counts. Work through new-broken first, then status, effects, body. results lists routes by verdict or text; show gives one route's old and new answer side by side, including the exception and log lines, which is what you need to fix it.
5. After fixing code, call rerun with that route (or controller) to replace just its results, and confirm it is now same.
5b. The checks beyond routes: schema_check (the database after the migrations, and the migrations alone on an empty database: what moving from Propel to migrations missed), console_check (artisan commands and queued jobs run on both apps; the schedule), channels_check (broadcast channel access after Echo -> Reverb; run it after run). Their findings join fix_list (areas Database, Jobs & commands, Broadcasting); Speed lists routes that run far more queries (N+1). summary shows what changed since the last run; overview shows every pair at once. import_log adds real requests from an access log as cases.
6. units_check runs every public method of the app's own classes on both apps (fix_list area Code). generate_tests writes PHPUnit tests for untested routes into the new repo (tests/Feature/Parity) and runs them; test_results lists the failing ones and the routes that could not get a test. With pcov or Xdebug in the new app's PHP it also measures the lines of app/ covered by the app's own tests and with the generated ones (tests.json "coverage"; the own suite is measured once and reused until the code or tests change). Never install pcov in someone's container without asking. Tests go next to the app's own, following app/ (feature tests per controller, unit tests per class).
8. To reach the coverage target ([tests] target, default 80%): coverage_gaps lists the methods with the most lines no test runs and the test_file / test_class each belongs in. Read the method, ask both apps what it does with probe_code, write the test (class in group laravel-parity-ai, expecting what the OLD app does), run it with check_tests, repeat. A test that fails on the new app because it expects the old behaviour is a bug for the developer to fix, not a test to change.
7. Then widen the coverage: test_gaps lists routes with few tested scenarios, with the controller's file:line, the cases already sent and the validation rules. Read the controller and add the scenarios worth checking with add_case (boundary values, each in: value, filters and paging the controller reads, wrong types, another project's id, unique-rule duplicates; "{param}" / "{user}" stand for that row's id / the signed-in user). rerun the route so both apps answer them (the old app's answer is the expected one), then generate_tests again. For a scenario that needs special setup (a factory state, a relation), write a test yourself in tests/Feature/Parity/Extra/ (the tool never touches that folder).

rejected means both apps refused the body with validation errors even after the tool filled the fields the old app named (the note lists what is still wanted): give a real body with set_request. both-fail means both crash the same way; on POST/PUT it can still mean the body is wrong: add a body to that request in runs/<name>/requests.json, or defaults in the pair file's [run] body. Unfilled route parameters go in [params]. Do not edit generated test files that still carry the @generated marker; remove the marker first if you change one by hand."#;
