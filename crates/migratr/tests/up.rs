use std::fs;
use std::path::Path;

use migratr::{
    Executor, LedgerRow, MigrateError, RusqliteExecutor, SchemaSnapshot, UpReport, load_dir, up,
};
use rusqlite::Connection;
use tempfile::TempDir;

mod common;
use common::{FailAt, dump};

fn write(dir: &Path, file: &str, ops: &str) {
    fs::write(dir.join(file), format!(r#"{{"up": [{ops}]}}"#)).expect("write migration");
}

fn executor() -> RusqliteExecutor {
    RusqliteExecutor::new(Connection::open_in_memory().expect("open"))
}

fn create_table(name: &str) -> String {
    format!(
        r#"{{"op": "create_table", "table": "{name}", "columns": [{{"name": "id", "type": "INTEGER", "primary_key": 1}}]}}"#
    )
}

fn ledger_versions(ex: &mut RusqliteExecutor) -> Vec<u64> {
    ex.read_ledger()
        .expect("ledger")
        .iter()
        .map(|r| r.version)
        .collect()
}

fn table_names(ex: &mut RusqliteExecutor) -> Vec<String> {
    let snap: SchemaSnapshot = ex.read_schema().expect("schema");
    snap.objects
        .into_iter()
        .filter(|o| o.kind == "table")
        .map(|o| o.name)
        .collect()
}

fn three_migrations(dir: &Path) {
    write(dir, "20260101000001_one.json", &create_table("one"));
    write(dir, "20260101000002_two.json", &create_table("two"));
    write(dir, "20260101000003_three.json", &create_table("three"));
}

#[test]
fn three_pending_apply_in_order_with_three_ledger_rows() {
    let dir = TempDir::new().expect("tempdir");
    three_migrations(dir.path());
    let migrations = load_dir(dir.path()).expect("load");
    let mut ex = executor();

    let report = up(&mut ex, &migrations, None).expect("up");

    assert_eq!(
        report,
        UpReport {
            applied: vec![20260101000001, 20260101000002, 20260101000003]
        }
    );
    assert_eq!(
        ledger_versions(&mut ex),
        vec![20260101000001, 20260101000002, 20260101000003]
    );
    let mut tables = table_names(&mut ex);
    tables.sort();
    assert_eq!(tables, ["_migratr_migrations", "one", "three", "two"]);
}

#[test]
fn ledger_rows_carry_name_checksum_and_time() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_one.json", &create_table("one"));
    let migrations = load_dir(dir.path()).expect("load");
    let mut ex = executor();
    up(&mut ex, &migrations, None).expect("up");

    assert_eq!(
        ex.read_ledger().expect("ledger"),
        vec![LedgerRow {
            version: 20260101000001,
            name: "one".into(),
            checksum: migrations[0].checksum.clone(),
        }]
    );
    let applied_at: String = ex
        .connection()
        .query_row("SELECT applied_at FROM _migratr_migrations", [], |r| {
            r.get(0)
        })
        .expect("applied_at");
    assert!(
        applied_at.ends_with('Z') && applied_at.contains('T'),
        "{applied_at}"
    );
}

#[test]
fn rerunning_up_applies_nothing() {
    let dir = TempDir::new().expect("tempdir");
    three_migrations(dir.path());
    let migrations = load_dir(dir.path()).expect("load");
    let mut ex = executor();
    up(&mut ex, &migrations, None).expect("first");

    let report = up(&mut ex, &migrations, None).expect("second");

    assert!(report.applied.is_empty());
    assert_eq!(ledger_versions(&mut ex).len(), 3);
}

#[test]
fn up_to_stops_after_that_version() {
    let dir = TempDir::new().expect("tempdir");
    three_migrations(dir.path());
    let migrations = load_dir(dir.path()).expect("load");
    let mut ex = executor();

    let report = up(&mut ex, &migrations, Some(20260101000002)).expect("up");

    assert_eq!(report.applied, vec![20260101000001, 20260101000002]);
    assert_eq!(
        ledger_versions(&mut ex),
        vec![20260101000001, 20260101000002]
    );
    let report = up(&mut ex, &migrations, None).expect("rest");
    assert_eq!(report.applied, vec![20260101000003]);
}

#[test]
fn failure_in_the_second_keeps_the_first_and_never_tries_the_third() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_one.json", &create_table("one"));
    write(
        dir.path(),
        "20260101000002_two.json",
        &format!(
            r#"{},{{"op": "raw_sql", "up": "NOT VALID SQL"}}"#,
            create_table("two")
        ),
    );
    write(
        dir.path(),
        "20260101000003_three.json",
        &create_table("three"),
    );
    let migrations = load_dir(dir.path()).expect("load");
    let mut ex = executor();

    let err = up(&mut ex, &migrations, None).expect_err("second fails");

    assert!(matches!(
        err,
        MigrateError::Apply {
            version: 20260101000002,
            statement: Some(ref statement),
            operation: Some(1),
            ..
        } if statement == "NOT VALID SQL"
    ));
    assert_eq!(ledger_versions(&mut ex), vec![20260101000001]);
    let mut tables = table_names(&mut ex);
    tables.sort();
    assert_eq!(tables, ["_migratr_migrations", "one"]);
}

#[test]
fn every_operation_applies_in_place() {
    let dir = TempDir::new().expect("tempdir");
    write(
        dir.path(),
        "20260101000001_all.json",
        r#"
        {"op": "create_table", "table": "users", "columns": [
          {"name": "id", "type": "INTEGER", "primary_key": 1},
          {"name": "email", "type": "TEXT", "not_null": true, "collation": "NOCASE"}]},
        {"op": "rename_table", "from": "users", "to": "people"},
        {"op": "add_column", "table": "people", "column":
          {"name": "age", "type": "INTEGER", "default": "0", "check": "age >= 0"}},
        {"op": "rename_column", "table": "people", "from": "age", "to": "years"},
        {"op": "create_index", "name": "people_email", "table": "people", "columns": ["email"], "unique": true},
        {"op": "drop_index", "definition":
          {"name": "people_email", "table": "people", "columns": ["email"], "unique": true}},
        {"op": "drop_column", "table": "people", "column": {"name": "years", "type": "INTEGER"}},
        {"op": "raw_sql", "up": "INSERT INTO people (email) VALUES ('a@b.c')"},
        {"op": "create_table", "table": "scratch", "columns": [{"name": "x", "type": "TEXT"}]},
        {"op": "drop_table", "table": "scratch", "definition": {
          "name": "scratch", "columns": [{"name": "x", "type": "TEXT"}],
          "sql": "CREATE TABLE scratch (x TEXT)"}}
        "#,
    );
    let migrations = load_dir(dir.path()).expect("load");
    let mut ex = executor();

    up(&mut ex, &migrations, None).expect("up");

    let mut tables = table_names(&mut ex);
    tables.sort();
    assert_eq!(tables, ["_migratr_migrations", "people"]);
    let columns: Vec<String> = ex
        .read_schema()
        .expect("schema")
        .tables
        .into_iter()
        .find(|t| t.name == "people")
        .expect("people")
        .columns
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(columns, ["id", "email"]);
}

#[test]
fn editing_an_applied_file_refuses_the_run_naming_it() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_one.json", &create_table("one"));
    let mut ex = executor();
    up(&mut ex, &load_dir(dir.path()).expect("load"), None).expect("up");

    write(dir.path(), "20260101000001_one.json", &create_table("uno"));
    write(dir.path(), "20260101000002_two.json", &create_table("two"));
    let err = up(&mut ex, &load_dir(dir.path()).expect("load"), None).expect_err("refused");

    assert!(matches!(
        err,
        MigrateError::ChecksumMismatch { version: 20260101000001, ref name } if name == "one"
    ));
    let message = err.to_string();
    assert!(message.contains("20260101000001_one"), "{message}");
    assert!(
        message.contains("Applied migrations are not edited"),
        "{message}"
    );
    assert!(
        message.contains(
            "restore the file to what was applied, then write a new migration for the change"
        ),
        "{message}"
    );
    assert_eq!(ledger_versions(&mut ex), vec![20260101000001]);
}

#[test]
fn a_missing_file_refuses_the_run_naming_it() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_one.json", &create_table("one"));
    let mut ex = executor();
    up(&mut ex, &load_dir(dir.path()).expect("load"), None).expect("up");

    fs::remove_file(dir.path().join("20260101000001_one.json")).expect("remove");
    write(dir.path(), "20260101000002_two.json", &create_table("two"));
    let err = up(&mut ex, &load_dir(dir.path()).expect("load"), None).expect_err("refused");

    assert!(matches!(
        err,
        MigrateError::MissingFile { version: 20260101000001, ref name } if name == "one"
    ));
}

#[test]
fn failure_at_every_statement_position_leaves_the_database_untouched() {
    let dir = TempDir::new().expect("tempdir");
    write(
        dir.path(),
        "20260101000001_multi.json",
        r#"
        {"op": "create_table", "table": "extra", "columns": [{"name": "id", "type": "INTEGER", "primary_key": 1}]},
        {"op": "add_column", "table": "base", "column": {"name": "note", "type": "TEXT"}},
        {"op": "raw_sql", "up": "UPDATE base SET v = v + 1"},
        {"op": "create_index", "name": "base_v", "table": "base", "columns": ["v"]},
        {"op": "rename_table", "from": "base", "to": "renamed"},
        {"op": "drop_table", "table": "extra", "definition": {
          "name": "extra", "columns": [{"name": "id", "type": "INTEGER", "primary_key": 1}],
          "sql": "CREATE TABLE extra (id INTEGER PRIMARY KEY)"}}
        "#,
    );
    let migrations = load_dir(dir.path()).expect("load");
    // The ledger create, six operations, and the ledger insert.
    let statement_count = migrations[0].up.len() + 2;

    // The exact statement at each position: ledger create, six operations, ledger insert.
    let expected: [String; 8] = [
        "CREATE TABLE IF NOT EXISTS _migratr_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL)".to_string(),
        r#"CREATE TABLE "extra" ("id" INTEGER, PRIMARY KEY ("id"))"#.to_string(),
        r#"ALTER TABLE "base" ADD COLUMN "note" TEXT"#.to_string(),
        "UPDATE base SET v = v + 1".to_string(),
        r#"CREATE INDEX "base_v" ON "base" ("v")"#.to_string(),
        r#"ALTER TABLE "base" RENAME TO "renamed""#.to_string(),
        r#"DROP TABLE "extra""#.to_string(),
        format!(
            "INSERT INTO _migratr_migrations (version, name, checksum, applied_at) VALUES (20260101000001, 'multi', '{}', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            migrations[0].checksum
        ),
    ];
    assert_eq!(expected.len(), statement_count);

    for (fail_at, expected_statement) in expected.iter().enumerate() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE base (id INTEGER PRIMARY KEY, v INTEGER);
             INSERT INTO base (v) VALUES (10), (20);",
        )
        .expect("seed");
        let mut ex = FailAt {
            inner: RusqliteExecutor::new(conn),
            fail_at,
            statement_count: 0,
        };
        let before = dump(ex.inner.connection());

        let err = up(&mut ex, &migrations, None).expect_err("injected failure");

        let MigrateError::Apply {
            version,
            statement,
            operation,
            ..
        } = err
        else {
            panic!("position {fail_at}: expected Apply, got {err:?}");
        };
        assert_eq!(version, 20260101000001, "position {fail_at}");
        assert_eq!(
            statement.as_deref(),
            Some(expected_statement.as_str()),
            "position {fail_at}"
        );
        let is_operation = (1..=6).contains(&fail_at);
        assert_eq!(
            operation,
            is_operation.then(|| fail_at - 1),
            "position {fail_at}"
        );
        assert_eq!(
            dump(ex.inner.connection()),
            before,
            "position {fail_at} changed the database"
        );
        assert!(ex.inner.read_ledger().expect("ledger").is_empty());
    }
}
