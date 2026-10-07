use std::fs;
use std::path::{Path, PathBuf};

use migratr::{
    AtomicError, Executor, LedgerRow, MigrateError, Migration, RusqliteExecutor, SchemaSnapshot,
    down, load_dir, restore, up,
};
use rusqlite::Connection;
use tempfile::TempDir;

/// An executor whose snapshot fails, as one that cannot snapshot a durable database does.
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

    fn snapshot(&mut self, _file_name: &str) -> Result<Option<PathBuf>, Self::Error> {
        Err(rusqlite::Error::InvalidQuery)
    }
}

const CREATE_USERS: &str = r#"{"op": "create_table", "table": "users", "columns": [
    {"name": "id", "type": "INTEGER", "primary_key": 1},
    {"name": "email", "type": "TEXT"}]}"#;

const DROP_USERS_SQL: &str = r#"{"op": "raw_sql", "up": "DROP TABLE users"}"#;

fn write(dir: &TempDir, file: &str, ops: &str) {
    fs::write(dir.path().join(file), format!(r#"{{"up": [{ops}]}}"#)).expect("write migration");
}

fn load(dir: &TempDir) -> Vec<Migration> {
    load_dir(dir.path()).expect("load")
}

/// A file database in its own directory.
fn file_db(dir: &TempDir) -> (RusqliteExecutor, PathBuf) {
    let db = dir.path().join("app.db");
    let ex = RusqliteExecutor::new(Connection::open(&db).expect("open"));
    (ex, db)
}

fn seed_users(ex: &RusqliteExecutor) {
    ex.connection()
        .execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
             INSERT INTO users VALUES (1, 'a@x');",
        )
        .expect("seed");
}

fn snapshot_files(dir: &TempDir) -> Vec<String> {
    let snapshots = dir.path().join(".migratr/snapshots");
    if !snapshots.exists() {
        return Vec::new();
    }
    let mut names: Vec<String> = fs::read_dir(snapshots)
        .expect("snapshot dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn user_emails(db: &Path) -> Vec<String> {
    let conn = Connection::open(db).expect("open");
    let mut stmt = conn
        .prepare("SELECT email FROM users ORDER BY id")
        .expect("prepare");
    stmt.query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn has_table(db: &Path, name: &str) -> bool {
    Connection::open(db)
        .expect("open")
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = ?1",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
        == 1
}

#[test]
fn a_destructive_up_writes_one_snapshot_of_the_state_before_it() {
    let dir = TempDir::new().expect("tmp");
    let migrations = TempDir::new().expect("tmp");
    let (mut ex, db) = file_db(&dir);
    seed_users(&ex);
    write(
        &migrations,
        "20260101000000_drop_users.json",
        DROP_USERS_SQL,
    );

    up(&mut ex, &load(&migrations), None).expect("up");

    let files = snapshot_files(&dir);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].ends_with("_20260101000000_up.db"), "{files:?}");
    assert!(!has_table(&db, "users"));
    let snapshot = dir.path().join(".migratr/snapshots").join(&files[0]);
    assert_eq!(user_emails(&snapshot), vec!["a@x"]);
}

#[test]
fn a_non_destructive_up_writes_no_snapshot() {
    let dir = TempDir::new().expect("tmp");
    let migrations = TempDir::new().expect("tmp");
    let (mut ex, _db) = file_db(&dir);
    write(&migrations, "20260101000000_users.json", CREATE_USERS);

    up(&mut ex, &load(&migrations), None).expect("up");

    assert!(snapshot_files(&dir).is_empty());
}

#[test]
fn a_down_that_drops_what_a_create_table_added_snapshots_its_rows() {
    let dir = TempDir::new().expect("tmp");
    let migrations = TempDir::new().expect("tmp");
    let (mut ex, db) = file_db(&dir);
    write(&migrations, "20260101000000_users.json", CREATE_USERS);
    let loaded = load(&migrations);
    up(&mut ex, &loaded, None).expect("up");
    ex.connection()
        .execute("INSERT INTO users VALUES (1, 'a@x')", [])
        .expect("insert");

    down(&mut ex, &loaded, 1).expect("down");

    let files = snapshot_files(&dir);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].ends_with("_20260101000000_down.db"), "{files:?}");
    assert!(!has_table(&db, "users"));
    let snapshot = dir.path().join(".migratr/snapshots").join(&files[0]);
    assert_eq!(user_emails(&snapshot), vec!["a@x"]);
}

#[test]
fn an_unwritable_snapshot_directory_stops_a_destructive_up_with_nothing_changed() {
    let dir = TempDir::new().expect("tmp");
    let migrations = TempDir::new().expect("tmp");
    let (mut ex, db) = file_db(&dir);
    seed_users(&ex);
    // A regular file where the .migratr directory must go makes the directory uncreatable.
    fs::write(dir.path().join(".migratr"), "").expect("blocker");
    write(
        &migrations,
        "20260101000000_drop_users.json",
        DROP_USERS_SQL,
    );

    let err = up(&mut ex, &load(&migrations), None).expect_err("must refuse");

    assert!(matches!(err, MigrateError::SnapshotFailed { .. }), "{err}");
    assert!(has_table(&db, "users"));
    assert!(ex.read_ledger().expect("ledger").is_empty());
}

#[test]
fn an_executor_that_cannot_snapshot_stops_a_destructive_up_with_nothing_changed() {
    let dir = TempDir::new().expect("tmp");
    let migrations = TempDir::new().expect("tmp");
    let (ex, db) = file_db(&dir);
    seed_users(&ex);
    let mut ex = NoSnapshots(ex);
    write(
        &migrations,
        "20260101000000_drop_users.json",
        DROP_USERS_SQL,
    );

    let err = up(&mut ex, &load(&migrations), None).expect_err("must refuse");

    assert!(matches!(err, MigrateError::SnapshotFailed { .. }), "{err}");
    assert!(has_table(&db, "users"));
    assert!(ex.read_ledger().expect("ledger").is_empty());
}

#[test]
fn an_executor_that_cannot_snapshot_stops_a_destructive_down_with_nothing_changed() {
    let dir = TempDir::new().expect("tmp");
    let migrations = TempDir::new().expect("tmp");
    let (ex, db) = file_db(&dir);
    write(&migrations, "20260101000000_users.json", CREATE_USERS);
    let loaded = load(&migrations);
    let mut ex = NoSnapshots(ex);
    up(&mut ex, &loaded, None).expect("up is not destructive");

    let err = down(&mut ex, &loaded, 1).expect_err("must refuse");

    assert!(matches!(err, MigrateError::SnapshotFailed { .. }), "{err}");
    assert!(has_table(&db, "users"));
    assert_eq!(ex.read_ledger().expect("ledger").len(), 1);
}

#[test]
fn the_second_snapshot_removes_the_first() {
    let dir = TempDir::new().expect("tmp");
    let migrations = TempDir::new().expect("tmp");
    let (mut ex, _db) = file_db(&dir);
    seed_users(&ex);
    write(
        &migrations,
        "20260101000000_raw_one.json",
        r#"{"op": "raw_sql", "up": "INSERT INTO users VALUES (2, 'b@x')"}"#,
    );
    write(
        &migrations,
        "20260101000001_raw_two.json",
        r#"{"op": "raw_sql", "up": "INSERT INTO users VALUES (3, 'c@x')"}"#,
    );

    up(&mut ex, &load(&migrations), None).expect("up");

    let files = snapshot_files(&dir);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].ends_with("_20260101000001_up.db"), "{files:?}");
}

/// Takes a snapshot named like the crate's own and returns the database path.
fn diverged_with_snapshot(dir: &TempDir) -> PathBuf {
    let (mut ex, db) = file_db(dir);
    ex.connection()
        .execute_batch(
            "CREATE TABLE _migratr_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL);
             INSERT INTO _migratr_migrations VALUES (1, 'a', 'ca', 't');
             CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
             INSERT INTO users VALUES (1, 'a@x');",
        )
        .expect("seed");
    ex.snapshot("20260101000000_1_up.db")
        .expect("snapshot")
        .expect("durable");
    ex.connection()
        .execute_batch(
            "INSERT INTO _migratr_migrations VALUES (2, 'b', 'cb', 't');
             INSERT INTO _migratr_migrations VALUES (3, 'c', 'cc', 't');
             DROP TABLE users;",
        )
        .expect("diverge");
    db
}

#[test]
fn restore_without_confirmation_reports_and_changes_nothing() {
    let dir = TempDir::new().expect("tmp");
    let db = diverged_with_snapshot(&dir);

    let err = restore(&db, None, false).expect_err("needs confirmation");

    let MigrateError::RestoreNotConfirmed { report } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(report.removed, vec![3, 2]);
    assert_eq!(report.snapshot_version, Some(1));
    assert!(!report.restored);
    assert!(!has_table(&db, "users"));
}

#[test]
fn restore_with_confirmation_returns_schema_data_and_ledger() {
    let dir = TempDir::new().expect("tmp");
    let db = diverged_with_snapshot(&dir);

    let report = restore(&db, None, true).expect("restore");

    assert!(report.restored);
    assert_eq!(report.removed, vec![3, 2]);
    assert_eq!(report.snapshot_time, "20260101000000");
    let mut after = RusqliteExecutor::new(Connection::open(&db).expect("open"));
    let versions: Vec<u64> = after
        .read_ledger()
        .expect("ledger")
        .iter()
        .map(|r| r.version)
        .collect();
    assert_eq!(versions, vec![1]);
    assert_eq!(user_emails(&db), vec!["a@x"]);
}

#[test]
fn restore_with_no_snapshots_is_an_error() {
    let dir = TempDir::new().expect("tmp");
    let (ex, db) = file_db(&dir);
    drop(ex);
    assert!(matches!(
        restore(&db, None, true),
        Err(MigrateError::Io { .. })
    ));
}
