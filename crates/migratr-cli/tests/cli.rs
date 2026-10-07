use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

struct Project {
    root: TempDir,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The whole of stdout as one JSON document.
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not one JSON document ({e}): {}", self.stdout))
    }
}

impl Project {
    fn new() -> Self {
        let root = TempDir::new().expect("tempdir");
        fs::create_dir(root.path().join("migrations")).expect("migrations dir");
        Self { root }
    }

    fn db(&self) -> PathBuf {
        self.root.path().join("app.db")
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("migrations")
    }

    fn schema(&self) -> Option<String> {
        fs::read_to_string(self.dir().join("schema.json")).ok()
    }

    fn run(&self, args: &[&str]) -> Run {
        let output = Command::new(env!("CARGO_BIN_EXE_migratr"))
            .arg("--db")
            .arg(self.db())
            .arg("--dir")
            .arg(self.dir())
            .args(args)
            .output()
            .expect("run migratr");
        Run {
            code: output.status.code().expect("exit code"),
            stdout: String::from_utf8(output.stdout).expect("utf8 stdout"),
            stderr: String::from_utf8(output.stderr).expect("utf8 stderr"),
        }
    }

    /// Scaffolds a migration and returns its path.
    fn new_migration(&self, args: &[&str]) -> PathBuf {
        let mut full = vec!["--json", "new"];
        full.extend(args);
        let run = self.run(&full);
        assert_eq!(run.code, 0, "{}", run.stdout);
        PathBuf::from(run.json()["path"].as_str().expect("path").to_string())
    }
}

fn files_under(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path.to_string_lossy().into_owned());
        }
    }
    found.sort();
    found
}

#[test]
fn every_command_writes_one_json_document_on_success_and_on_failure() {
    let p = Project::new();

    let ok_runs: Vec<(&str, Vec<&str>)> = vec![
        ("new", vec!["new", "create_users", "id:integer:pk"]),
        ("plan", vec!["plan"]),
        ("up", vec!["up"]),
        ("status", vec!["status"]),
        ("down", vec!["down"]),
    ];
    for (command, args) in ok_runs {
        let mut full = vec!["--json"];
        full.extend(args);
        let run = p.run(&full);
        assert_eq!(run.code, 0, "{command}: {}", run.stdout);
        let doc = run.json();
        assert_eq!(doc["command"], command);
        assert_eq!(doc["status"], "ok");
    }

    let failing: Vec<(&str, Vec<&str>)> = vec![
        ("new", vec!["new", "create_users"]),
        ("plan", vec!["plan", "down", "--steps", "1"]),
        ("up", vec!["up", "--to", "1"]),
        ("status", vec!["status"]),
        ("down", vec!["down"]),
        ("restore", vec!["restore"]),
    ];
    // Break the directory so every command that reads it fails.
    fs::write(p.dir().join("20990101000000_bad.json"), "not json").expect("write bad");
    for (command, args) in failing {
        let mut full = vec!["--json"];
        full.extend(args);
        let run = p.run(&full);
        assert_ne!(run.code, 0, "{command} should fail");
        let doc = run.json();
        assert_eq!(doc["command"], command);
        assert_eq!(doc["status"], "error");
        assert!(doc["code"].as_str().is_some_and(|c| !c.is_empty()));
        assert!(doc["message"].as_str().is_some_and(|m| !m.is_empty()));
    }
}

#[test]
fn a_usage_error_under_json_is_one_document_with_exit_code_two() {
    let p = Project::new();
    let run = p.run(&["--json", "up", "--to", "not-a-number"]);
    assert_eq!(run.code, 2);
    let doc = run.json();
    assert_eq!(doc["command"], "up");
    assert_eq!(doc["code"], "usage");
}

#[test]
fn logs_and_errors_stay_off_stdout_without_json() {
    let p = Project::new();
    fs::write(p.dir().join("20990101000000_bad.json"), "not json").expect("write bad");
    let run = p.run(&["up"]);
    assert_eq!(run.code, 1);
    assert!(run.stdout.is_empty());
    assert!(run.stderr.contains("error [parse]"), "{}", run.stderr);
}

#[test]
fn up_and_down_write_schema_json_and_a_failed_run_leaves_it_alone() {
    let p = Project::new();
    p.new_migration(&["create_users", "id:integer:pk"]);
    assert!(p.schema().is_none());

    assert_eq!(p.run(&["up"]).code, 0);
    let after_up = p.schema().expect("schema.json after up");
    assert!(after_up.contains("CREATE TABLE"), "{after_up}");

    assert_eq!(p.run(&["down"]).code, 0);
    let after_down = p.schema().expect("schema.json after down");
    assert_ne!(after_up, after_down);

    // A migration whose SQL fails rolls back and leaves schema.json as it was.
    fs::write(
        p.dir().join("20990101000000_broken.json"),
        r#"{"up":[{"raw_sql":{"up":"THIS IS NOT SQL"}}]}"#,
    )
    .expect("write broken");
    let failed = p.run(&["--json", "up"]);
    assert_eq!(failed.code, 1, "{}", failed.stdout);
    assert_eq!(p.schema().as_deref(), Some(after_down.as_str()));
}

#[test]
fn plan_prints_the_sql_up_runs_and_changes_nothing() {
    let p = Project::new();
    p.new_migration(&["create_users", "id:integer:pk", "name:text"]);

    let plan = p.run(&["--json", "plan"]).json();
    let statements: Vec<&str> = plan["steps"][0]["statements"]
        .as_array()
        .expect("statements")
        .iter()
        .map(|s| s.as_str().expect("statement"))
        .collect();
    assert_eq!(plan["steps"][0]["destructive"], false);
    assert!(!p.db().exists(), "plan created the database");
    assert!(p.schema().is_none());

    assert_eq!(p.run(&["up"]).code, 0);
    let schema = p.schema().expect("schema.json");
    let create = statements
        .iter()
        .find(|s| s.starts_with("CREATE TABLE \"users\""))
        .expect("plan has the CREATE TABLE");
    let stored = create.replace('\\', "\\\\").replace('"', "\\\"");
    assert!(
        schema.contains(&stored),
        "up stored different SQL than plan printed:\n{create}\n{schema}"
    );
}

#[test]
fn plan_leaves_database_snapshots_and_schema_json_unchanged() {
    let p = Project::new();
    p.new_migration(&["create_users", "id:integer:pk"]);
    assert_eq!(p.run(&["up"]).code, 0);
    p.new_migration(&["drop_users"]);

    let db_before = fs::read(p.db()).expect("db");
    let schema_before = p.schema();
    let files_before = files_under(p.root.path());

    let plan = p.run(&["--json", "plan"]).json();
    let step = &plan["steps"][0];
    assert_eq!(step["destructive"], true);
    assert!(
        step["snapshot"]
            .as_str()
            .is_some_and(|s| s.ends_with("_up.db")),
        "{step}"
    );
    assert!(plan["database_bytes"].as_u64().is_some_and(|b| b > 0));

    assert_eq!(fs::read(p.db()).expect("db"), db_before);
    assert_eq!(p.schema(), schema_before);
    assert_eq!(files_under(p.root.path()), files_before);
}

#[test]
fn plan_down_reports_the_reverted_steps() {
    let p = Project::new();
    p.new_migration(&["create_users", "id:integer:pk"]);
    assert_eq!(p.run(&["up"]).code, 0);

    let plan = p.run(&["--json", "plan", "down"]).json();
    assert_eq!(plan["direction"], "down");
    let statements = plan["steps"][0]["statements"].to_string();
    assert!(statements.contains("DROP TABLE"), "{statements}");
    assert_eq!(plan["steps"][0]["destructive"], true);
}

#[test]
fn status_reports_applied_pending_mismatch_and_missing() {
    let p = Project::new();
    let first = p.new_migration(&["create_users", "id:integer:pk"]);
    p.new_migration(&["create_posts", "id:integer:pk"]);
    p.new_migration(&["create_tags", "id:integer:pk"]);
    assert_eq!(p.run(&["up", "--to", &version_of(&p, 1)]).code, 0);

    let doc = p.run(&["--json", "status"]).json();
    let states: Vec<&str> = doc["migrations"]
        .as_array()
        .expect("migrations")
        .iter()
        .map(|m| m["state"].as_str().expect("state"))
        .collect();
    assert_eq!(states, ["applied", "applied", "pending"]);
    assert!(doc["migrations"][0]["applied_at"].is_string());
    assert!(doc["migrations"][2]["applied_at"].is_null());
    assert_eq!(doc["migrations"][0]["checksum_mismatch"], false);

    let text = fs::read_to_string(&first).expect("read");
    fs::write(&first, text.replace("\"id\"", "\"ident\"")).expect("edit");
    let edited = p.run(&["--json", "status"]).json();
    assert_eq!(edited["status"], "ok", "{edited}");
    assert_eq!(edited["migrations"][0]["checksum_mismatch"], true);

    fs::remove_file(&first).expect("remove");
    let gone = p.run(&["--json", "status"]).json();
    assert_eq!(gone["missing"][0]["version"], version_number(&first));
}

#[test]
fn down_says_data_is_not_restored_and_points_at_the_snapshot() {
    let p = Project::new();
    p.new_migration(&["create_users", "id:integer:pk"]);
    assert_eq!(p.run(&["up"]).code, 0);

    let run = p.run(&["down"]);
    assert_eq!(run.code, 0);
    assert!(run.stdout.contains("not restored"), "{}", run.stdout);
    assert!(run.stdout.contains("snapshot"), "{}", run.stdout);

    let doc = p.run(&["--json", "status"]).json();
    assert!(doc["latest_snapshot"].is_string());
}

#[test]
fn restore_needs_yes_and_then_replaces_the_database() {
    let p = Project::new();
    p.new_migration(&["create_users", "id:integer:pk"]);
    assert_eq!(p.run(&["up"]).code, 0);
    p.new_migration(&["drop_users"]);
    assert_eq!(p.run(&["up"]).code, 0);

    let refused = p.run(&["--json", "restore"]);
    assert_eq!(refused.code, 1);
    assert_eq!(refused.json()["code"], "restore_not_confirmed");

    let done = p.run(&["--json", "restore", "--yes"]);
    assert_eq!(done.code, 0, "{}", done.stdout);
    let doc = done.json();
    assert_eq!(doc["restored"], true);
    assert_eq!(doc["removed"].as_array().map(Vec::len), Some(1));
}

fn version_of(p: &Project, index: usize) -> String {
    let doc = p.run(&["--json", "status"]).json();
    doc["migrations"][index]["version"].to_string()
}

fn version_number(path: &Path) -> u64 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split('_').next())
        .and_then(|v| v.parse().ok())
        .expect("version in file name")
}
