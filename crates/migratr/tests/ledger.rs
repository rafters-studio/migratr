use std::fs;

use migratr::{
    Executor, MigrateError, Migration, RusqliteExecutor, down, load_dir, record_applied, up,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

mod common;
use common::{FailAt, dump};

/// One migration that would create table `made` if it ran.
fn creates_a_table() -> (TempDir, Vec<Migration>) {
    let dir = TempDir::new().expect("tempdir");
    fs::write(
        dir.path().join("20260101000001_made.json"),
        r#"{"up": [{"op": "create_table", "table": "made", "columns": [{"name": "id", "type": "INTEGER", "primary_key": 1}]}]}"#,
    )
    .expect("write migration");
    let migrations = load_dir(dir.path()).expect("load");
    (dir, migrations)
}

fn seeded() -> RusqliteExecutor {
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch("CREATE TABLE kept (a TEXT); INSERT INTO kept VALUES ('x');")
        .expect("seed");
    RusqliteExecutor::new(conn)
}

fn has_ledger(ex: &RusqliteExecutor) -> bool {
    dump(ex.connection())
        .iter()
        .any(|line| line.contains("_migratr_migrations"))
}

#[test]
fn a_recorded_migration_is_applied_for_a_later_up_which_does_not_run_it() {
    let (_dir, migrations) = creates_a_table();
    let mut ex = seeded();
    let before = dump(ex.connection());

    record_applied(&mut ex, &migrations[0]).expect("record");

    let rows = ex.read_ledger().expect("ledger");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].version, migrations[0].version);
    assert_eq!(rows[0].name, migrations[0].name);
    assert_eq!(rows[0].checksum, migrations[0].checksum);

    let recorded = dump(ex.connection());
    let report = up(&mut ex, &migrations, None).expect("up");
    assert!(report.applied.is_empty());
    assert_eq!(dump(ex.connection()), recorded);
    // The dump is ordered by name, so the ledger comes first and the rest is what was there.
    assert!(recorded.ends_with(&before), "{recorded:?}");
    assert!(!recorded.iter().any(|line| line.starts_with("table|made|")));
}

#[test]
fn a_failure_is_returned_and_leaves_no_ledger() {
    let (_dir, migrations) = creates_a_table();
    let mut ex = FailAt {
        inner: seeded(),
        fail_at: 1,
        statement_count: 0,
    };

    let err = record_applied(&mut ex, &migrations[0]).expect_err("refused");

    assert!(matches!(
        err,
        MigrateError::Apply { version, statement: Some(_), operation: None, .. }
            if version == migrations[0].version
    ));
    assert!(!has_ledger(&ex.inner));
}

fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text))
}

#[test]
fn an_applied_migration_records_the_hashes_of_its_statements_up_and_down() {
    let (_dir, migrations) = creates_a_table();
    let mut ex = seeded();

    up(&mut ex, &migrations, None).expect("up");

    let rows = ex.read_ledger().expect("ledger");
    assert_eq!(
        rows[0].up_hash.as_deref(),
        Some(sha256(r#"CREATE TABLE "made" ("id" INTEGER, PRIMARY KEY ("id"))"#).as_str())
    );
    assert_eq!(
        rows[0].down_hash.as_deref(),
        Some(sha256(r#"DROP TABLE "made""#).as_str())
    );
}

#[test]
fn a_recorded_row_has_no_hashes() {
    let (_dir, migrations) = creates_a_table();
    let mut ex = seeded();

    record_applied(&mut ex, &migrations[0]).expect("record");

    let rows = ex.read_ledger().expect("ledger");
    assert_eq!(
        (rows[0].up_hash.clone(), rows[0].down_hash.clone()),
        (None, None)
    );
}

/// A database whose ledger was written before the hash columns, holding two rows.
fn old_ledger() -> (TempDir, Vec<Migration>, RusqliteExecutor) {
    let dir = TempDir::new().expect("tempdir");
    for (version, table) in [(1, "a"), (2, "b")] {
        fs::write(
            dir.path().join(format!("2026010100000{version}_{table}.json")),
            format!(
                r#"{{"up": [{{"op": "create_table", "table": "{table}", "columns": [{{"name": "id", "type": "INTEGER", "primary_key": 1}}]}}]}}"#
            ),
        )
        .expect("write migration");
    }
    let migrations = load_dir(dir.path()).expect("load");
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch(
        "CREATE TABLE a (id INTEGER, PRIMARY KEY (id));
         CREATE TABLE b (id INTEGER, PRIMARY KEY (id));
         CREATE TABLE _migratr_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, \
           checksum TEXT NOT NULL, applied_at TEXT NOT NULL);",
    )
    .expect("seed");
    for m in &migrations {
        conn.execute(
            "INSERT INTO _migratr_migrations VALUES (?1, ?2, ?3, 't')",
            rusqlite::params![
                i64::try_from(m.version).expect("version"),
                m.name,
                m.checksum
            ],
        )
        .expect("old row");
    }
    (dir, migrations, RusqliteExecutor::new(conn))
}

fn hash_columns(ex: &RusqliteExecutor) -> Vec<String> {
    let mut statement = ex
        .connection()
        .prepare(
            "SELECT name FROM pragma_table_info('_migratr_migrations') WHERE name LIKE '%_hash'",
        )
        .expect("prepare");
    statement
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("columns")
}

#[test]
fn an_old_ledger_gains_the_hash_columns_on_an_up_with_nothing_pending_and_keeps_its_rows() {
    let (_dir, migrations, mut ex) = old_ledger();

    let report = up(&mut ex, &migrations, None).expect("up");

    assert!(report.applied.is_empty());
    assert_eq!(hash_columns(&ex), ["up_hash", "down_hash"]);
    let rows = ex.read_ledger().expect("ledger");
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|r| r.up_hash.is_none() && r.down_hash.is_none())
    );
}

#[test]
fn an_old_ledger_gains_the_hash_columns_on_a_down_and_keeps_its_other_rows() {
    let (_dir, migrations, mut ex) = old_ledger();

    down(&mut ex, &migrations, 1).expect("down");

    assert_eq!(hash_columns(&ex), ["up_hash", "down_hash"]);
    let rows = ex.read_ledger().expect("ledger");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].version, rows[0].up_hash.clone()),
        (migrations[0].version, None)
    );
}

#[test]
fn an_old_ledger_takes_a_new_migration_with_its_hashes() {
    let (dir, _, mut ex) = old_ledger();
    fs::write(
        dir.path().join("20260101000003_c.json"),
        r#"{"up": [{"op": "create_table", "table": "c", "columns": [{"name": "id", "type": "INTEGER", "primary_key": 1}]}]}"#,
    )
    .expect("write migration");
    let migrations = load_dir(dir.path()).expect("load");

    up(&mut ex, &migrations, None).expect("up");

    let rows = ex.read_ledger().expect("ledger");
    assert_eq!(rows.len(), 3);
    assert!(rows[..2].iter().all(|r| r.up_hash.is_none()));
    assert_eq!(
        rows[2].down_hash.as_deref(),
        Some(sha256(r#"DROP TABLE "c""#).as_str())
    );
}

#[test]
fn a_database_with_a_full_text_table_still_records_the_hashes() {
    let (_dir, migrations) = creates_a_table();
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch("CREATE VIRTUAL TABLE docs USING fts5(body)")
        .expect("seed");
    let mut ex = RusqliteExecutor::new(conn);

    up(&mut ex, &migrations, None).expect("up");

    let rows = ex.read_ledger().expect("ledger");
    assert_eq!(
        rows[0].down_hash.as_deref(),
        Some(sha256(r#"DROP TABLE "made""#).as_str())
    );
}
