//! Runs every `.feature` file under this crate's `tests/` directory.
//!
//! A scenario with no matching step code fails `cargo test`, and the failure
//! names the scenario.

use std::path::{Path, PathBuf};
use std::process::Command;

use rstest_bdd_macros::{given, scenarios, then, when};

#[path = "support/d1.rs"]
mod d1;
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
    /// The stdout of each run of a ship step.
    stdouts: Vec<String>,
    /// The bytes of the place and of the local database before a dev step, `None` for a file
    /// not there.
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// What migratr and smugglr are run with besides, such as how to reach the D1 stand-in.
    env: Vec<(String, String)>,
    /// The rewind point dev recorded, before the step under test.
    rewind: Option<PathBuf>,
    /// Each run of a step that runs dev more than once, with the local database before and
    /// after it.
    dev_runs: Vec<DevRun>,
}

/// One run of migratr dev, with the local database's bytes and rewind-point rows before and
/// after it.
struct DevRun {
    exit: Option<i32>,
    output: String,
    before: (Option<Vec<u8>>, Vec<String>),
    after: (Option<Vec<u8>>, Vec<String>),
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
        stdouts: Vec::new(),
        files: Vec::new(),
        env: Vec::new(),
        rewind: None,
        dev_runs: Vec::new(),
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

fn dev_command(world: &World, smugglr: &str, path: Option<&Path>, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_migratr"));
    command
        .arg("--db")
        .arg(app_db(world))
        .arg("--dir")
        .arg(world.root.path().join("migrations"))
        .arg("dev")
        .args(args)
        .current_dir(&world.cwd)
        .env("SMUGGLR", smugglr)
        .env("XDG_CONFIG_HOME", world.root.path().join("cfg"))
        .env("HOME", world.root.path().join("home"))
        .envs(world.env.iter().cloned());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command
}

fn run_dev(world: &mut World) {
    run_dev_with(world, &[]);
}

fn run_dev_with(world: &mut World, args: &[&str]) {
    world.before = tree(world.root.path());
    world.files = files(world);
    let smugglrs = if world.smugglrs.is_empty() {
        vec![(smugglr_bin(), None)]
    } else {
        world.smugglrs.clone()
    };
    for (smugglr, path) in &smugglrs {
        let output = dev_command(world, smugglr, path.as_deref(), args)
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
    in_step(
        world,
        Path::new(point["snapshot"].as_str().expect("snapshot")),
    );
}

/// A dry-run sync between `snapshot` and the local database has every table in step.
fn in_step(world: &World, snapshot: &Path) {
    let output = Command::new(smugglr_bin())
        .arg("sync")
        .arg(snapshot)
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

/// org, member and the org_name index, as a migration writes them.
const ADOPT_MIGRATION: &str = r#"{"up": [
  {"op": "create_table", "table": "org", "columns": [
    {"name": "id", "type": "TEXT", "primary_key": 1},
    {"name": "name", "type": "TEXT", "not_null": true}]},
  {"op": "create_index", "name": "org_name", "table": "org", "columns": ["name"]},
  {"op": "create_table", "table": "member", "columns": [
    {"name": "id", "type": "TEXT", "primary_key": 1},
    {"name": "org", "type": "TEXT", "not_null": true,
     "references": {"table": "org", "column": "id"}},
    {"name": "email", "type": "TEXT", "not_null": true},
    {"name": "qty", "type": "INTEGER", "not_null": true, "default": "0"}],
   "checks": ["qty >= 0"], "unique": [["org", "email"]]}
]}"#;

/// The same schema written by hand, in other quoting, case and whitespace, with rows.
const ADOPT_PLACE: &str = "
  create table org (id text primary key, name text not null);
  CREATE INDEX org_name ON org (name);
  create table member (
    id text PRIMARY KEY,
    org text not null references org,
    email text NOT NULL,
    qty integer not null default 0,
    check(qty >= 0),
    unique(org,email));
  insert into org values ('o1', 'acme');
  insert into member values ('m1', 'o1', 'a@acme.test', 2);";

/// The place file and the local database, with their bytes, `None` for a file not there.
fn files(world: &World) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut paths = vec![app_db(world)];
    paths.extend(world.place.clone());
    paths
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).ok();
            (path, bytes)
        })
        .collect()
}

fn unchanged(world: &World) {
    assert_eq!(files(world), world.files, "a file changed");
    assert_eq!(tree(world.root.path()), world.before);
}

fn write_migration(world: &World, text: &str) {
    let dir = world.root.path().join("migrations");
    for entry in std::fs::read_dir(&dir).expect("migrations") {
        std::fs::remove_file(entry.expect("entry").path()).expect("remove migration");
    }
    std::fs::write(dir.join("20260101000001_init.json"), text).expect("migration");
}

/// A place file made from `batch` and named in migratr.jsonc, replacing any earlier place.
fn fresh_place(world: &mut World, batch: &str) {
    let place = world.root.path().join("place.db");
    if place.exists() {
        std::fs::remove_file(&place).expect("remove place");
    }
    make_place(world, batch);
    name_place(world);
}

fn place_fingerprint(world: &World) -> String {
    fingerprint_of(world, world.place.as_ref().expect("a place").as_os_str())
}

/// smugglr exec's read form of the database or place `address`.
fn read_of(world: &World, address: &std::ffi::OsStr) -> Value {
    let output = Command::new(smugglr_bin())
        .arg("exec")
        .arg(address)
        .arg("--read")
        .envs(world.env.iter().cloned())
        .output()
        .expect("run smugglr");
    serde_json::from_slice(&output.stdout).expect("read form")
}

/// The fingerprint smugglr exec's read form gives for the database or place `address`.
fn fingerprint_of(world: &World, address: &std::ffi::OsStr) -> String {
    read_of(world, address)["fingerprint"]
        .as_str()
        .expect("fingerprint")
        .to_string()
}

fn migration_rows(world: &World) -> Vec<String> {
    rows(
        &app_db(world),
        "SELECT CAST(version AS TEXT) FROM _migratr_migrations WHERE version NOT IN (0, 1) \
         ORDER BY version",
    )
}

#[given(
    "a place file with tables and no migratr ledger whose schema by structure is what the migrations make"
)]
fn adoptable_place(world: &mut World) {
    write_migration(world, ADOPT_MIGRATION);
    fresh_place(world, ADOPT_PLACE);
}

#[then("the ledger records every migration and the place's fingerprint")]
fn ledger_records_adoption(world: &mut World) {
    dev_ready(world);
    assert!(
        world.stdout.contains("adopted the place"),
        "{}",
        world.stdout
    );
    assert_eq!(migration_rows(world), vec!["20260101000001"]);
    let found = rows(
        &app_db(world),
        "SELECT checksum FROM _migratr_migrations WHERE version = 1 AND name = 'fingerprint'",
    );
    assert_eq!(found.len(), 1, "{found:?}");
    let note: Value = serde_json::from_str(&found[0]).expect("JSON");
    assert_eq!(
        note["fingerprint"].as_str(),
        Some(place_fingerprint(world).as_str())
    );
    assert_eq!(rows(&app_db(world), "SELECT name FROM org"), vec!["acme"]);
}

#[given("instead a place file whose table t lacks a CHECK the migrations make")]
fn place_lacking_check(world: &mut World) {
    // A new start: no local database and no snapshots from the run before.
    std::fs::remove_file(app_db(world)).expect("remove local database");
    std::fs::remove_dir_all(world.root.path().join(".migratr")).expect("remove .migratr");
    write_migration(
        world,
        r#"{"up": [{"op": "create_table", "table": "t", "columns": [
          {"name": "id", "type": "TEXT", "primary_key": 1},
          {"name": "qty", "type": "INTEGER"}], "checks": ["qty >= 0"]}]}"#,
    );
    fresh_place(
        world,
        "CREATE TABLE t (id TEXT PRIMARY KEY, qty INTEGER); INSERT INTO t VALUES ('1', 5);",
    );
    sql(
        &app_db(world),
        "CREATE TABLE mine (x TEXT); INSERT INTO mine VALUES ('local');",
    );
}

#[then("the output names table t and the missing CHECK")]
fn names_missing_check(world: &mut World) {
    assert_eq!(world.exit, Some(1), "{}", world.stderr);
    assert!(
        world
            .stderr
            .contains("table t: the place lacks CHECK check(qty >= 0)"),
        "{}",
        world.stderr
    );
}

#[then("neither the place nor the local database has changed")]
fn neither_changed(world: &mut World) {
    unchanged(world);
}

#[given("migratr dev has run against a place file")]
fn dev_has_run(world: &mut World) {
    adoptable_place(world);
    run_dev(world);
    dev_ready(world);
    assert!(
        world.stdout.contains("adopted the place"),
        "{}",
        world.stdout
    );
}

#[when("the agent runs migratr dev again")]
fn runs_dev_again(world: &mut World) {
    run_dev(world);
}

/// Waits for the start of the next second.
fn next_second() {
    let second = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    while std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        == second
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[then("no adoption runs and the output ends with Dev ready")]
fn no_adoption(world: &mut World) {
    dev_ready(world);
    assert!(!world.stdout.contains("adopted"), "{}", world.stdout);
    assert_eq!(migration_rows(world), vec!["20260101000001"]);
}

#[given("a column added to the place outside migratr")]
fn column_added(world: &mut World) {
    let place = world.place.clone().expect("a place");
    let table = rows(
        &place,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('org', 'notes')",
    );
    let table = table.first().expect("a table of the place");
    sql(
        &place,
        &format!("ALTER TABLE {table} ADD COLUMN extra TEXT"),
    );
}

#[then("nothing has changed on either side")]
fn nothing_changed_either_side(world: &mut World) {
    assert_eq!(world.exit, Some(1), "{}", world.stderr);
    unchanged(world);
}

#[then("the output says the place's shape changed outside migratr and names --discover")]
fn says_shape_changed(world: &mut World) {
    assert!(
        world
            .stderr
            .contains("the place's shape changed outside migratr"),
        "{}",
        world.stderr
    );
    assert!(world.stderr.contains("--discover"), "{}", world.stderr);
}

#[when("the agent runs migratr dev --discover against a place file")]
fn runs_dev_discover(world: &mut World) {
    ensure_place(world);
    sql(
        &app_db(world),
        "CREATE TABLE mine (x TEXT); INSERT INTO mine VALUES ('local');",
    );
    run_dev_with(world, &["--discover"]);
}

#[then("the --discover report of the local database and the place is printed")]
fn discover_printed(world: &mut World) {
    assert_eq!(world.exit, Some(0), "{}", world.stderr);
    let place = world.place.clone().expect("a place");
    for line in [
        format!("source: file {}", app_db(world).display()),
        format!("target: file {}", place.display()),
        "shapes: differ".to_string(),
    ] {
        assert!(
            world.stdout.lines().any(|l| l == line),
            "missing {line:?} in {}",
            world.stdout
        );
    }
    assert!(
        world
            .stdout
            .lines()
            .any(|l| l.starts_with("  table mine: 1 rows")),
        "{}",
        world.stdout
    );
}

#[then("neither has changed")]
fn neither_has_changed(world: &mut World) {
    unchanged(world);
}

// up --smuggl (FR-MIGRATR-024) and the ledger it tidies (FR-MIGRATR-003).

/// The place the ship scenarios start from.
const NOTES_PLACE: &str = "CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT); \
     INSERT INTO notes VALUES ('p1', 'one'), ('p2', 'two'), ('p3', 'three');";

/// The version of the migration a ship scenario adds after dev.
const SHIPPED: &str = "20260201000000";

/// migratr run in the root with the smugglr the tests were given.
fn smuggl_command(world: &World, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_migratr"));
    command
        .arg("--db")
        .arg(app_db(world))
        .arg("--dir")
        .arg(world.root.path().join("migrations"))
        .args(args)
        .current_dir(&world.cwd)
        .env("SMUGGLR", smugglr_bin())
        .env("XDG_CONFIG_HOME", world.root.path().join("cfg"))
        .env("HOME", world.root.path().join("home"))
        .envs(world.env.iter().cloned());
    command
}

/// Runs each of `runs` in turn, after noting the files, the tree and the rewind point.
fn run_smuggl(world: &mut World, runs: &[&[&str]]) {
    world.before = tree(world.root.path());
    world.files = files(world);
    world.rewind = recorded_rewind(world);
    world.runs.clear();
    world.stdouts.clear();
    for args in runs {
        let output = smuggl_command(world, args).output().expect("run migratr");
        record(world, &output);
        world.runs.push((world.exit, world.stderr.clone()));
        world.stdouts.push(world.stdout.clone());
    }
}

/// The snapshot the ledger's rewind-point row names, if it has one.
fn recorded_rewind(world: &World) -> Option<PathBuf> {
    if !app_db(world).exists() {
        return None;
    }
    let found = rows(
        &app_db(world),
        "SELECT checksum FROM sqlite_master, _migratr_migrations \
         WHERE sqlite_master.name = '_migratr_migrations' AND version = 0 LIMIT 1",
    );
    let point: Value = serde_json::from_str(found.first()?).ok()?;
    point["snapshot"].as_str().map(PathBuf::from)
}

fn place_file(world: &World) -> PathBuf {
    world.place.clone().expect("a place")
}

/// The snapshots among migratr's whose names end with `suffix`.
fn snapshots(world: &World, suffix: &str) -> Vec<PathBuf> {
    let dir = world.root.path().join(".migratr").join("snapshots");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .map(|e| e.expect("entry").path())
        .filter(|p| p.to_string_lossy().ends_with(suffix))
        .collect()
}

/// Writes the migration `version`_`name` beside any there are.
fn add_migration(world: &World, version: &str, name: &str, text: &str) {
    std::fs::write(
        world
            .root
            .path()
            .join("migrations")
            .join(format!("{version}_{name}.json")),
        text,
    )
    .expect("migration");
}

fn dev_against(world: &mut World, place: &str) {
    fresh_place(world, place);
    run_dev(world);
    dev_ready(world);
}

fn add_tag(world: &World) {
    add_migration(
        world,
        SHIPPED,
        "add_tag",
        r#"{"up": [{"op": "add_column", "table": "notes", "column": {"name": "tag", "type": "TEXT"}}]}"#,
    );
}

fn shipped(world: &World) {
    assert_eq!(world.exit, Some(0), "{}{}", world.stdout, world.stderr);
    assert_eq!(
        world.stdout.trim_end().lines().last(),
        Some("done"),
        "{}",
        world.stdout
    );
}

fn place_holds_shipped(world: &World) {
    assert_eq!(
        rows(
            &place_file(world),
            &format!("SELECT name FROM _migratr_migrations WHERE version = {SHIPPED}")
        ),
        vec!["add_tag"]
    );
}

#[given("migratr dev against a place file and one new local migration")]
fn dev_and_migration(world: &mut World) {
    dev_against(world, NOTES_PLACE);
    add_tag(world);
}

#[given("migratr dev against a local place file and one new local migration")]
fn dev_local_and_migration(world: &mut World) {
    dev_and_migration(world);
}

#[given("migratr dev against a place file")]
fn dev_against_place(world: &mut World) {
    dev_against(world, NOTES_PLACE);
}

#[when("the agent runs migratr up --smuggl")]
fn runs_up_smuggl(world: &mut World) {
    run_smuggl(world, &[&["up", "--smuggl"]]);
}

#[when("the agent runs migratr up --smuggl with autoclean left on")]
fn runs_up_smuggl_autoclean(world: &mut World) {
    let settings =
        std::fs::read_to_string(world.root.path().join("migratr.jsonc")).expect("settings");
    assert!(!settings.contains("autoclean"), "{settings}");
    runs_up_smuggl(world);
}

#[then("the place holds the migration in its ledger")]
fn place_holds_migration(world: &mut World) {
    shipped(world);
    place_holds_shipped(world);
}

#[then("the place's fingerprint equals the local database's")]
fn fingerprints_equal(world: &mut World) {
    assert_eq!(
        place_fingerprint(world),
        fingerprint_of(world, app_db(world).as_os_str())
    );
}

#[given("the place then inserts one row, updates one and deletes one")]
fn place_changes(world: &mut World) {
    sql(
        &place_file(world),
        "INSERT INTO notes VALUES ('p4', 'four'); UPDATE notes SET body = 'ONE' WHERE id = 'p1'; \
         DELETE FROM notes WHERE id = 'p2';",
    );
}

#[given("the working database gains a row of its own")]
fn working_row(world: &mut World) {
    sql(&app_db(world), "INSERT INTO notes VALUES ('w', 'work');");
}

const NOTES_ROWS: &str = "SELECT id || ':' || body FROM notes ORDER BY id";

#[then("the local database holds the place's insert and update and not its deleted row")]
fn local_has_place_changes(world: &mut World) {
    shipped(world);
    assert_eq!(
        rows(&app_db(world), NOTES_ROWS),
        vec!["p1:ONE", "p3:three", "p4:four"]
    );
}

#[then("the place does not hold the working database's row")]
fn place_lacks_working_row(world: &mut World) {
    assert_eq!(
        rows(&place_file(world), NOTES_ROWS),
        vec!["p1:ONE", "p3:three", "p4:four"]
    );
}

/// The working database's snapshot the output names.
fn named_kept(world: &World) -> PathBuf {
    let line = world
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("kept the working database as "))
        .expect("the output names the working database's snapshot");
    PathBuf::from(line)
}

fn rewind_rows(world: &World) -> Vec<String> {
    rows(
        &app_db(world),
        "SELECT name FROM _migratr_migrations WHERE version = 0",
    )
}

#[then("the rewind-point snapshot, its ledger row and the working database's snapshot are gone")]
fn three_gone(world: &mut World) {
    shipped(world);
    let rewind = world.rewind.clone().expect("dev recorded a rewind point");
    assert!(!rewind.exists(), "{}", rewind.display());
    assert!(rewind_rows(world).is_empty());
    assert!(!named_kept(world).exists());
    assert!(snapshots(world, "_kept.db").is_empty());
    assert!(snapshots(world, "_dev.db").is_empty());
}

#[then("the output names each")]
fn names_each(world: &mut World) {
    let rewind = world.rewind.clone().expect("rewind point");
    for line in [
        format!("removed the rewind point {}", rewind.display()),
        "removed the rewind point's ledger row".to_string(),
        format!(
            "removed the working database's snapshot {}",
            named_kept(world).display()
        ),
    ] {
        assert!(
            world.stdout.lines().any(|l| l == line),
            "missing {line:?} in {}",
            world.stdout
        );
    }
}

#[given("the same with autoclean false in migratr.jsonc")]
fn same_without_autoclean(world: &mut World) {
    std::fs::remove_file(app_db(world)).expect("remove local database");
    std::fs::remove_dir_all(world.root.path().join(".migratr")).expect("remove .migratr");
    std::fs::remove_file(
        world
            .root
            .path()
            .join("migrations")
            .join(format!("{SHIPPED}_add_tag.json")),
    )
    .expect("remove migration");
    fresh_place(world, NOTES_PLACE);
    let place = place_file(world);
    write_settings(
        world.root.path(),
        &serde_json::json!({ "smuggl": place, "loud": false, "autoclean": false }).to_string(),
    );
    run_dev(world);
    dev_ready(world);
    add_tag(world);
}

#[then("all three remain")]
fn three_remain(world: &mut World) {
    shipped(world);
    place_holds_shipped(world);
    let rewind = world.rewind.clone().expect("rewind point");
    assert!(rewind.is_file(), "{}", rewind.display());
    assert_eq!(rewind_rows(world), vec!["rewind_point"]);
    assert!(named_kept(world).is_file());
}

#[given(
    "migratr dev against a place file and a new local migration that drops a column, so migratr snapshots before it runs"
)]
fn dev_and_drop(world: &mut World) {
    dev_against(
        world,
        "CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT, extra TEXT); \
         INSERT INTO notes VALUES ('p1', 'one', 'x'), ('p2', 'two', 'y');",
    );
    add_migration(
        world,
        SHIPPED,
        "drop_extra",
        r#"{"up": [{"op": "drop_column", "table": "notes", "column": {"name": "extra", "type": "TEXT"}}]}"#,
    );
}

#[given("the place will not end with the expected fingerprint")]
fn place_off_shape(world: &mut World) {
    // A place ledger that refuses the migration's row. The ledger is not part of the
    // fingerprint, so the shape stop passes, and smugglr exec's batch, which writes the row,
    // fails whole: the place never gets the migration's shape.
    sql(
        &place_file(world),
        &format!(
            "CREATE TABLE _migratr_migrations (version INTEGER PRIMARY KEY \
             CHECK (version <> {SHIPPED}), name TEXT NOT NULL, checksum TEXT NOT NULL, \
             applied_at TEXT NOT NULL, up_hash TEXT, down_hash TEXT);"
        ),
    );
}

#[then("the local database's hashes equal the rewind point's")]
fn local_is_rewind(world: &mut World) {
    let rewind = world.rewind.clone().expect("rewind point");
    in_step(world, &rewind);
    let columns = rows(
        &app_db(world),
        "SELECT name FROM pragma_table_info('notes')",
    );
    assert_eq!(columns, vec!["id", "body", "extra"]);
}

#[then("its ledger holds the rewind-point and fingerprint rows")]
fn ledger_has_notes(world: &mut World) {
    assert_eq!(
        Some(PathBuf::from(
            rewind_point(world)["snapshot"].as_str().expect("snapshot")
        )),
        world.rewind
    );
    assert_eq!(
        rows(
            &app_db(world),
            "SELECT name FROM _migratr_migrations WHERE version = 1"
        ),
        vec!["fingerprint"]
    );
    assert!(
        migration_rows(world).is_empty(),
        "{:?}",
        migration_rows(world)
    );
}

#[then("the errors are printed and the exit code is non-zero")]
fn errors_printed(world: &mut World) {
    assert_ne!(world.exit, Some(0), "{}", world.stdout);
    assert!(
        world.stderr.contains("error [ship_failed]"),
        "{}",
        world.stderr
    );
    assert!(world.stderr.contains("fingerprint"), "{}", world.stderr);
}

#[then("the rewind point snapshot and the working database's snapshot remain")]
fn snapshots_remain(world: &mut World) {
    let rewind = world.rewind.clone().expect("rewind point");
    assert!(rewind.is_file(), "{}", rewind.display());
    assert_eq!(snapshots(world, "_kept.db").len(), 1);
    // The drop's own snapshot was taken, and its prune kept dev's.
    assert_eq!(snapshots(world, "_up.db").len(), 1);
}

#[when("the place is fixed and the agent runs migratr up --smuggl again")]
fn fixed_and_again(world: &mut World) {
    sql(&place_file(world), "DROP TABLE _migratr_migrations;");
    next_second();
    runs_up_smuggl(world);
}

#[then("the migration reaches the place")]
fn migration_reaches_place(world: &mut World) {
    shipped(world);
    assert_eq!(
        rows(
            &place_file(world),
            &format!("SELECT name FROM _migratr_migrations WHERE version = {SHIPPED}")
        ),
        vec!["drop_extra"]
    );
    assert_eq!(
        rows(
            &place_file(world),
            "SELECT name FROM pragma_table_info('notes')"
        ),
        vec!["id", "body"]
    );
    fingerprints_equal(world);
}

#[given("a ledger with no rewind point")]
fn ledger_without_rewind(world: &mut World) {
    fresh_place(world, NOTES_PLACE);
    add_tag(world);
    sql(&app_db(world), NOTES_PLACE);
    let up = migratr(world, &["up"]);
    assert!(up.status.success(), "{up:?}");
}

#[when("the agent runs migratr up --smuggl, and then migratr down --smuggl")]
fn runs_up_and_down_smuggl(world: &mut World) {
    run_smuggl(world, &[&["up", "--smuggl"], &["down", "--smuggl"]]);
}

#[then("nothing has changed anywhere")]
fn nothing_anywhere(world: &mut World) {
    assert_eq!(world.runs.len(), 2);
    unchanged(world);
    if world.rewind.is_some() {
        // The rewind point is still the file dev recorded, so a real run can use it.
        rewind_sha(world);
    }
}

#[then("each says to run migratr dev")]
fn each_says_dev(world: &mut World) {
    for (exit, stderr) in &world.runs {
        assert_eq!(*exit, Some(1), "{stderr}");
        assert!(stderr.contains("run migratr dev"), "{stderr}");
    }
}

#[then("each says the place's shape changed outside migratr and names --discover")]
fn each_says_shape_changed(world: &mut World) {
    for ((exit, stderr), command) in world.runs.iter().zip(["up", "down"]) {
        assert_eq!(*exit, Some(1), "{stderr}");
        assert!(
            stderr.contains("error [shape_changed]: the place's shape changed outside migratr"),
            "{stderr}"
        );
        assert!(
            stderr.contains(&format!("run migratr {command} --smuggl --discover")),
            "{stderr}"
        );
    }
}

#[given(
    "migratr dev against a place file and a local migration that drops table old holding 3 rows"
)]
fn dev_and_drop_old(world: &mut World) {
    dev_against(
        world,
        &format!(
            "{NOTES_PLACE} CREATE TABLE old (id TEXT PRIMARY KEY); \
             INSERT INTO old VALUES ('o1'), ('o2'), ('o3');"
        ),
    );
    add_migration(
        world,
        SHIPPED,
        "drop_old",
        r#"{"up": [{"op": "drop_table", "table": "old", "definition": {"name": "old",
          "columns": [{"name": "id", "type": "TEXT", "primary_key": 1}],
          "sql": "CREATE TABLE old (id TEXT PRIMARY KEY)"}}]}"#,
    );
}

#[when("the agent runs migratr up --smuggl --discover, and then migratr down --smuggl --discover")]
fn runs_discover(world: &mut World) {
    run_smuggl(
        world,
        &[
            &["up", "--smuggl", "--discover"],
            &["down", "--smuggl", "--discover"],
        ],
    );
}

#[then(
    "each prints the place's --discover report and the migrations it would send with their statements"
)]
fn discover_reports(world: &mut World) {
    let target = format!("target: file {}", place_file(world).display());
    for (stdout, command) in world.stdouts.iter().zip(["up", "down"]) {
        assert!(stdout.lines().any(|l| l == target), "{stdout}");
        assert!(
            stdout
                .lines()
                .any(|l| l == format!("migratr {command} --smuggl would send:")),
            "{stdout}"
        );
    }
    let up = &world.stdouts[0];
    let step = up
        .lines()
        .position(|l| l.starts_with(&format!("  {SHIPPED}_drop_old")))
        .unwrap_or_else(|| panic!("no step for {SHIPPED}_drop_old in {up}"));
    // Only the migration's own statements: smugglr writes the ledger row.
    assert_eq!(
        up.lines().nth(step + 1),
        Some("    DROP TABLE \"old\""),
        "{up}"
    );
    let sent: Vec<&str> = up
        .lines()
        .skip_while(|l| !l.ends_with("would send:"))
        .collect();
    assert!(
        sent.iter().all(|l| !l.contains("_migratr_migrations")),
        "{up}"
    );
    // Dev recorded no migration the place held, so down would send none.
    assert!(
        world.stdouts[1].contains("nothing to do"),
        "{}",
        world.stdouts[1]
    );
}

#[then("the up report names table old and 3 rows destroyed")]
fn names_old_destroyed(world: &mut World) {
    let up = &world.stdouts[0];
    assert!(
        up.lines()
            .any(|l| l == format!("{SHIPPED} destroys table old on the place: 3 rows")),
        "{up}"
    );
}

#[then("each exits 0, or 3 when the run would stop")]
fn exits_zero_or_three(world: &mut World) {
    for ((exit, stderr), stdout) in world.runs.iter().zip(&world.stdouts) {
        let stops = stdout.lines().any(|l| l.starts_with("the run would stop"));
        assert_eq!(*exit, Some(if stops { 3 } else { 0 }), "{stdout}{stderr}");
    }
    // Here neither run would stop.
    assert!(world.runs.iter().all(|(exit, _)| *exit == Some(0)));
}

#[given("migratr dev, a new local migration, and migratr up --smuggl")]
fn dev_migration_and_up(world: &mut World) {
    dev_and_migration(world);
    next_second();
    runs_up_smuggl(world);
    shipped(world);
    place_holds_shipped(world);
}

#[given("migratr dev again")]
fn dev_again(world: &mut World) {
    next_second();
    run_dev(world);
    dev_ready(world);
}

#[when("the agent runs migratr down --smuggl")]
fn runs_down_smuggl(world: &mut World) {
    next_second();
    run_smuggl(world, &[&["down", "--smuggl"]]);
}

#[then("the place's ledger no longer holds that migration")]
fn place_lacks_migration(world: &mut World) {
    shipped(world);
    assert!(
        rows(
            &place_file(world),
            &format!("SELECT name FROM _migratr_migrations WHERE version = {SHIPPED}")
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &place_file(world),
            "SELECT name FROM pragma_table_info('notes')"
        ),
        vec!["id", "body"]
    );
    assert!(
        migration_rows(world).is_empty(),
        "{:?}",
        migration_rows(world)
    );
}

/// parent and child, child referencing parent, as the first migration makes them. note is
/// UNIQUE, so dropping it rebuilds parent with child rather than running an in-place DROP COLUMN.
const FAMILY: &str = r#"{"up": [
  {"op": "create_table", "table": "parent", "columns": [
    {"name": "id", "type": "INTEGER", "primary_key": 1},
    {"name": "name", "type": "TEXT", "not_null": true},
    {"name": "note", "type": "TEXT", "unique": true}]},
  {"op": "create_index", "name": "parent_name", "table": "parent", "columns": ["name"]},
  {"op": "create_table", "table": "child", "columns": [
    {"name": "id", "type": "INTEGER", "primary_key": 1},
    {"name": "parent", "type": "INTEGER", "not_null": true,
     "references": {"table": "parent", "column": "id"}},
    {"name": "v", "type": "TEXT"}]},
  {"op": "create_index", "name": "child_parent", "table": "child", "columns": ["parent"]}
]}"#;

const PARENT_ROWS: &str = "SELECT id || ':' || name FROM parent ORDER BY id";
const CHILD_ROWS: &str = "SELECT id || ':' || parent || ':' || v FROM child ORDER BY id";

#[given("migratr dev against a d1:// place whose table parent is referenced by child")]
fn dev_against_d1(world: &mut World) {
    add_migration(world, "20260101000001", "family", FAMILY);
    let place = world.root.path().join("d1.db");
    let made = Command::new(env!("CARGO_BIN_EXE_migratr"))
        .arg("--db")
        .arg(&place)
        .arg("--dir")
        .arg(world.root.path().join("migrations"))
        .arg("up")
        .current_dir(&world.cwd)
        .env("HOME", world.root.path().join("home"))
        .output()
        .expect("make the place");
    assert!(made.status.success(), "{made:?}");
    let mut batch = String::new();
    for p in 1..=20 {
        batch.push_str(&format!(
            "INSERT INTO parent VALUES ({p}, 'parent {p}', 'note {p}');"
        ));
    }
    for c in 1..=50 {
        batch.push_str(&format!(
            "INSERT INTO child VALUES ({c}, {}, 'child {c}');",
            c % 20 + 1
        ));
    }
    sql(&place, &batch);
    world.env = d1::serve("shop", place.to_str().expect("UTF-8 path"));
    world.place = Some(place);
    write_settings(
        world.root.path(),
        &serde_json::json!({ "smuggl": "d1://shop", "loud": false }).to_string(),
    );
    run_dev(world);
    dev_ready(world);
    world.planned.1 = rows(&place_file(world), PARENT_ROWS);
    world.planned.1.extend(rows(&place_file(world), CHILD_ROWS));
}

#[given("a local migration that rebuilds parent")]
fn rebuild_parent(world: &mut World) {
    add_migration(
        world,
        SHIPPED,
        "drop_parent_note",
        r#"{"up": [{"op": "drop_column", "table": "parent", "column": {"name": "note", "type": "TEXT"}}]}"#,
    );
}

#[then("parent and child are rebuilt on the place")]
fn family_rebuilt(world: &mut World) {
    shipped(world);
    let place = place_file(world);
    assert_eq!(
        rows(&place, "SELECT name FROM pragma_table_info('parent')"),
        vec!["id", "name"]
    );
    assert_eq!(
        rows(
            &place,
            "SELECT \"table\" FROM pragma_foreign_key_list('child')"
        ),
        vec!["parent"]
    );
    assert!(
        rows(
            &place,
            "SELECT name FROM sqlite_master WHERE name LIKE '%migratr_new%'"
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &place,
            "SELECT name FROM sqlite_master WHERE type = 'index' AND sql IS NOT NULL ORDER BY name"
        ),
        vec!["child_parent", "parent_name"]
    );
    assert!(rows(&place, "SELECT \"table\" FROM pragma_foreign_key_check").is_empty());
    assert_eq!(
        rows(
            &place,
            &format!("SELECT name FROM _migratr_migrations WHERE version = {SHIPPED}")
        ),
        vec!["drop_parent_note"]
    );
    assert_eq!(
        fingerprint_of(world, std::ffi::OsStr::new("d1://shop")),
        fingerprint_of(world, app_db(world).as_os_str())
    );
}

#[then("every row of both is there")]
fn every_row(world: &mut World) {
    let place = place_file(world);
    let mut found = rows(&place, PARENT_ROWS);
    assert_eq!(found.len(), 20);
    let children = rows(&place, CHILD_ROWS);
    assert_eq!(children.len(), 50);
    found.extend(children);
    assert_eq!(found, world.planned.1);
}

#[given("autoclean left at its default")]
fn autoclean_default(world: &mut World) {
    let settings =
        std::fs::read_to_string(world.root.path().join("migratr.jsonc")).expect("settings");
    assert!(!settings.contains("autoclean"), "{settings}");
}

#[then("the ledger holds no rewind-point row")]
fn no_rewind_row(world: &mut World) {
    shipped(world);
    assert!(rewind_rows(world).is_empty());
}

#[then("the fingerprint row holds the place's new fingerprint")]
fn fingerprint_row_new(world: &mut World) {
    let found = rows(
        &app_db(world),
        "SELECT checksum FROM _migratr_migrations WHERE version = 1 AND name = 'fingerprint'",
    );
    assert_eq!(found.len(), 1, "{found:?}");
    let note: Value = serde_json::from_str(&found[0]).expect("JSON");
    let now = place_fingerprint(world);
    assert_eq!(note["fingerprint"].as_str(), Some(now.as_str()));
    assert_eq!(
        note["migrations"],
        serde_json::json!([SHIPPED.parse::<u64>().expect("version")])
    );
}

/// The migration versions in the ledger of the database or place `address`, as smugglr exec's
/// read form gives them, without migratr dev's two notes.
fn ledger_of(world: &World, address: &std::ffi::OsStr) -> Vec<u64> {
    read_of(world, address)["ledger"]
        .as_array()
        .expect("a ledger")
        .iter()
        .filter_map(|row| row["version"].as_u64())
        .filter(|v| *v > 1)
        .collect()
}

/// FR-MIGRATR-024, migratr#91: a ship whose second migration fails on the place after the first
/// reached it finishes when run again, once the cause is fixed.
#[rstest::rstest]
fn a_ship_that_failed_partway_finishes_when_run_again(mut world: World) {
    // b is in smugglr's ignore list, so its rows stay on the place: the duplicate there breaks
    // the second migration's unique index on the place alone.
    let config = world.root.path().join("cfg").join("smugglr");
    std::fs::create_dir_all(&config).expect("config dir");
    std::fs::write(config.join("config.toml"), "ignore = [\"b\"]\n").expect("config");
    dev_against(
        &mut world,
        &format!(
            "{NOTES_PLACE} CREATE TABLE b (id TEXT PRIMARY KEY, v TEXT); \
             INSERT INTO b VALUES ('3', 'z'), ('4', 'z');"
        ),
    );
    add_tag(&world);
    add_migration(
        &world,
        "20260202000000",
        "unique_b",
        r#"{"up": [{"op": "create_index", "name": "b_v", "table": "b", "columns": ["v"], "unique": true}]}"#,
    );

    runs_up_smuggl(&mut world);
    assert_ne!(world.exit, Some(0), "{}", world.stdout);
    place_holds_shipped(&world);
    assert_eq!(ledger_of(&world, place_file(&world).as_os_str()).len(), 1);

    sql(&place_file(&world), "DELETE FROM b WHERE id = '4';");
    next_second();
    runs_up_smuggl(&mut world);
    shipped(&world);
    assert!(
        world.stdout.contains("shipped 1 migration(s)"),
        "{}",
        world.stdout
    );
    fingerprints_equal(&mut world);
    let place = ledger_of(&world, place_file(&world).as_os_str());
    assert_eq!(place, vec![20_260_201_000_000, 20_260_202_000_000]);
    assert_eq!(place, ledger_of(&world, app_db(&world).as_os_str()));
}

/// FR-MIGRATR-024, migratr#82: a down whose second revert fails on the place after the first
/// reached it finishes when run again, once the cause is fixed.
#[rstest::rstest]
fn a_down_that_failed_partway_finishes_when_run_again(mut world: World) {
    // b is in smugglr's ignore list, so its rows stay on the place: a duplicate there breaks the
    // unique index the second revert puts back, on the place alone.
    let config = world.root.path().join("cfg").join("smugglr");
    std::fs::create_dir_all(&config).expect("config dir");
    std::fs::write(config.join("config.toml"), "ignore = [\"b\"]\n").expect("config");
    dev_against(
        &mut world,
        &format!(
            "{NOTES_PLACE} CREATE TABLE b (id TEXT PRIMARY KEY, v TEXT); \
             INSERT INTO b VALUES ('3', 'z');"
        ),
    );
    add_migration(
        &world,
        "20260201000000",
        "unique_b",
        r#"{"up": [{"op": "create_index", "name": "b_v", "table": "b", "columns": ["v"], "unique": true}]}"#,
    );
    add_migration(
        &world,
        "20260202000000",
        "drop_b_v",
        r#"{"up": [{"op": "drop_index", "definition": {"name": "b_v", "table": "b", "columns": ["v"], "unique": true}}]}"#,
    );
    add_migration(
        &world,
        "20260203000000",
        "add_tag",
        r#"{"up": [{"op": "add_column", "table": "notes", "column": {"name": "tag", "type": "TEXT"}}]}"#,
    );
    next_second();
    runs_up_smuggl(&mut world);
    shipped(&world);
    next_second();
    run_dev(&mut world);
    dev_ready(&world);

    sql(&place_file(&world), "INSERT INTO b VALUES ('4', 'z');");
    next_second();
    run_smuggl(&mut world, &[&["down", "--smuggl", "--steps", "2"]]);
    assert_ne!(world.exit, Some(0), "{}", world.stdout);
    assert_eq!(
        ledger_of(&world, place_file(&world).as_os_str()),
        vec![20_260_201_000_000, 20_260_202_000_000]
    );

    sql(&place_file(&world), "DELETE FROM b WHERE id = '4';");
    next_second();
    run_smuggl(
        &mut world,
        &[&["down", "--smuggl", "--steps", "2", "--discover"]],
    );
    assert_eq!(world.exit, Some(0), "{}{}", world.stdout, world.stderr);
    assert!(world.stdout.contains("20260202000000"), "{}", world.stdout);
    assert!(!world.stdout.contains("20260203000000"), "{}", world.stdout);
    run_smuggl(&mut world, &[&["down", "--smuggl", "--steps", "2"]]);
    shipped(&world);
    assert!(
        world.stdout.contains("shipped 1 migration(s)"),
        "{}",
        world.stdout
    );
    fingerprints_equal(&mut world);
    let place = ledger_of(&world, place_file(&world).as_os_str());
    assert_eq!(place, vec![20_260_201_000_000]);
    assert_eq!(place, ledger_of(&world, app_db(&world).as_os_str()));
}

/// FR-MIGRATR-024, migratr#82: with autoclean off, a down from a rewind point older than the
/// place stops before anything changes, rather than revert there a migration that is not the
/// latest, and goes ahead after migratr dev again.
#[rstest::rstest]
fn a_down_from_a_rewind_point_behind_the_place_stops(mut world: World) {
    dev_against(&mut world, NOTES_PLACE);
    let place = place_file(&world);
    write_settings(
        world.root.path(),
        &serde_json::json!({ "smuggl": place, "loud": false, "autoclean": false }).to_string(),
    );
    add_tag(&world);
    next_second();
    runs_up_smuggl(&mut world);
    shipped(&world);
    next_second();
    run_dev(&mut world);
    dev_ready(&world);
    add_migration(
        &world,
        "20260202000000",
        "index_body",
        r#"{"up": [{"op": "create_index", "name": "notes_body", "table": "notes", "columns": ["body"]}]}"#,
    );
    next_second();
    runs_up_smuggl(&mut world);
    shipped(&world);
    sql(&place, "UPDATE notes SET tag = 'keep';");

    next_second();
    run_smuggl(
        &mut world,
        &[&["down", "--smuggl", "--discover"], &["down", "--smuggl"]],
    );
    assert_eq!(world.runs[0].0, Some(3), "{}", world.stdouts[0]);
    assert!(
        world.stdouts[0].contains("run migratr dev again"),
        "{}",
        world.stdouts[0]
    );
    assert_ne!(world.exit, Some(0), "{}", world.stdout);
    assert!(
        world.stderr.contains("run migratr dev again") && world.stderr.contains("nothing changed"),
        "{}",
        world.stderr
    );
    unchanged(&world);
    assert_eq!(
        ledger_of(&world, place.as_os_str()),
        vec![20_260_201_000_000, 20_260_202_000_000]
    );

    next_second();
    run_dev(&mut world);
    dev_ready(&world);
    next_second();
    run_smuggl(&mut world, &[&["down", "--smuggl"]]);
    shipped(&world);
    assert_eq!(
        ledger_of(&world, place.as_os_str()),
        vec![20_260_201_000_000]
    );
    assert_eq!(rows(&place, "SELECT tag FROM notes LIMIT 1"), vec!["keep"]);
}

#[given("a place file with an AUTOINCREMENT table")]
fn place_with_autoincrement(world: &mut World) {
    fresh_place(
        world,
        "CREATE TABLE counter (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);
         INSERT INTO counter (v) VALUES ('x');",
    );
}

#[then("the output names the AUTOINCREMENT table")]
fn names_autoincrement_table(world: &mut World) {
    assert_eq!(world.exit, Some(1), "{}", world.stderr);
    assert!(
        world
            .stderr
            .contains("the place has AUTOINCREMENT in table counter"),
        "{}",
        world.stderr
    );
}

#[given("instead a place file with an FTS5 table holding two rows")]
fn place_with_fts5(world: &mut World) {
    fresh_place(
        world,
        "CREATE VIRTUAL TABLE docs USING fts5(body);
         INSERT INTO docs (body) VALUES ('first note'), ('second note');",
    );
}

#[then("the local database holds the FTS5 table and its two rows")]
fn holds_fts5_rows(world: &mut World) {
    dev_ready(world);
    assert_eq!(
        rows(&app_db(world), "SELECT body FROM docs ORDER BY body"),
        vec!["first note", "second note"]
    );
    assert_eq!(
        rows(
            &app_db(world),
            "SELECT body FROM docs WHERE docs MATCH 'second'"
        ),
        vec!["second note"]
    );
}

#[given("a local database after migratr dev")]
fn a_local_after_dev(world: &mut World) {
    ensure_place(world);
    // At the top of a fresh second, so the runs after it fall in the same second.
    next_second();
    run_dev(world);
    dev_ready(world);
}

/// The local database's bytes and its rewind-point rows.
fn local_state(world: &World) -> (Option<Vec<u8>>, Vec<String>) {
    let db = app_db(world);
    let bytes = std::fs::read(&db).ok();
    let points = rows(
        &db,
        "SELECT checksum FROM _migratr_migrations WHERE version = 0 AND name = 'rewind_point'",
    );
    (bytes, points)
}

#[when("the agent runs migratr dev twice in the same second")]
fn runs_dev_twice(world: &mut World) {
    for _ in 0..2 {
        let before = local_state(world);
        let output = dev_command(world, &smugglr_bin(), None, &[])
            .output()
            .expect("run migratr dev");
        let after = local_state(world);
        world.dev_runs.push(DevRun {
            exit: output.status.code(),
            output: format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            before,
            after,
        });
    }
}

#[then(
    "both runs end with Dev ready, or the one that fails leaves the local database's hashes and its ledger's rewind-point row as they were before it"
)]
fn failed_run_leaves_local(world: &mut World) {
    assert_eq!(world.dev_runs.len(), 2);
    for run in &world.dev_runs {
        if run.exit == Some(0) {
            assert_eq!(
                run.output.trim_end().lines().last(),
                Some("Dev ready"),
                "{}",
                run.output
            );
        } else {
            assert!(
                run.after.0 == run.before.0,
                "a failed dev changed the local database: {}",
                run.output
            );
            assert_eq!(run.after.1, run.before.1, "{}", run.output);
        }
    }
}

// Tests of FR-MIGRATR-023's criteria beyond the generated scenarios.

/// Every migratr ledger row, every column.
fn ledger_rows(world: &World) -> Vec<String> {
    rows(
        &app_db(world),
        "SELECT json_array(version, name, checksum, applied_at, up_hash, down_hash) \
         FROM _migratr_migrations ORDER BY version",
    )
}

/// Every file of the pair's memory, with its bytes.
fn pair_files(world: &World) -> Vec<(PathBuf, Vec<u8>)> {
    tree(&world.root.path().join(".migratr").join("pair"))
        .into_iter()
        .filter(|p| p.is_file())
        .map(|p| {
            let bytes = std::fs::read(&p).expect("pair file");
            (p, bytes)
        })
        .collect()
}

#[cfg(unix)]
#[test]
fn a_dev_whose_remember_fails_puts_the_old_local_database_and_memory_back() {
    use std::os::unix::fs::PermissionsExt;

    let mut world = world();
    ensure_place(&mut world);
    run_dev(&mut world);
    dev_ready(&world);
    sql(&app_db(&world), "INSERT INTO a VALUES ('3', 'local');");
    let before = world.root.path().join("before.db");
    sql(
        &app_db(&world),
        &format!("VACUUM INTO '{}'", before.display()),
    );
    let ledger = ledger_rows(&world);
    let pair = pair_files(&world);
    assert!(!pair.is_empty());
    let rewinds = snapshots(&world, "_dev.db");

    // A smugglr that fails only the sync that makes the pair's memory, after the rename.
    let wrapper = world.root.path().join("smugglr-failing-remember");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \"$XDG_CONFIG_HOME\" in */.migratr/pair) \
             echo 'remember refused' >&2; exit 1;; esac\nexec '{}' \"$@\"\n",
            smugglr_bin()
        ),
    )
    .expect("wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let output = dev_command(&world, wrapper.to_str().expect("utf-8"), None, &[])
        .output()
        .expect("run migratr dev");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("remember refused"), "{stderr}");

    in_step(&world, &before);
    assert_eq!(
        rows(&app_db(&world), "SELECT id || ':' || v FROM a ORDER BY id"),
        vec!["1:x", "2:y", "3:local"]
    );
    assert_eq!(ledger_rows(&world), ledger);
    assert_eq!(pair_files(&world), pair);
    assert_eq!(snapshots(&world, "_dev.db"), rewinds);
}

#[test]
fn dev_brings_a_table_with_a_generated_virtual_column() {
    let mut world = world();
    make_place(
        &mut world,
        "CREATE TABLE g (id TEXT PRIMARY KEY, a INTEGER, \
         b INTEGER GENERATED ALWAYS AS (a * 2) VIRTUAL); \
         INSERT INTO g (id, a) VALUES ('1', 1), ('2', 5);",
    );
    name_place(&world);
    run_dev(&mut world);
    dev_ready(&world);
    assert_eq!(
        rows(
            &app_db(&world),
            "SELECT id || ':' || a || ':' || b FROM g ORDER BY id"
        ),
        vec!["1:1:2", "2:5:10"]
    );
}

scenarios!("tests", fixtures = [world: World]);
