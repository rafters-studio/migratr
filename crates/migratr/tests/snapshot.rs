use std::fs;
use std::path::Path;

use migratr::{
    AtomicError, Direction, Executor, LedgerRow, MigrateError, Op, RusqliteExecutor,
    SchemaSnapshot, is_destructive, restore, snapshot,
};
use rusqlite::Connection;
use tempfile::TempDir;

/// An executor that cannot write snapshots.
struct NoSnapshots(RusqliteExecutor);

impl Executor for NoSnapshots {
    type Error = rusqlite::Error;

    fn read_schema(&mut self) -> Result<SchemaSnapshot, Self::Error> {
        self.0.read_schema()
    }

    fn read_ledger(&mut self) -> Result<Vec<LedgerRow>, Self::Error> {
        self.0.read_ledger()
    }

    fn run_atomic(
        &mut self,
        statements: &[String],
        suspend_foreign_keys: bool,
    ) -> Result<(), AtomicError<Self::Error>> {
        self.0.run_atomic(statements, suspend_foreign_keys)
    }

    fn snapshot(&mut self, _path: &Path) -> Result<bool, Self::Error> {
        Ok(false)
    }
}

fn seeded(dir: &TempDir) -> (RusqliteExecutor, std::path::PathBuf) {
    let db = dir.path().join("app.db");
    let conn = Connection::open(&db).expect("open");
    conn.execute_batch(
        "CREATE TABLE _migratr_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL);
         INSERT INTO _migratr_migrations VALUES (1, 'a', 'ca', 't');
         CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
         INSERT INTO users VALUES (1, 'a@x');",
    )
    .expect("seed");
    (RusqliteExecutor::new(conn), db)
}

fn snapshot_files(dir: &TempDir) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir.path().join(".migratr/snapshots"))
        .expect("snapshot dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn drop_ops() -> Vec<Op> {
    vec![Op::RawSql {
        up: "DROP TABLE users".into(),
        down: None,
    }]
}

#[test]
fn a_snapshot_is_a_named_copy_of_the_database() {
    let dir = TempDir::new().expect("tmp");
    let (mut ex, db) = seeded(&dir);

    let path = snapshot(&mut ex, &db, 20260101000000, Direction::Up).expect("snapshot");

    let name = path
        .file_name()
        .expect("name")
        .to_string_lossy()
        .into_owned();
    assert!(name.ends_with("_20260101000000_up.db"), "{name}");
    assert_eq!(name.split('_').next().map(str::len), Some(14));
    let copy = Connection::open(&path).expect("open copy");
    let email: String = copy
        .query_row("SELECT email FROM users", [], |r| r.get(0))
        .expect("row");
    assert_eq!(email, "a@x");
}

#[test]
fn down_snapshots_are_named_down() {
    let dir = TempDir::new().expect("tmp");
    let (mut ex, db) = seeded(&dir);
    let path = snapshot(&mut ex, &db, 5, Direction::Down).expect("snapshot");
    assert!(path.to_string_lossy().ends_with("_5_down.db"));
}

#[test]
fn only_destructive_steps_need_a_snapshot() {
    assert!(is_destructive(&drop_ops()));
    assert!(!is_destructive(&[Op::RenameTable {
        from: "a".into(),
        to: "b".into()
    }]));
}

#[test]
fn the_default_keeps_only_the_newest_snapshot() {
    let dir = TempDir::new().expect("tmp");
    let (mut ex, db) = seeded(&dir);

    let first = snapshot(&mut ex, &db, 1, Direction::Up).expect("first");
    let second = snapshot(&mut ex, &db, 2, Direction::Up).expect("second");

    assert!(!first.exists());
    assert!(second.exists());
    assert_eq!(snapshot_files(&dir).len(), 1);
}

#[test]
fn an_executor_without_snapshot_support_stops_the_step() {
    let dir = TempDir::new().expect("tmp");
    let (ex, db) = seeded(&dir);
    let mut ex = NoSnapshots(ex);

    let err = snapshot(&mut ex, &db, 1, Direction::Up).expect_err("must fail");

    assert!(matches!(err, MigrateError::SnapshotFailed { .. }), "{err}");
    assert!(snapshot_files(&dir).is_empty());
}

#[test]
fn an_unwritable_directory_stops_the_step_and_leaves_the_database_alone() {
    let dir = TempDir::new().expect("tmp");
    let (mut ex, db) = seeded(&dir);
    // A file where the .migratr directory's parent must be makes the directory uncreatable.
    let blocker = dir.path().join("blocked");
    fs::write(&blocker, "").expect("blocker");
    let unreachable_db = blocker.join("app.db");

    let err = snapshot(&mut ex, &unreachable_db, 1, Direction::Up).expect_err("must fail");

    assert!(matches!(err, MigrateError::SnapshotFailed { .. }), "{err}");
    let rows: i64 = Connection::open(&db)
        .expect("open")
        .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 1);
}

#[test]
fn restore_without_confirmation_reports_and_changes_nothing() {
    let dir = TempDir::new().expect("tmp");
    let (mut ex, db) = seeded(&dir);
    snapshot(&mut ex, &db, 1, Direction::Up).expect("snapshot");
    ex.connection()
        .execute_batch(
            "INSERT INTO _migratr_migrations VALUES (2, 'b', 'cb', 't'); DROP TABLE users;",
        )
        .expect("diverge");
    drop(ex);

    let err = restore(&db, None, false).expect_err("needs confirmation");

    let MigrateError::RestoreNotConfirmed { report } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(report.removed, vec![2]);
    assert_eq!(report.snapshot_version, Some(1));
    assert!(!report.restored);
    let tables: i64 = Connection::open(&db)
        .expect("open")
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'users'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(tables, 0);
}

#[test]
fn restore_with_confirmation_returns_schema_data_and_ledger() {
    let dir = TempDir::new().expect("tmp");
    let (mut ex, db) = seeded(&dir);
    snapshot(&mut ex, &db, 1, Direction::Up).expect("snapshot");
    ex.connection()
        .execute_batch(
            "INSERT INTO _migratr_migrations VALUES (2, 'b', 'cb', 't');
             INSERT INTO _migratr_migrations VALUES (3, 'c', 'cc', 't');
             DROP TABLE users;",
        )
        .expect("diverge");
    drop(ex);

    let report = restore(&db, None, true).expect("restore");

    assert!(report.restored);
    assert_eq!(report.removed, vec![3, 2]);
    assert_eq!(report.snapshot_time.len(), 14);
    let mut after = RusqliteExecutor::new(Connection::open(&db).expect("open"));
    let versions: Vec<u64> = after
        .read_ledger()
        .expect("ledger")
        .iter()
        .map(|r| r.version)
        .collect();
    assert_eq!(versions, vec![1]);
    let email: String = after
        .connection()
        .query_row("SELECT email FROM users", [], |r| r.get(0))
        .expect("users row");
    assert_eq!(email, "a@x");
}

#[test]
fn restore_with_no_snapshots_is_an_error() {
    let dir = TempDir::new().expect("tmp");
    let (ex, db) = seeded(&dir);
    drop(ex);
    assert!(matches!(
        restore(&db, None, true),
        Err(MigrateError::Io { .. })
    ));
}
