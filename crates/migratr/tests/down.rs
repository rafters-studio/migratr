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

#[test]
fn a_dropped_unique_column_comes_back_last_with_its_definition() {
    let dir = TempDir::new().expect("tempdir");
    write(
        dir.path(),
        "20260101000001_users.json",
        r#"{"op": "create_table", "table": "users", "columns": [
            {"name": "id", "type": "INTEGER", "primary_key": 1},
            {"name": "tag", "type": "TEXT", "unique": true},
            {"name": "email", "type": "TEXT"}]}"#,
    );
    write(
        dir.path(),
        "20260101000002_drop_tag.json",
        r#"{"op": "drop_column", "table": "users",
            "column": {"name": "tag", "type": "TEXT", "unique": true}}"#,
    );
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations[..1], None).expect("up one");
    ex.connection()
        .execute(
            "INSERT INTO users (id, tag, email) VALUES (1, 't', 'e')",
            [],
        )
        .expect("seed");
    up(&mut ex, &migrations, None).expect("up two");

    down(&mut ex, &migrations, 1).expect("down");

    let columns: Vec<String> = ex
        .connection()
        .prepare("SELECT name FROM pragma_table_info('users')")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(columns, ["id", "email", "tag"]);
    let tag: Option<String> = ex
        .connection()
        .query_row("SELECT tag FROM users", [], |r| r.get(0))
        .expect("row survives");
    assert_eq!(tag, None);
    ex.connection()
        .execute("INSERT INTO users (id, tag) VALUES (2, 'a'), (3, 'b')", [])
        .expect("distinct tags");
    assert!(
        ex.connection()
            .execute("INSERT INTO users (id, tag) VALUES (4, 'a')", [])
            .is_err(),
        "the restored column is still UNIQUE"
    );
}

#[test]
fn a_dropped_column_comes_back_with_type_default_and_collation() {
    let dir = TempDir::new().expect("tempdir");
    write(
        dir.path(),
        "20260101000001_users.json",
        r#"{"op": "create_table", "table": "users", "columns": [
            {"name": "id", "type": "INTEGER", "primary_key": 1},
            {"name": "name", "type": "TEXT", "collation": "NOCASE", "default": "'anon'"},
            {"name": "email", "type": "TEXT"}]}"#,
    );
    write(
        dir.path(),
        "20260101000002_drop_name.json",
        r#"{"op": "drop_column", "table": "users",
            "column": {"name": "name", "type": "TEXT", "collation": "NOCASE", "default": "'anon'"}}"#,
    );
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations, None).expect("up");

    down(&mut ex, &migrations, 1).expect("down");

    let schema = ex.read_schema().expect("schema");
    let users = schema
        .tables
        .iter()
        .find(|t| t.name == "users")
        .expect("users");
    let last = users.columns.last().expect("columns");
    assert_eq!(last.name, "name");
    assert_eq!(last.decl_type, "TEXT");
    assert_eq!(last.default_value.as_deref(), Some("'anon'"));
    let sql = schema
        .objects
        .iter()
        .find(|o| o.name == "users")
        .and_then(|o| o.sql.clone())
        .expect("sql");
    assert!(sql.contains("COLLATE NOCASE"), "{sql}");
}

#[test]
fn an_added_column_with_its_index_reverses() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_users.json", CREATE_USERS);
    write(
        dir.path(),
        "20260101000002_nick.json",
        r#"{"op": "add_column", "table": "users", "column": {"name": "nick", "type": "TEXT"}},
           {"op": "create_index", "name": "users_nick", "table": "users", "columns": ["nick"]}"#,
    );
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations[..1], None).expect("up one");
    let after_one = state(ex.connection());
    up(&mut ex, &migrations, None).expect("up two");

    down(&mut ex, &migrations, 1).expect("down");

    assert_eq!(state(ex.connection()), after_one);
}

#[test]
fn rename_table_round_trips() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_users.json", CREATE_USERS);
    write(
        dir.path(),
        "20260101000002_rename.json",
        r#"{"op": "rename_table", "from": "users", "to": "people"}"#,
    );
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations[..1], None).expect("up one");
    let after_one = state(ex.connection());
    up(&mut ex, &migrations, None).expect("up two");

    down(&mut ex, &migrations, 1).expect("down");

    assert_eq!(state(ex.connection()), after_one);
}

#[test]
fn a_failed_reversal_names_the_operation_in_file_order() {
    let dir = TempDir::new().expect("tempdir");
    write(
        dir.path(),
        "20260101000001_two_ops.json",
        r#"{"op": "create_table", "table": "a", "columns": [{"name": "id", "type": "INTEGER"}]},
           {"op": "raw_sql", "up": "CREATE TABLE b (x)", "down": "NOT SQL"}"#,
    );
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations, None).expect("up");

    let err = down(&mut ex, &migrations, 1).expect_err("fails");

    assert!(matches!(
        err,
        MigrateError::Apply {
            operation: Some(1),
            ..
        }
    ));
}

#[test]
fn a_reversal_that_would_rebuild_after_another_operation_refuses_before_anything_runs() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_users.json", CREATE_USERS);
    write(
        dir.path(),
        "20260101000002_code.json",
        &format!(
            r#"{{"op": "add_column", "table": "users",
                "column": {{"name": "code", "type": "TEXT", "unique": true}}}}, {ADD_NAME}"#
        ),
    );
    write(
        dir.path(),
        "20260101000003_index.json",
        r#"{"op": "create_index", "name": "users_name", "table": "users", "columns": ["name"]}"#,
    );
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations, None).expect("up");
    let before = dump(ex.connection());

    let err = down(&mut ex, &migrations, 2).expect_err("refused");

    assert!(
        matches!(
            err,
            MigrateError::RebuildNotFirst { version: 20260101000002, ref column, operation: 0, .. }
                if column == "code"
        ),
        "{err:?}"
    );
    assert_eq!(dump(ex.connection()), before);
}

/// Each key of `index` as (column name or NULL for an expression, desc), from PRAGMA index_xinfo.
fn index_keys(conn: &Connection, index: &str) -> Vec<(Option<String>, bool)> {
    conn.prepare(&format!(
        "SELECT name, \"desc\" FROM pragma_index_xinfo('{index}') WHERE key = 1 ORDER BY seqno"
    ))
    .expect("prepare")
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("keys")
}

#[test]
fn partial_expression_and_desc_indexes_are_made_and_each_down_drops_it() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_users.json", CREATE_USERS);
    write(
        dir.path(),
        "20260101000002_indexes.json",
        r#"{"op": "create_index", "name": "users_partial", "table": "users",
             "columns": ["email"], "where": "email IS NOT NULL"},
           {"op": "create_index", "name": "users_lower", "table": "users", "unique": true,
             "columns": [{"expr": "lower(email)"}]},
           {"op": "create_index", "name": "users_desc", "table": "users",
             "columns": [{"name": "email", "desc": true}, "id"]}"#,
    );
    let migrations = load(&dir);
    let mut ex = executor();
    up(&mut ex, &migrations, Some(20260101000001)).expect("users");
    let before = state(ex.connection());

    up(&mut ex, &migrations, None).expect("indexes");

    let conn = ex.connection();
    let partial: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'users_partial'",
            [],
            |r| r.get(0),
        )
        .expect("partial");
    assert!(partial.ends_with("WHERE email IS NOT NULL"), "{partial}");
    let is_partial: i64 = conn
        .query_row(
            "SELECT partial FROM pragma_index_list('users') WHERE name = 'users_partial'",
            [],
            |r| r.get(0),
        )
        .expect("index_list");
    assert_eq!(is_partial, 1);
    assert_eq!(index_keys(conn, "users_lower"), [(None, false)]);
    conn.execute("INSERT INTO users (email) VALUES ('A')", [])
        .expect("first");
    assert!(
        conn.execute("INSERT INTO users (email) VALUES ('a')", [])
            .is_err(),
        "the expression index is not enforced"
    );
    conn.execute("DELETE FROM users", []).expect("clear");
    assert_eq!(
        index_keys(conn, "users_desc"),
        [(Some("email".into()), true), (Some("id".into()), false)]
    );

    down(&mut ex, &migrations, 1).expect("down");
    assert_eq!(state(ex.connection()), before);
}

const AUDITED: &str = r#"{"op": "create_table", "table": "items", "columns": [
    {"name": "id", "type": "INTEGER", "primary_key": 1}]},
    {"op": "create_table", "table": "audit", "columns": [{"name": "item", "type": "INTEGER"}]}"#;

const TRIGGER_SQL: &str =
    "CREATE TRIGGER items_audit AFTER INSERT ON items BEGIN INSERT INTO audit VALUES (new.id); END";

fn trigger_sql(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'items_audit'",
        [],
        |r| r.get(0),
    )
    .ok()
}

/// Inserts an item and returns how many audit rows that insert added.
fn audited(conn: &Connection, id: i64) -> i64 {
    let count = |conn: &Connection| -> i64 {
        conn.query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
            .expect("count")
    };
    let before = count(conn);
    conn.execute("INSERT INTO items VALUES (?1)", [id])
        .expect("insert");
    count(conn) - before
}

#[test]
fn a_created_trigger_is_verbatim_and_fires_and_a_dropped_one_comes_back_on_down() {
    let dir = TempDir::new().expect("tempdir");
    write(dir.path(), "20260101000001_tables.json", AUDITED);
    let fields = format!(r#""name": "items_audit", "table": "items", "sql": "{TRIGGER_SQL}""#);
    write(
        dir.path(),
        "20260101000002_trigger.json",
        &format!(r#"{{"op": "create_trigger", {fields}}}"#),
    );
    let mut ex = executor();
    up(&mut ex, &load(&dir), None).expect("up");
    assert_eq!(trigger_sql(ex.connection()).as_deref(), Some(TRIGGER_SQL));
    assert_eq!(audited(ex.connection(), 1), 1);

    down(&mut ex, &load(&dir), 1).expect("down drops it");
    assert_eq!(trigger_sql(ex.connection()), None);
    assert_eq!(audited(ex.connection(), 2), 0);

    up(&mut ex, &load(&dir), None).expect("up again");
    write(
        dir.path(),
        "20260101000003_drop.json",
        &format!(r#"{{"op": "drop_trigger", "definition": {{{fields}}}}}"#),
    );
    up(&mut ex, &load(&dir), None).expect("drop_trigger");
    assert_eq!(trigger_sql(ex.connection()), None);
    assert_eq!(audited(ex.connection(), 3), 0);

    down(&mut ex, &load(&dir), 1).expect("down makes it again");
    assert_eq!(trigger_sql(ex.connection()).as_deref(), Some(TRIGGER_SQL));
    assert_eq!(audited(ex.connection(), 4), 1);
}
