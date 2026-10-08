//! Runs every `.feature` file under this crate's `tests/` directory.
//!
//! A scenario with no matching step code fails `cargo test`, and the failure
//! names the scenario.

use std::process::Command;

use rstest_bdd_macros::{given, scenarios, then, when};
use serde_json::Value;
use tempfile::TempDir;

struct World {
    root: TempDir,
    exit: Option<i32>,
    stdout: String,
}

#[rstest::fixture]
fn world() -> World {
    let root = TempDir::new().expect("tempdir");
    std::fs::create_dir(root.path().join("migrations")).expect("migrations dir");
    World {
        root,
        exit: None,
        stdout: String::new(),
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

scenarios!("tests", fixtures = [world: World]);
