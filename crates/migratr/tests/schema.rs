use std::fs;
use std::path::Path;

use migratr::{Executor, RusqliteExecutor, down, load_dir, scaffold, up, write_schema};
use rusqlite::Connection;
use tempfile::TempDir;

mod common;
use common::{FailAt, dump};

fn executor() -> RusqliteExecutor {
    RusqliteExecutor::new(Connection::open_in_memory().expect("open"))
}

fn write_migration(dir: &Path, file: &str, ops: &str) {
    fs::write(dir.join(file), format!(r#"{{"up": [{ops}]}}"#)).expect("write migration");
}

fn create_table(name: &str) -> String {
    format!(
        r#"{{"op": "create_table", "table": "{name}", "columns": [{{"name": "id", "type": "INTEGER", "primary_key": 1}}]}}"#
    )
}

fn migrations_dir() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    write_migration(dir.path(), "20260101000001_one.json", &create_table("one"));
    write_migration(dir.path(), "20260101000002_two.json", &create_table("two"));
    dir
}

fn schema_json(dir: &Path) -> String {
    fs::read_to_string(dir.join("schema.json")).expect("schema.json")
}

fn migrate_up(ex: &mut impl Executor, dir: &Path) {
    let migrations = load_dir(dir).expect("load");
    up(ex, &migrations, None).expect("up");
    write_schema(ex, dir).expect("write schema");
}

#[test]
fn the_same_migrations_on_two_empty_databases_write_identical_bytes() {
    let (a, b) = (migrations_dir(), migrations_dir());
    migrate_up(&mut executor(), a.path());
    migrate_up(&mut executor(), b.path());
    assert_eq!(schema_json(a.path()), schema_json(b.path()));
}

#[test]
fn schema_json_follows_up_and_down() {
    let dir = migrations_dir();
    let mut ex = executor();
    migrate_up(&mut ex, dir.path());
    let head = schema_json(dir.path());
    assert!(head.contains("\"version\": 20260101000002"));
    assert!(head.contains("\"name\": \"one\"") && head.contains("\"name\": \"two\""));

    // Writing again with nothing changed leaves the bytes alone.
    write_schema(&mut ex, dir.path()).expect("rewrite");
    assert_eq!(schema_json(dir.path()), head);

    let migrations = load_dir(dir.path()).expect("load");
    down(&mut ex, &migrations, 1).expect("down");
    write_schema(&mut ex, dir.path()).expect("write after down");
    let after = schema_json(dir.path());
    assert!(after.contains("\"version\": 20260101000001"));
    assert!(!after.contains("\"name\": \"two\""));
}

#[test]
fn a_failed_up_leaves_schema_json_unchanged() {
    let dir = migrations_dir();
    let mut ex = executor();
    let migrations = load_dir(dir.path()).expect("load");
    up(&mut ex, &migrations[..1], None).expect("first");
    write_schema(&mut ex, dir.path()).expect("write");
    let before = schema_json(dir.path());
    let database = dump(ex.connection());

    let mut failing = FailAt {
        inner: ex,
        fail_at: 1,
        statement_count: 0,
    };
    assert!(up(&mut failing, &migrations, None).is_err());
    assert_eq!(dump(failing.inner.connection()), database);
    assert_eq!(schema_json(dir.path()), before);
}

#[test]
fn a_drop_scaffolded_from_schema_json_reverses_with_the_table_intact() {
    let dir = TempDir::new().expect("tempdir");
    write_migration(
        dir.path(),
        "20200101000000_create_people.json",
        r#"{"op": "create_table", "table": "people", "columns": [
            {"name": "id", "type": "INTEGER", "primary_key": 1},
            {"name": "email", "type": "TEXT", "not_null": true, "collation": "NOCASE", "default": "'x'"}]}"#,
    );
    let mut ex = executor();
    migrate_up(&mut ex, dir.path());

    scaffold(dir.path(), "remove_email_from_people", &[]).expect("scaffold drop");
    let migrations = load_dir(dir.path()).expect("load");
    up(&mut ex, &migrations, None).expect("drop the column");
    down(&mut ex, &migrations, 1).expect("restore it");
    write_schema(&mut ex, dir.path()).expect("write");

    let restored = schema_json(dir.path());
    assert!(restored.contains("email"));
    assert!(restored.contains("NOCASE"));
}
