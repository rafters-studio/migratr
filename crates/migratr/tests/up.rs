use std::fs;
use std::path::Path;

use migratr::{
    Executor, LedgerRow, MigrateError, RusqliteExecutor, SchemaSnapshot, UpReport, load_dir, up,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
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

    let ledger = ex.read_ledger().expect("ledger");
    let hashes = (ledger[0].up_hash.clone(), ledger[0].down_hash.clone());
    assert!(hashes.0.is_some() && hashes.1.is_some(), "{ledger:?}");
    assert_eq!(
        ledger,
        vec![LedgerRow {
            version: 20260101000001,
            name: "one".into(),
            checksum: migrations[0].checksum.clone(),
            up_hash: hashes.0,
            down_hash: hashes.1,
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
    let operations = [
        r#"CREATE TABLE "extra" ("id" INTEGER, PRIMARY KEY ("id"))"#,
        r#"ALTER TABLE "base" ADD COLUMN "note" TEXT"#,
        "UPDATE base SET v = v + 1",
        r#"CREATE INDEX "base_v" ON "base" ("v")"#,
        r#"ALTER TABLE "base" RENAME TO "renamed""#,
        r#"DROP TABLE "extra""#,
    ];
    // The raw_sql has no down, so the migration has no down hash.
    let up_hash = format!("{:x}", Sha256::digest(operations.join("\n")));
    let expected: [String; 8] = [
        "CREATE TABLE IF NOT EXISTS _migratr_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL, up_hash TEXT, down_hash TEXT)".to_string(),
        operations[0].to_string(),
        operations[1].to_string(),
        operations[2].to_string(),
        operations[3].to_string(),
        operations[4].to_string(),
        operations[5].to_string(),
        format!(
            "INSERT INTO _migratr_migrations (version, name, checksum, applied_at, up_hash, down_hash) VALUES (20260101000001, 'multi', '{}', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), '{up_hash}', NULL)",
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

#[test]
fn a_create_table_with_checks_and_unique_writes_and_enforces_each() {
    let dir = TempDir::new().expect("tempdir");
    write(
        dir.path(),
        "20260101000001_seats.json",
        r#"{"op": "create_table", "table": "seats", "columns": [
            {"name": "id", "type": "INTEGER", "primary_key": 1},
            {"name": "row", "type": "TEXT"},
            {"name": "num", "type": "INTEGER"}],
           "checks": ["num > 0", "length(row) = 1"], "unique": [["row", "num"]]}"#,
    );
    let mut ex = executor();
    up(&mut ex, &load_dir(dir.path()).expect("load"), None).expect("up");

    let conn = ex.connection();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'seats'",
            [],
            |r| r.get(0),
        )
        .expect("sql");
    assert_eq!(
        sql,
        "CREATE TABLE \"seats\" (\"id\" INTEGER, \"row\" TEXT, \"num\" INTEGER, \
         PRIMARY KEY (\"id\"), CHECK (num > 0), CHECK (length(row) = 1), UNIQUE (\"row\", \"num\"))"
    );
    conn.execute("INSERT INTO seats (row, num) VALUES ('A', 1)", [])
        .expect("a valid row");
    for (row, why) in [
        ("('A', 0)", "a row breaking the first check"),
        ("('AB', 2)", "a row breaking the second check"),
        ("('A', 1)", "a row repeating the unique pair"),
    ] {
        let err = conn
            .execute(&format!("INSERT INTO seats (row, num) VALUES {row}"), [])
            .expect_err(why);
        assert!(
            err.to_string().contains("constraint failed"),
            "{why}: {err}"
        );
    }
}

#[test]
fn an_index_column_with_a_collate_writes_it_and_the_index_uses_it() {
    let dir = TempDir::new().expect("tempdir");
    write(
        dir.path(),
        "20260101000001_people.json",
        &format!(
            r#"{}, {{"op": "create_index", "name": "people_email", "table": "people",
                "columns": [{{"name": "email", "collate": "NOCASE"}}]}}"#,
            r#"{"op": "create_table", "table": "people", "columns": [{"name": "email", "type": "TEXT"}]}"#
        ),
    );
    let mut ex = executor();
    up(&mut ex, &load_dir(dir.path()).expect("load"), None).expect("up");

    let conn = ex.connection();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'people_email'",
            [],
            |r| r.get(0),
        )
        .expect("sql");
    assert_eq!(
        sql,
        "CREATE INDEX \"people_email\" ON \"people\" (\"email\" COLLATE NOCASE)"
    );
    let collation: String = conn
        .query_row(
            "SELECT coll FROM pragma_index_xinfo('people_email') WHERE name = 'email'",
            [],
            |r| r.get(0),
        )
        .expect("collation");
    assert_eq!(collation, "NOCASE");
}

#[test]
fn a_named_check_writes_its_constraint_name_and_enforces_it() {
    let dir = TempDir::new().expect("tempdir");
    write(
        dir.path(),
        "20260101000001_seats.json",
        r#"{"op": "create_table", "table": "seats", "columns": [{"name": "num", "type": "INTEGER"}],
           "checks": ["num < 100", {"name": "positive", "expr": "num > 0"}]}"#,
    );
    let mut ex = executor();
    up(&mut ex, &load_dir(dir.path()).expect("load"), None).expect("up");

    let conn = ex.connection();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'seats'",
            [],
            |r| r.get(0),
        )
        .expect("sql");
    assert_eq!(
        sql,
        "CREATE TABLE \"seats\" (\"num\" INTEGER, CHECK (num < 100), \
         CONSTRAINT \"positive\" CHECK (num > 0))"
    );
    let err = conn
        .execute("INSERT INTO seats (num) VALUES (0)", [])
        .expect_err("a row breaking the named check");
    assert!(err.to_string().contains("positive"), "{err}");
}

#[test]
fn a_create_trigger_naming_another_trigger_or_table_is_refused_and_nothing_runs() {
    let statement = "CREATE TRIGGER stamp AFTER INSERT ON one BEGIN SELECT 1; END";
    for (name, table) in [("other", "one"), ("stamp", "two")] {
        let dir = TempDir::new().expect("tempdir");
        write(
            dir.path(),
            "20260101000001_one.json",
            &format!(
                r#"{}, {{"op": "create_trigger", "name": "{name}", "table": "{table}", "sql": "{statement}"}}"#,
                create_table("one")
            ),
        );
        let mut ex = executor();

        let err = up(&mut ex, &load_dir(dir.path()).expect("load"), None)
            .expect_err("a trigger that disagrees with its statement");

        assert!(
            matches!(
                &err,
                MigrateError::TriggerMismatch { version: 20260101000001, operation: 1, statement, .. }
                    if statement == &Some(("stamp".to_string(), "one".to_string()))
            ),
            "{err:?}"
        );
        let message = err.to_string();
        for named in [name, table, "stamp", "one"] {
            assert!(message.contains(named), "{message}");
        }
        assert!(table_names(&mut ex).is_empty(), "nothing ran");
    }
}

/// migratr#100: a database written by VACUUM INTO, such as dev's rewind point, lists a
/// full-text table's shadow tables before the table; up still works out its hashes.
#[test]
fn up_records_hashes_on_a_vacuumed_database_with_a_full_text_table() {
    let dir = TempDir::new().expect("tempdir");
    let source = Connection::open_in_memory().expect("open");
    source
        .execute_batch(
            "CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT); \
             CREATE VIRTUAL TABLE docs USING fts5(body); \
             INSERT INTO docs (body) VALUES ('first note');",
        )
        .expect("place");
    let vacuumed = dir.path().join("vacuumed.db");
    source
        .execute("VACUUM INTO ?1", [vacuumed.to_str().expect("utf-8")])
        .expect("vacuum");
    let order: Vec<String> = Connection::open(&vacuumed)
        .expect("open vacuumed")
        .prepare("SELECT name FROM sqlite_master WHERE name LIKE 'docs%' ORDER BY rowid")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("names");
    assert_eq!(order.last().map(String::as_str), Some("docs"), "{order:?}");

    let migrations = dir.path().join("migrations");
    fs::create_dir(&migrations).expect("migrations dir");
    write(
        &migrations,
        "20260201000000_add_tag.json",
        r#"{"op": "add_column", "table": "notes", "column": {"name": "tag", "type": "TEXT"}}"#,
    );
    let mut ex = RusqliteExecutor::new(Connection::open(&vacuumed).expect("open"));
    up(&mut ex, &load_dir(&migrations).expect("load"), None).expect("up");
    let ledger = ex.read_ledger().expect("ledger");
    let up_hash = format!(
        "{:x}",
        Sha256::digest("ALTER TABLE \"notes\" ADD COLUMN \"tag\" TEXT")
    );
    assert_eq!(ledger[0].up_hash.as_deref(), Some(up_hash.as_str()));
    assert!(ledger[0].down_hash.is_some());
}
