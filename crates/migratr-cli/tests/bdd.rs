//! Runs every `.feature` file under this crate's `tests/` directory.
//!
//! A scenario with no matching step code fails `cargo test`, and the failure
//! names the scenario.

use std::path::{Path, PathBuf};
use std::process::Command;

use rstest_bdd_macros::{given, scenarios, then, when};
use serde_json::Value;
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

scenarios!("tests", fixtures = [world: World]);
