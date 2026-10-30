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
     plan     (one request per shared route; {id} parameters get real ids from the test data)
     run      (every request on both apps, rolled back; mail, jobs, HTTP calls faked)
     tests    (PHPUnit tests for routes without one, in the new repo's tests/Feature/Parity)

5. Read runs/<name>/fixes.md first: what to fix, worst first, with the file:line and the likely cause,
   including the web app's calls the upgrade breaks (needs [web] in the pair file).
   runs/<name>/report.md has every route, top down:
   new-broken  the new app fails where the old one worked: fix these first
   status      a different status code (401/403/404 often means middleware or scoping changed)
   effects     different rows written, jobs, mail, notifications or broadcasts
   body        a different answer (new fields are usually fine; missing ones are not)
   minor       only "1" vs 1 or list order
   new-fixed   the old app failed, the new one works
   both-fail   both fail: usually the request needs a real body or more data (see below)

6. After fixing a route, run just that part again; it replaces only those results:
     laravel-parity run pairs/<name>.toml --only "api/admin/leases"

Making it more realistic
  - The inputs the web app sends with every call (user_id, project_id ...): [run] query / body.
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
6. generate_tests writes PHPUnit tests for untested routes into the new repo (tests/Feature/Parity) and runs them; test_results lists the failing ones and the routes that could not get a test.
7. Then widen the coverage: test_gaps lists routes with few tested scenarios, with the controller's file:line, the cases already sent and the validation rules. Read the controller and add the scenarios worth checking with add_case (boundary values, each in: value, filters and paging the controller reads, wrong types, another project's id, unique-rule duplicates; "{param}" / "{user}" stand for that row's id / the signed-in user). rerun the route so both apps answer them (the old app's answer is the expected one), then generate_tests again. For a scenario that needs special setup (a factory state, a relation), write a test yourself in tests/Feature/Parity/Extra/ (the tool never touches that folder).

both-fail on POST/PUT usually means the request body is empty: add a body to that request in runs/<name>/requests.json, or defaults in the pair file's [run] body. Unfilled route parameters go in [params]. Do not edit generated test files that still carry the @generated marker; remove the marker first if you change one by hand."#;
