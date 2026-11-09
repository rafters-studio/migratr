//! Runs every `.feature` file under this crate's `tests/` directory.
//!
//! A scenario with no matching step code fails `cargo test`, and the failure
//! names the scenario.

use std::path::{Path, PathBuf};
use std::process::Command;

use rstest_bdd_macros::{given, scenarios, then, when};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

struct World {
    root: TempDir,
    /// The working directory migratr runs in; the root unless a step moves it.
    cwd: PathBuf,
    exit: Option<i32>,
    stdout: String,
    stderr: String,
    /// The settings file a step names with --config.
    config: Option<PathBuf>,
    /// The migration `plan` says the next `up` applies, as version_name, and its statements.
    planned: (String, Vec<String>),
    /// Every path under the root, before the step under test.
    before: Vec<PathBuf>,
    /// The version of the migration a step made.
    version: Option<i64>,
    /// The place file migratr dev copies.
    place: Option<PathBuf>,
    /// Each smugglr a dev step runs with, as SMUGGLR and PATH; the one the tests were given
    /// when empty.
    smugglrs: Vec<(String, Option<PathBuf>)>,
    /// The exit code and stderr of each run of a dev step.
    runs: Vec<(Option<i32>, String)>,
    /// The project directories a step runs migratr in, one per case.
    cases: Vec<PathBuf>,
}

#[rstest::fixture]
fn world() -> World {
    let root = TempDir::new().expect("tempdir");
    std::fs::create_dir(root.path().join("migrations")).expect("migrations dir");
    World {
        cwd: root.path().to_path_buf(),
        root,
        exit: None,
        stdout: String::new(),
        stderr: String::new(),
        config: None,
        planned: (String::new(), Vec::new()),
        before: Vec::new(),
        version: None,
        place: None,
        smugglrs: Vec::new(),
        runs: Vec::new(),
        cases: Vec::new(),
    }
}

fn migratr(world: &World, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_migratr"))
        .arg("--json")
        .arg("--db")
        .arg(world.root.path().join("app.db"))
        .arg("--dir")
        .arg(world.root.path().join("migrations"))
        .args(args)
        .current_dir(&world.cwd)
        .output()
        .expect("run migratr")
}

#[given("a project with two applied migrations")]
fn a_project(world: &mut World) {
    assert!(
        migratr(world, &["new", "create_users", "id:integer:pk"])
            .status
            .success()
    );
    assert!(migratr(world, &["up"]).status.success());
    assert!(migratr(world, &["new", "drop_users"]).status.success());
    assert!(migratr(world, &["up"]).status.success());
}

#[when("I run {command:string} without --yes")]
fn run_without_yes(world: &mut World, command: String) {
    let output = migratr(world, &[command.as_str()]);
    world.exit = output.status.code();
    world.stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
}

#[then("the process exits 1 with error code {code:string}")]
fn exits_with_code(world: &mut World, code: String) {
    assert_eq!(world.exit, Some(1));
    let doc: Value = serde_json::from_str(&world.stdout).expect("stdout is one JSON document");
    assert_eq!(doc["code"], code.as_str());
}

fn record(world: &mut World, output: &std::process::Output) {
    world.exit = output.status.code();
    world.stdout = String::from_utf8(output.stdout.clone()).expect("utf8 stdout");
    world.stderr = String::from_utf8(output.stderr.clone()).expect("utf8 stderr");
}

fn write_settings(dir: &Path, text: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("settings dir");
    let path = dir.join("migratr.jsonc");
    std::fs::write(&path, text).expect("write migratr.jsonc");
    path
}

fn no_settings_above(world: &World) {
    assert!(
        world
            .cwd
            .ancestors()
            .all(|dir| !dir.join("migratr.jsonc").exists()),
        "a migratr.jsonc exists above {}",
        world.cwd.display()
    );
}

fn holds_only_the_result(world: &World) {
    assert_eq!(world.exit, Some(0), "{}", world.stdout);
    let doc: Value = serde_json::from_str(&world.stdout).expect("stdout is one JSON document");
    assert_eq!(doc["status"], "ok");
    assert!(world.stderr.is_empty(), "stderr: {}", world.stderr);
}

fn says_defaults(world: &World) {
    for line in [
        "settings: no migratr.jsonc found",
        "smuggl = unset (set by the default)",
        "loud = true (set by the default)",
        "safe = true (set by the default)",
        "autoclean = true (set by the default)",
    ] {
        assert!(
            world.stderr.lines().any(|l| l == line),
            "missing {line:?} in {}",
            world.stderr
        );
    }
}

fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            paths.extend(tree(&path));
        }
        paths.push(path);
    }
    paths.sort();
    paths
}

#[given(
    "a migratr.jsonc with a comment and loud set to false, two directories above the working directory"
)]
fn quiet_settings_above(world: &mut World) {
    let root = world.root.path().to_path_buf();
    write_settings(&root, "// keep runs quiet\n{ \"loud\": false }\n");
    world.cwd = root.join("a").join("b");
    std::fs::create_dir_all(&world.cwd).expect("working directory");
}

#[when("the agent runs migratr status")]
fn runs_status(world: &mut World) {
    let output = migratr(world, &["status"]);
    record(world, &output);
}

#[then("the output holds only the result")]
fn only_the_result(world: &mut World) {
    holds_only_the_result(world);
}

#[given("another migratr.jsonc with loud left on, named by --config")]
fn other_settings(world: &mut World) {
    let dir = world.root.path().join("elsewhere");
    world.config = Some(write_settings(&dir, "{ \"safe\": true }\n"));
}

#[when("the agent runs migratr status --config <that file>")]
fn runs_status_with_config(world: &mut World) {
    let config = world.config.clone().expect("a --config file");
    let config = config.to_str().expect("utf8 path");
    let output = migratr(world, &["status", "--config", config]);
    record(world, &output);
}

#[then("the output names that file as the settings used, and loud as on, set by the default")]
fn names_that_file(world: &mut World) {
    assert_eq!(world.exit, Some(0), "{}", world.stdout);
    let config = world.config.clone().expect("a --config file");
    let used = format!("settings: {}", config.display());
    assert!(world.stderr.lines().any(|l| l == used), "{}", world.stderr);
    assert!(
        world
            .stderr
            .lines()
            .any(|l| l == "loud = true (set by the default)"),
        "{}",
        world.stderr
    );
}

#[given("no migratr.jsonc anywhere above the working directory")]
fn no_settings(world: &mut World) {
    no_settings_above(world);
}

#[then("the output says no settings file was found")]
fn says_none_found(world: &mut World) {
    assert_eq!(world.exit, Some(0), "{}", world.stdout);
    assert!(
        world
            .stderr
            .lines()
            .any(|l| l == "settings: no migratr.jsonc found"),
        "{}",
        world.stderr
    );
}

#[then("it names loud, safe and autoclean as on and smuggl as unset, each set by the default")]
fn names_the_defaults(world: &mut World) {
    says_defaults(world);
}

fn one_pending(world: &mut World) {
    let planned = migratr(world, &["plan"]);
    let doc: Value = serde_json::from_slice(&planned.stdout).expect("plan document");
    let steps = doc["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 1, "{doc}");
    let label = format!(
        "{}_{}",
        steps[0]["version"],
        steps[0]["name"].as_str().expect("name")
    );
    let statements = steps[0]["statements"]
        .as_array()
        .expect("statements")
        .iter()
        .map(|s| s.as_str().expect("statement").to_string())
        .collect();
    world.planned = (label, statements);
}

#[given("one pending migration and no migratr.jsonc")]
fn pending_without_settings(world: &mut World) {
    no_settings_above(world);
    assert!(
        migratr(world, &["new", "create_users", "id:integer:pk"])
            .status
            .success()
    );
    one_pending(world);
}

#[given("a pending migration that adds a column")]
fn pending_add_column(world: &mut World) {
    assert!(
        migratr(world, &["new", "create_users", "id:integer:pk"])
            .status
            .success()
    );
    assert!(migratr(world, &["up"]).status.success());
    let made = migratr(world, &["new", "add_email_to_users", "email:text"]);
    assert!(made.status.success());
    let doc: Value = serde_json::from_slice(&made.stdout).expect("new document");
    let path = doc["path"].as_str().expect("path");
    let file = Path::new(path)
        .file_name()
        .expect("file name")
        .to_string_lossy();
    let version = file
        .split('_')
        .next()
        .expect("version")
        .parse()
        .expect("number");
    world.version = Some(version);
}

#[when("the agent runs migratr up")]
fn runs_up(world: &mut World) {
    world.before = tree(world.root.path());
    let output = migratr(world, &["up"]);
    record(world, &output);
}

#[then(
    "the output says no settings file was found, names each key with its value set by the default, and names each migration as it starts and each statement it runs"
)]
fn loud_up(world: &mut World) {
    assert_eq!(world.exit, Some(0), "{}", world.stdout);
    says_defaults(world);
    let (migration, statements) = &world.planned;
    let starts = format!("migration {migration} starts");
    assert!(
        world.stderr.lines().any(|l| l == starts),
        "missing {starts:?} in {}",
        world.stderr
    );
    for statement in statements {
        let line = format!("  {statement}");
        assert!(
            world.stderr.lines().any(|l| l == line),
            "missing {line:?} in {}",
            world.stderr
        );
    }
}

#[then("it ends with a note that loud can be turned off in migratr.jsonc")]
fn ends_with_note(world: &mut World) {
    let last = world
        .stderr
        .lines()
        .last()
        .expect("a loud run prints lines");
    assert!(
        last.contains("loud can be turned off in migratr.jsonc"),
        "{last}"
    );
}

#[given("the same with loud false in migratr.jsonc")]
fn pending_quiet(world: &mut World) {
    assert!(migratr(world, &["down"]).status.success());
    let root = world.root.path().to_path_buf();
    write_settings(&root, "{ \"loud\": false }\n");
    one_pending(world);
}

#[then("the output holds only the result and any errors")]
fn only_result_and_errors(world: &mut World) {
    holds_only_the_result(world);
}

#[given("a migratr.jsonc holding a key token")]
fn settings_with_token(world: &mut World) {
    let root = world.root.path().to_path_buf();
    write_settings(&root, "{ \"token\": \"s3cret\" }\n");
}

#[then("the output names the key token")]
fn names_token(world: &mut World) {
    let doc: Value = serde_json::from_str(&world.stdout).expect("stdout is one JSON document");
    assert_eq!(doc["code"], "usage");
    let message = doc["message"].as_str().expect("message");
    assert!(message.contains("\"token\""), "{message}");
}

#[then("the exit code is 2")]
fn exits_two(world: &mut World) {
    assert_eq!(world.exit, Some(2));
}

#[then("nothing has changed")]
fn nothing_changed(world: &mut World) {
    assert!(!world.root.path().join("app.db").exists());
    assert_eq!(tree(world.root.path()), world.before);
}

#[then(
    "its ledger row's down hash equals the SHA-256 of what migratr plan down printed for it right after"
)]
fn down_hash_matches_plan_down(world: &mut World) {
    assert_eq!(world.exit, Some(0), "{}", world.stdout);
    let version = world.version.expect("a migration was made");
    let planned = migratr(world, &["plan", "down"]);
    let doc: Value = serde_json::from_slice(&planned.stdout).expect("plan document");
    let step = &doc["steps"][0];
    assert_eq!(step["version"].as_i64(), Some(version), "{doc}");
    let operations: Vec<&str> = step["statements"]
        .as_array()
        .expect("statements")
        .iter()
        .map(|s| s.as_str().expect("statement"))
        .filter(|s| !s.contains("_migratr_migrations"))
        .collect();
    let printed = format!("{:x}", Sha256::digest(operations.join("\n")));

    let conn = rusqlite::Connection::open(world.root.path().join("app.db")).expect("open");
    let recorded: String = conn
        .query_row(
            "SELECT down_hash FROM _migratr_migrations WHERE version = ?1",
            [version],
            |row| row.get(0),
        )
        .expect("down_hash");
    assert_eq!(recorded, printed);
}

/// The smugglr the tests run: the binary SMUGGLR names, built with exec.
fn smugglr_bin() -> String {
    std::env::var("SMUGGLR")
        .ok()
        .filter(|bin| !bin.is_empty())
        .expect("set SMUGGLR to a smugglr with exec to run the dev scenarios")
}

fn sql(path: &Path, batch: &str) {
    rusqlite::Connection::open(path)
        .expect("open")
        .execute_batch(batch)
        .expect("batch");
}

fn rows(path: &Path, query: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).expect("open");
    let mut statement = conn.prepare(query).expect("prepare");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn app_db(world: &World) -> PathBuf {
    world.root.path().join("app.db")
}

fn make_place(world: &mut World, batch: &str) {
    let place = world.root.path().join("place.db");
    sql(&place, batch);
    world.place = Some(place);
}

fn name_place(world: &World) {
    let place = world.place.as_ref().expect("a place");
    let settings = json_settings(place);
    write_settings(world.root.path(), &settings);
}

fn json_settings(place: &Path) -> String {
    serde_json::json!({ "smuggl": place, "loud": false }).to_string()
}

/// A place with rows and a migratr.jsonc naming it, unless a step has written settings.
fn ensure_place(world: &mut World) {
    if world.root.path().join("migratr.jsonc").exists() {
        return;
    }
    if world.place.is_none() {
        make_place(
            world,
            "CREATE TABLE a (id TEXT PRIMARY KEY, v TEXT); INSERT INTO a VALUES ('1', 'x'), ('2', 'y');",
        );
    }
    name_place(world);
}

fn dev_command(world: &World, smugglr: &str, path: Option<&Path>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_migratr"));
    command
        .arg("--db")
        .arg(app_db(world))
        .arg("--dir")
        .arg(world.root.path().join("migrations"))
        .arg("dev")
        .current_dir(&world.cwd)
        .env("SMUGGLR", smugglr)
        .env("XDG_CONFIG_HOME", world.root.path().join("cfg"))
        .env("HOME", world.root.path().join("home"));
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command
}

fn run_dev(world: &mut World) {
    world.before = tree(world.root.path());
    let smugglrs = if world.smugglrs.is_empty() {
        vec![(smugglr_bin(), None)]
    } else {
        world.smugglrs.clone()
    };
    for (smugglr, path) in &smugglrs {
        let output = dev_command(world, smugglr, path.as_deref())
            .output()
            .expect("run migratr dev");
        record(world, &output);
        world.runs.push((world.exit, world.stderr.clone()));
    }
}

fn dev_ready(world: &World) {
    assert_eq!(world.exit, Some(0), "{}{}", world.stdout, world.stderr);
    assert_eq!(
        world.stdout.trim_end().lines().last(),
        Some("Dev ready"),
        "{}",
        world.stdout
    );
}

#[given("a place file with rows in tables a and b, and b in smugglr's ignore list")]
fn place_with_ignored_table(world: &mut World) {
    make_place(
        world,
        "CREATE TABLE a (id TEXT PRIMARY KEY, v TEXT); CREATE TABLE b (id TEXT PRIMARY KEY, v TEXT);
         INSERT INTO a VALUES ('1', 'x'), ('2', 'y'); INSERT INTO b VALUES ('3', 'z');",
    );
    let config = world.root.path().join("cfg").join("smugglr");
    std::fs::create_dir_all(&config).expect("config dir");
    std::fs::write(config.join("config.toml"), "ignore = [\"b\"]\n").expect("config");
}

#[given("a migratr.jsonc with smuggl naming that place")]
fn settings_naming_place(world: &mut World) {
    name_place(world);
}

#[when("the agent runs migratr dev")]
fn runs_dev(world: &mut World) {
    ensure_place(world);
    run_dev(world);
}

#[when("the agent runs migratr dev against a place file")]
fn runs_dev_against_place(world: &mut World) {
    ensure_place(world);
    run_dev(world);
}

#[then("the local database holds a's rows and b's shape with no rows")]
fn holds_a_and_b_shape(world: &mut World) {
    assert_eq!(world.exit, Some(0), "{}", world.stderr);
    let db = app_db(world);
    assert_eq!(
        rows(&db, "SELECT id || v FROM a ORDER BY id"),
        vec!["1x", "2y"]
    );
    assert_eq!(rows(&db, "SELECT CAST(count(*) AS TEXT) FROM b"), vec!["0"]);
    let place = world.place.clone().expect("a place");
    assert_eq!(
        rows(&place, "SELECT CAST(count(*) AS TEXT) FROM b"),
        vec!["1"]
    );
}

#[then("the output ends with Dev ready")]
fn ends_with_dev_ready(world: &mut World) {
    dev_ready(world);
}

#[given(
    "a place file with a trigger that writes a row to an audit table on insert, an index and a view"
)]
fn place_with_trigger(world: &mut World) {
    make_place(
        world,
        "CREATE TABLE audit (id TEXT PRIMARY KEY, what TEXT);
         CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT);
         CREATE TRIGGER notes_audit AFTER INSERT ON notes
           BEGIN INSERT INTO audit VALUES (new.id, 'inserted'); END;
         CREATE INDEX notes_body ON notes (body);
         CREATE VIEW note_bodies AS SELECT body FROM notes;
         INSERT INTO notes VALUES ('n1', 'a'), ('n2', 'b');",
    );
}

#[then("the local audit table holds exactly the place's audit rows")]
fn audit_rows_equal(world: &mut World) {
    dev_ready(world);
    let query = "SELECT id || ':' || what FROM audit ORDER BY id";
    let place = world.place.clone().expect("a place");
    let expected = rows(&place, query);
    assert_eq!(expected.len(), 2);
    assert_eq!(rows(&app_db(world), query), expected);
}

#[then("the local database holds the place's trigger, index and view")]
fn holds_trigger_index_view(world: &mut World) {
    let query = "SELECT type || ' ' || name || ' ' || sql FROM sqlite_master \
                 WHERE type IN ('trigger', 'index', 'view') AND sql IS NOT NULL ORDER BY name";
    let place = world.place.clone().expect("a place");
    let expected = rows(&place, query);
    assert_eq!(expected.len(), 3, "{expected:?}");
    assert_eq!(rows(&app_db(world), query), expected);
}

#[given("a local database that already holds rows")]
fn local_with_rows(world: &mut World) {
    sql(
        &app_db(world),
        "CREATE TABLE old (x TEXT); INSERT INTO old VALUES ('kept');",
    );
}

#[then("a snapshot of the old local database exists among migratr's snapshots")]
fn old_snapshot_exists(world: &mut World) {
    dev_ready(world);
    let kept = kept_snapshot(world);
    assert_eq!(rows(&kept, "SELECT x FROM old"), vec!["kept"]);
}

fn kept_snapshot(world: &World) -> PathBuf {
    let dir = world.root.path().join(".migratr").join("snapshots");
    let kept: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("snapshots")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.to_string_lossy().ends_with("_kept.db"))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    kept[0].clone()
}

#[then("the output names it")]
fn output_names_snapshot(world: &mut World) {
    let kept = kept_snapshot(world);
    assert!(
        world.stdout.contains(&kept.display().to_string()),
        "{}",
        world.stdout
    );
}

fn rewind_point(world: &World) -> Value {
    let found = rows(
        &app_db(world),
        "SELECT checksum FROM _migratr_migrations WHERE version = 0 AND name = 'rewind_point'",
    );
    assert_eq!(found.len(), 1, "{found:?}");
    serde_json::from_str(&found[0]).expect("the rewind point is JSON")
}

#[then("the ledger holds a rewind-point JSON row naming a snapshot and the SHA-256 of its file")]
fn rewind_row(world: &mut World) {
    dev_ready(world);
    let point = rewind_point(world);
    let snapshot = point["snapshot"].as_str().expect("snapshot");
    assert!(Path::new(snapshot).is_file(), "{point}");
    assert_eq!(point["sha256"].as_str().map(str::len), Some(64), "{point}");
}

#[then("that file's SHA-256 is the one recorded")]
fn rewind_sha(world: &mut World) {
    let point = rewind_point(world);
    let bytes = std::fs::read(point["snapshot"].as_str().expect("snapshot")).expect("read");
    assert_eq!(
        point["sha256"].as_str(),
        Some(format!("{:x}", Sha256::digest(&bytes)).as_str())
    );
}

#[then("smugglr sync --dryrun between the snapshot and the local database moves nothing")]
fn dryrun_moves_nothing(world: &mut World) {
    let point = rewind_point(world);
    let output = Command::new(smugglr_bin())
        .arg("sync")
        .arg(point["snapshot"].as_str().expect("snapshot"))
        .arg(app_db(world))
        .args(["--dryrun", "--json"])
        .env("XDG_CONFIG_HOME", world.root.path().join("dryrun-cfg"))
        .output()
        .expect("run smugglr");
    assert!(output.status.success(), "{output:?}");
    let record: Value = serde_json::from_slice(&output.stdout).expect("record");
    let tables = record["tables"].as_object().expect("tables");
    assert!(!tables.is_empty(), "{record}");
    assert!(
        tables.values().all(|t| t["status"] == "in step"),
        "{record}"
    );
}

#[given("no smugglr on the path, or a smugglr whose exec read form is refused")]
fn no_smugglr(world: &mut World) {
    ensure_place(world);
    let empty = world.root.path().join("empty-path");
    std::fs::create_dir(&empty).expect("empty dir");
    // No SMUGGLR and nothing on PATH; then a binary that refuses `exec <db> --read`.
    world.smugglrs = vec![
        (String::new(), Some(empty)),
        (env!("CARGO_BIN_EXE_migratr").to_string(), None),
    ];
}

#[then("the output names smugglr 0.6.3 to install")]
fn names_smugglr_version(world: &mut World) {
    assert_eq!(world.runs.len(), 2);
    for (exit, stderr) in &world.runs {
        assert_eq!(*exit, Some(1), "{stderr}");
        assert!(stderr.contains("install smugglr 0.6.3"), "{stderr}");
    }
}

#[given("a migratr.jsonc with no smuggl")]
fn settings_without_smuggl(world: &mut World) {
    write_settings(world.root.path(), "{ \"loud\": false }\n");
}

#[then("the output names the smuggl setting")]
fn names_smuggl(world: &mut World) {
    assert_eq!(world.exit, Some(2), "{}", world.stderr);
    assert!(world.stderr.contains("\"smuggl\""), "{}", world.stderr);
}

#[given("a local database after migratr dev against a local place file")]
fn after_dev(world: &mut World) {
    ensure_place(world);
    run_dev(world);
    dev_ready(world);
}

#[then("the ledger holds one rewind-point row and one fingerprint row, each holding JSON")]
fn two_notes(world: &mut World) {
    let found = rows(
        &app_db(world),
        "SELECT version || ' ' || name || ' ' || checksum FROM _migratr_migrations ORDER BY version",
    );
    assert_eq!(found.len(), 2, "{found:?}");
    for (row, prefix) in found.iter().zip(["0 rewind_point ", "1 fingerprint "]) {
        let json = row.strip_prefix(prefix).expect("kind at its version");
        let value: Value = serde_json::from_str(json).expect("JSON");
        assert!(value.is_object(), "{row}");
    }
}

#[then("status lists each by its kind")]
fn status_lists_kinds(world: &mut World) {
    assert_eq!(world.exit, Some(0), "{}", world.stdout);
    let doc: Value = serde_json::from_str(&world.stdout).expect("status document");
    let kinds: Vec<&str> = doc["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .map(|n| n["kind"].as_str().expect("kind"))
        .collect();
    assert_eq!(kinds, vec!["rewind_point", "fingerprint"]);
}

#[then("status reports no missing file")]
fn status_no_missing(world: &mut World) {
    let doc: Value = serde_json::from_str(&world.stdout).expect("status document");
    assert_eq!(doc["missing"], serde_json::json!([]), "{doc}");
}

#[then(
    "migratr up applies a pending migration and migratr down reverts it, neither refusing on those rows"
)]
fn up_and_down_run(world: &mut World) {
    assert!(
        migratr(world, &["new", "create_widgets", "id:integer:pk"])
            .status
            .success()
    );
    let up = migratr(world, &["up"]);
    let doc: Value = serde_json::from_slice(&up.stdout).expect("up document");
    assert_eq!(doc["applied"].as_array().map(Vec::len), Some(1), "{doc}");
    let down = migratr(world, &["down"]);
    let doc: Value = serde_json::from_slice(&down.stdout).expect("down document");
    assert_eq!(doc["reverted"].as_array().map(Vec::len), Some(1), "{doc}");
}

/// The ledger migratr makes, as `CREATE_LEDGER` writes it.
const LEDGER: &str = "CREATE TABLE _migratr_migrations (version INTEGER PRIMARY KEY, \
    name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL, up_hash TEXT, \
    down_hash TEXT);";

/// A ledger in `db` holding a rewind point, as migratr dev records it.
fn ledger_with_rewind_point(db: &Path) {
    sql(
        db,
        &format!(
            "{LEDGER} INSERT INTO _migratr_migrations VALUES (0, 'rewind_point', \
             '{{\"snapshot\":\"s.db\",\"sha256\":\"x\"}}', '2026-11-08T00:00:00.000Z', NULL, NULL);"
        ),
    );
}

fn migration_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("migrations dir")
        .map(|e| e.expect("entry").path())
        .collect();
    files.sort();
    files
}

#[given("a migratr.jsonc with smuggl set to a local place file and safe left at its default")]
fn settings_smuggl_safe_default(world: &mut World) {
    let place = world.root.path().join("place.db");
    sql(&place, "CREATE TABLE a (id TEXT PRIMARY KEY);");
    let settings = serde_json::json!({ "smuggl": place }).to_string();
    write_settings(world.root.path(), &settings);
}

#[given("a ledger holding no rewind point")]
fn ledger_without_rewind_point(world: &mut World) {
    sql(&app_db(world), LEDGER);
}

#[when("the agent runs migratr new add_title")]
fn runs_new_add_title(world: &mut World) {
    let output = migratr(world, &["new", "add_title"]);
    record(world, &output);
}

#[then("no migration file is written")]
fn no_migration_written(world: &mut World) {
    assert_eq!(world.exit, Some(1), "{}", world.stdout);
    assert!(
        migration_files(&world.root.path().join("migrations")).is_empty(),
        "{}",
        world.stdout
    );
}

#[then("the output says there is no rewind point and to run migratr dev")]
fn says_no_rewind_point(world: &mut World) {
    let doc: Value = serde_json::from_str(&world.stdout).expect("stdout is one JSON document");
    assert_eq!(doc["code"], "no_rewind_point", "{doc}");
    let message = doc["message"].as_str().expect("message");
    assert!(message.contains("no rewind point"), "{message}");
    assert!(message.contains("run migratr dev"), "{message}");
}

#[given(
    "each of: a migratr.jsonc with safe false and smuggl set; a migratr.jsonc with no smuggl; a migratr.jsonc with smuggl set and a ledger holding a rewind point"
)]
fn each_case(world: &mut World) {
    let place = world.root.path().join("place.db");
    sql(&place, "CREATE TABLE a (id TEXT PRIMARY KEY);");
    let cases = [
        (
            "safe_off",
            serde_json::json!({ "smuggl": place, "safe": false }),
        ),
        ("no_smuggl", serde_json::json!({ "loud": false })),
        ("rewind_point", serde_json::json!({ "smuggl": place })),
    ];
    for (name, settings) in cases {
        let dir = world.root.path().join(name);
        std::fs::create_dir_all(dir.join("migrations")).expect("migrations dir");
        write_settings(&dir, &settings.to_string());
        world.cases.push(dir);
    }
    ledger_with_rewind_point(&world.root.path().join("rewind_point").join("app.db"));
}

#[when("the agent runs migratr new add_title in each")]
fn runs_new_in_each(world: &mut World) {
    for dir in &world.cases {
        let output = Command::new(env!("CARGO_BIN_EXE_migratr"))
            .arg("--json")
            .arg("--db")
            .arg(dir.join("app.db"))
            .arg("--dir")
            .arg(dir.join("migrations"))
            .args(["new", "add_title"])
            .current_dir(dir)
            .output()
            .expect("run migratr");
        world.runs.push((
            output.status.code(),
            String::from_utf8(output.stdout).expect("utf8 stdout"),
        ));
    }
}

#[then("each writes one migration file as new did before")]
fn each_writes_one(world: &mut World) {
    assert_eq!(world.runs.len(), 3);
    for (dir, (exit, stdout)) in world.cases.iter().zip(&world.runs) {
        assert_eq!(*exit, Some(0), "{}: {stdout}", dir.display());
        let doc: Value = serde_json::from_str(stdout).expect("new document");
        let files = migration_files(&dir.join("migrations"));
        assert_eq!(files.len(), 1, "{}: {files:?}", dir.display());
        assert_eq!(doc["path"].as_str(), files[0].to_str(), "{doc}");
        assert!(
            files[0].to_string_lossy().ends_with("_add_title.json"),
            "{files:?}"
        );
    }
}

scenarios!("tests", fixtures = [world: World]);
