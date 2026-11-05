use std::fs;

use migratr::{Executor, MigrateError, Migration, RusqliteExecutor, load_dir, record_applied, up};
use rusqlite::Connection;
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
