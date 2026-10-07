use std::fs;
use std::path::Path;

use migratr::{
    Executor, LedgerRow, MigrateError, RusqliteExecutor, SchemaSnapshot, UpReport, load_dir, up,
};
use rusqlite::Connection;
use tempfile::TempDir;

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
        MigrateError::Apply { version: 20260101000002, ref statement, .. }
            if statement.contains("NOT VALID SQL")
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

/// Runs the statements through a real executor with statement `fail_at` replaced by invalid
/// SQL, so the real transaction fails at that position and rolls back.
struct FailAt {
    inner: RusqliteExecutor,
    fail_at: usize,
}

impl Executor for FailAt {
    type Error = rusqlite::Error;

    fn read_schema(&mut self) -> Result<SchemaSnapshot, Self::Error> {
        self.inner.read_schema()
    }

    fn read_ledger(&mut self) -> Result<Vec<LedgerRow>, Self::Error> {
        self.inner.read_ledger()
    }

    fn run_atomic(
        &mut self,
        statements: &[String],
        suspend_foreign_keys: bool,
    ) -> Result<(), Self::Error> {
        let mut broken = statements.to_vec();
        broken[self.fail_at] = "FAULT INJECTED HERE".to_string();
        self.inner.run_atomic(&broken, suspend_foreign_keys)
    }

    fn snapshot(&mut self, path: &Path) -> Result<bool, Self::Error> {
        self.inner.snapshot(path)
    }
}

/// Every sqlite_master row and every table's rows, as text.
fn dump(conn: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    let mut master = conn
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name")
        .expect("prepare");
    let rows: Vec<(String, String, String, Option<String>)> = master
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    for (kind, name, tbl, sql) in rows {
        out.push(format!("{kind}|{name}|{tbl}|{sql:?}"));
        if kind == "table" {
            let mut all = conn
                .prepare(&format!("SELECT * FROM \"{name}\" ORDER BY 1"))
                .expect("select");
            let width = all.column_count();
            let cells: Vec<String> = all
                .query_map([], |r| {
                    (0..width)
                        .map(|i| {
                            r.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<Result<Vec<_>, _>>()
                        .map(|c| c.join(","))
                })
                .expect("cells")
                .collect::<Result<_, _>>()
                .expect("cells");
            out.extend(cells);
        }
    }
    out
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

    for fail_at in 0..statement_count {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE base (id INTEGER PRIMARY KEY, v INTEGER);
             INSERT INTO base (v) VALUES (10), (20);",
        )
        .expect("seed");
        let mut ex = FailAt {
            inner: RusqliteExecutor::new(conn),
            fail_at,
        };
        let before = dump(ex.inner.connection());

        let err = up(&mut ex, &migrations, None).expect_err("injected failure");

        assert!(
            matches!(
                err,
                MigrateError::Apply {
                    version: 20260101000001,
                    ..
                }
            ),
            "position {fail_at}: {err:?}"
        );
        assert_eq!(
            dump(ex.inner.connection()),
            before,
            "position {fail_at} changed the database"
        );
        assert!(ex.inner.read_ledger().expect("ledger").is_empty());
    }
}
