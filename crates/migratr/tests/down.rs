use std::fs;
use std::path::Path;

use migratr::{Executor, MigrateError, Migration, RusqliteExecutor, down, load_dir, up};
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

fn load(dir: &TempDir) -> Vec<Migration> {
    load_dir(dir.path()).expect("load")
}

fn versions(ex: &mut RusqliteExecutor) -> Vec<u64> {
    ex.read_ledger()
        .expect("ledger")
        .iter()
        .map(|r| r.version)
        .collect()
}

/// The schema and rows, leaving out the ledger table and its rows (versions start `2026`).
fn state(conn: &Connection) -> Vec<String> {
    dump(conn)
        .into_iter()
        .filter(|line| !line.contains("_migratr_migrations") && !line.starts_with("Integer(2026"))
        .collect()
}

const CREATE_USERS: &str = r#"{"op": "create_table", "table": "users", "columns": [
    {"name": "id", "type": "INTEGER", "primary_key": 1},
    {"name": "email", "type": "TEXT"}]}"#;

const ADD_NAME: &str = r#"{"op": "add_column", "table": "users",
    "column": {"name": "name", "type": "TEXT"}}"#;

const INDEX_EMAIL: &str = r#"{"op": "create_index", "name": "users_email", "table": "users",
    "columns": ["email"], "unique": true}"#;

const RENAME: &str = r#"{"op": "rename_column", "table": "users", "from": "email", "to": "mail"}"#;

#[test]
fn every_operation_round_trips_to_the_prior_schema() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_users.json", CREATE_USERS);
    write(
        dir.path(),
        "20260101000002_more.json",
        &format!("{ADD_NAME}, {INDEX_EMAIL}, {RENAME}"),
    );
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations[..1], None).expect("up one");
    let after_one = state(ex.connection());
    up(&mut ex, &migrations, None).expect("up all");
    assert_ne!(state(ex.connection()), after_one);

    let report = down(&mut ex, &migrations, 1).expect("down");

    assert_eq!(report.reverted, vec![20260101000002]);
    assert_eq!(state(ex.connection()), after_one);
    assert_eq!(versions(&mut ex), vec![20260101000001]);

    down(&mut ex, &migrations, 1).expect("down again");
    assert_eq!(versions(&mut ex), Vec::<u64>::new());
    assert!(state(ex.connection()).is_empty());
}

#[test]
fn steps_reverse_newest_first_and_stop_at_the_count() {
    let dir = TempDir::new().expect("tempdir");
    for (i, table) in ["one", "two", "three"].iter().enumerate() {
        write(
            dir.path(),
            &format!("2026010100000{}_{table}.json", i + 1),
            &format!(
                r#"{{"op": "create_table", "table": "{table}", "columns": [{{"name": "id", "type": "INTEGER"}}]}}"#
            ),
        );
    }
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations, None).expect("up");

    let report = down(&mut ex, &migrations, 2).expect("down");

    assert_eq!(report.reverted, vec![20260101000003, 20260101000002]);
    assert_eq!(versions(&mut ex), vec![20260101000001]);
    assert!(
        report
            .to_string()
            .contains("Data in dropped objects is not restored")
    );

    let report = down(&mut ex, &migrations, 10).expect("down past the start");
    assert_eq!(report.reverted, vec![20260101000001]);
    assert!(
        down(&mut ex, &migrations, 1)
            .expect("empty")
            .reverted
            .is_empty()
    );
}

#[test]
fn drop_table_and_drop_index_recreate_from_the_carried_definition() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_users.json", CREATE_USERS);
    write(
        dir.path(),
        "20260101000002_partial.json",
        r#"{"op": "create_index", "name": "p", "table": "users", "columns": ["email"]}"#,
    );
    let before_drops = {
        let migrations = load(&dir);
        let mut ex = executor();
        up(&mut ex, &migrations, None).expect("up");
        state(ex.connection())
    };

    write(
        dir.path(),
        "20260101000003_drops.json",
        r#"{"op": "drop_index", "definition": {"name": "p", "table": "users", "columns": ["email"]}},
           {"op": "drop_table", "table": "users", "definition": {"name": "users",
             "columns": [{"name": "id", "type": "INTEGER", "primary_key": 1},
                         {"name": "email", "type": "TEXT"}],
             "sql": "CREATE TABLE \"users\" (\"id\" INTEGER, \"email\" TEXT, PRIMARY KEY (\"id\"))"}}"#,
    );
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations, None).expect("up");
    assert!(state(ex.connection()).is_empty());

    down(&mut ex, &migrations, 1).expect("down");

    let restored = state(ex.connection());
    assert!(restored.iter().any(|l| l.contains("|users|users|")));
    assert!(restored.iter().any(|l| l.starts_with("index|p|users|")));
    assert_eq!(restored.len(), before_drops.len());
}

#[test]
fn raw_sql_runs_its_down() {
    let dir = TempDir::new().expect("tempdir");
    write(
        dir.path(),
        "20260101000001_raw.json",
        r#"{"op": "raw_sql", "up": "CREATE TABLE r (a)", "down": "DROP TABLE r"}"#,
    );
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations, None).expect("up");

    down(&mut ex, &migrations, 1).expect("down");

    assert!(state(ex.connection()).is_empty());
}

#[test]
fn an_irreversible_operation_refuses_before_anything_runs() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_users.json", CREATE_USERS);
    write(
        dir.path(),
        "20260101000002_raw.json",
        &format!(r#"{ADD_NAME}, {{"op": "raw_sql", "up": "UPDATE users SET name = 'x'"}}"#),
    );
    write(dir.path(), "20260101000003_index.json", INDEX_EMAIL);
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations, None).expect("up");
    let applied = state(ex.connection());

    let err = down(&mut ex, &migrations, 3).expect_err("refused");

    assert!(matches!(
        err,
        MigrateError::Irreversible {
            version: 20260101000002,
            op_index: 1
        }
    ));
    assert_eq!(state(ex.connection()), applied);
    assert_eq!(versions(&mut ex).len(), 3);
}

#[test]
fn a_failed_reversal_rolls_back_its_ledger_delete() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_users.json", CREATE_USERS);
    let migrations = load(&dir);
    let mut ex = FailAt {
        inner: executor(),
        fail_at: usize::MAX,
        statement_count: 0,
    };
    up(&mut ex, &migrations, None).expect("up");
    let applied = state(ex.inner.connection());
    ex.fail_at = 1;

    let err = down(&mut ex, &migrations, 1).expect_err("fails");

    assert!(matches!(
        err,
        MigrateError::Apply {
            version: 20260101000001,
            ..
        }
    ));
    assert_eq!(state(ex.inner.connection()), applied);
    assert_eq!(versions(&mut ex.inner), vec![20260101000001]);
}

#[test]
fn an_edited_file_refuses_down() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_users.json", CREATE_USERS);
    let mut ex = executor();
    up(&mut ex, &load(&dir), None).expect("up");
    write(dir.path(), "20260101000001_users.json", INDEX_EMAIL);

    let err = down(&mut ex, &load(&dir), 1).expect_err("refused");

    assert!(matches!(err, MigrateError::ChecksumMismatch { .. }));
}
