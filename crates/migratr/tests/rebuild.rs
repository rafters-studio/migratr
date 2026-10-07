use std::fs;
use std::path::Path;

use migratr::{
    AtomicError, Executor, LedgerRow, MigrateError, Migration, RusqliteExecutor, SchemaSnapshot,
    load_dir, up,
};
use rusqlite::Connection;
use tempfile::TempDir;

mod common;
use common::{FailAt, dump};

/// Loads one migration with `ops` as its operations.
fn migration(ops: &str) -> (TempDir, Vec<Migration>) {
    let dir = TempDir::new().expect("tempdir");
    fs::write(
        dir.path().join("20260101000001_change.json"),
        format!(r#"{{"up": [{ops}]}}"#),
    )
    .expect("write migration");
    let migrations = load_dir(dir.path()).expect("load");
    (dir, migrations)
}

fn drop_column(table: &str, column: &str) -> String {
    format!(
        r#"{{"op": "drop_column", "table": "{table}", "column": {{"name": "{column}", "type": "TEXT"}}}}"#
    )
}

/// An executor over a database seeded with `schema`, with foreign_keys on.
fn seeded(schema: &str) -> RusqliteExecutor {
    let conn = Connection::open_in_memory().expect("open");
    conn.pragma_update(None, "foreign_keys", false)
        .expect("fk off");
    conn.execute_batch(schema).expect("seed");
    conn.pragma_update(None, "foreign_keys", true)
        .expect("fk on");
    RusqliteExecutor::new(conn)
}

fn query_i64(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect(sql)
}

fn foreign_keys(conn: &Connection) -> bool {
    conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .expect("pragma")
}

/// `code` is UNIQUE, so SQLite's own ALTER TABLE DROP COLUMN refuses it and only the rebuild
/// can drop it.
const LIBRARY: &str = "
    CREATE TABLE authors (
      id INTEGER PRIMARY KEY,
      name TEXT NOT NULL COLLATE NOCASE,
      code TEXT UNIQUE,
      updated INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE books (
      id INTEGER PRIMARY KEY,
      author_id INTEGER NOT NULL REFERENCES authors (id) ON DELETE CASCADE,
      title TEXT
    );
    CREATE TABLE audit (event TEXT);
    CREATE INDEX authors_name ON authors (name);
    CREATE TRIGGER authors_touch AFTER UPDATE OF name ON authors
      BEGIN INSERT INTO audit VALUES ('renamed ' || NEW.name); END;
    CREATE TRIGGER authors_insert AFTER INSERT ON authors
      BEGIN INSERT INTO audit VALUES ('inserted ' || NEW.name); END;
    CREATE VIEW author_names AS SELECT id, name FROM authors;
    CREATE TRIGGER author_names_insert INSTEAD OF INSERT ON author_names
      BEGIN INSERT INTO authors (id, name) VALUES (NEW.id, NEW.name); END;
    INSERT INTO authors (id, name, code) VALUES (1, 'ann', 'a1'), (2, 'bob', 'b2');
    INSERT INTO books (author_id, title) VALUES (1, 'x'), (1, 'y'), (2, 'z');
    DELETE FROM audit;
";

#[test]
fn dropping_a_column_keeps_the_index_trigger_view_and_cascade_working() {
    let mut ex = seeded(LIBRARY);
    let (_dir, migrations) = migration(&drop_column("authors", "code"));

    up(&mut ex, &migrations, None).expect("up");
    let conn = ex.connection();

    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('authors')")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("columns");
    assert_eq!(columns, ["id", "name", "updated"]);
    assert_eq!(query_i64(conn, "SELECT count(*) FROM books"), 3);

    // The index is back and the planner uses it.
    let plan: String = conn
        .query_row(
            "EXPLAIN QUERY PLAN SELECT id FROM authors WHERE name = 'ann'",
            [],
            |r| r.get(3),
        )
        .expect("plan");
    assert!(plan.contains("authors_name"), "{plan}");

    // The trigger fires.
    conn.execute("UPDATE authors SET name = 'anne' WHERE id = 1", [])
        .expect("update");
    assert_eq!(
        query_i64(
            conn,
            "SELECT count(*) FROM audit WHERE event = 'renamed anne'"
        ),
        1
    );

    // The view reads the table and its INSTEAD OF trigger writes through it.
    conn.execute("INSERT INTO author_names VALUES (3, 'cy')", [])
        .expect("insert through view");
    assert_eq!(query_i64(conn, "SELECT count(*) FROM author_names"), 3);

    // The child's ON DELETE CASCADE still fires.
    conn.execute("DELETE FROM authors WHERE id = 1", [])
        .expect("delete");
    assert_eq!(query_i64(conn, "SELECT count(*) FROM books"), 1);
}

#[test]
fn a_trigger_on_the_table_does_not_fire_during_the_row_copy() {
    let mut ex = seeded(LIBRARY);
    let (_dir, migrations) = migration(&drop_column("authors", "code"));

    up(&mut ex, &migrations, None).expect("up");

    let conn = ex.connection();
    assert_eq!(query_i64(conn, "SELECT count(*) FROM audit"), 0);
    conn.execute("INSERT INTO authors (name) VALUES ('dee')", [])
        .expect("insert");
    assert_eq!(query_i64(conn, "SELECT count(*) FROM audit"), 1);
}

#[test]
fn an_autoincrement_table_keeps_its_sqlite_sequence_value() {
    let mut ex = seeded(
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT, code TEXT UNIQUE);
         INSERT INTO t (v) VALUES ('a'), ('b'), ('c');
         DELETE FROM t WHERE id > 1;",
    );
    let (_dir, migrations) = migration(&drop_column("t", "code"));

    up(&mut ex, &migrations, None).expect("up");

    let conn = ex.connection();
    assert_eq!(
        query_i64(conn, "SELECT seq FROM sqlite_sequence WHERE name = 't'"),
        3
    );
    assert_eq!(
        query_i64(
            conn,
            "SELECT count(*) FROM sqlite_sequence WHERE name = 't'"
        ),
        1
    );
    conn.execute("INSERT INTO t (v) VALUES ('d')", [])
        .expect("insert");
    assert_eq!(query_i64(conn, "SELECT max(id) FROM t"), 4);
}

#[test]
fn a_typeless_column_keeps_no_declared_type() {
    let mut ex = seeded(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, loose, code TEXT UNIQUE);
         INSERT INTO t (loose) VALUES (1), ('1'), (x'01'), (1.5);",
    );
    let (_dir, migrations) = migration(&drop_column("t", "code"));

    up(&mut ex, &migrations, None).expect("up");

    let conn = ex.connection();
    let declared: String = conn
        .query_row(
            "SELECT type FROM pragma_table_info('t') WHERE name = 'loose'",
            [],
            |r| r.get(0),
        )
        .expect("type");
    assert_eq!(declared, "");
    let types: Vec<String> = conn
        .prepare("SELECT typeof(loose) FROM t ORDER BY id")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("types");
    assert_eq!(types, ["integer", "text", "blob", "real"]);
}

#[test]
fn foreign_keys_is_restored_to_its_prior_value_on_success() {
    for prior in [true, false] {
        let mut ex = seeded(LIBRARY);
        ex.connection()
            .pragma_update(None, "foreign_keys", prior)
            .expect("set fk");
        let (_dir, migrations) = migration(&drop_column("authors", "code"));

        up(&mut ex, &migrations, None).expect("up");

        assert_eq!(foreign_keys(ex.connection()), prior);
        assert_eq!(query_i64(ex.connection(), "SELECT count(*) FROM books"), 3);
    }
}

#[test]
fn a_failure_at_any_rebuild_statement_changes_nothing_and_restores_foreign_keys() {
    let (_dir, migrations) = migration(&drop_column("authors", "code"));
    let mut fail_at = 0;
    loop {
        let mut ex = FailAt {
            inner: seeded(LIBRARY),
            fail_at,
            statement_count: 0,
        };
        let before = dump(ex.inner.connection());

        let err = up(&mut ex, &migrations, None).expect_err("injected failure");

        assert!(
            matches!(err, MigrateError::Apply { .. }),
            "position {fail_at}: {err:?}"
        );
        assert_eq!(
            dump(ex.inner.connection()),
            before,
            "position {fail_at} changed the database"
        );
        assert!(foreign_keys(ex.inner.connection()), "position {fail_at}");

        fail_at += 1;
        if fail_at == ex.statement_count {
            break;
        }
    }
    // The ledger create, the view drop, create, copy, drop, rename, one index, one view,
    // three triggers, the foreign-key check, and the ledger insert.
    assert_eq!(fail_at, 13);
}

#[test]
fn a_rebuild_that_breaks_foreign_keys_is_refused_with_the_row_count() {
    let mut ex = seeded(
        "CREATE TABLE parents (id INTEGER PRIMARY KEY, code TEXT UNIQUE);
         CREATE TABLE kids (parent_id INTEGER REFERENCES parents (id));
         INSERT INTO parents VALUES (1, 'a');
         INSERT INTO kids VALUES (1), (7), (8);",
    );
    let before = dump(ex.connection());
    let (_dir, migrations) = migration(&drop_column("parents", "code"));

    let err = up(&mut ex, &migrations, None).expect_err("violations");

    assert!(
        matches!(
            err,
            MigrateError::ForeignKeyViolation { ref table, rows: 2 } if table == "parents"
        ),
        "{err:?}"
    );
    assert_eq!(dump(ex.connection()), before);
    assert!(foreign_keys(ex.connection()));
}

/// A real executor whose schema reads show `table` with a CREATE statement that does not
/// parse.
struct Mangled {
    inner: RusqliteExecutor,
    ran: bool,
}

impl Executor for Mangled {
    type Error = rusqlite::Error;

    fn read_schema(&mut self) -> Result<SchemaSnapshot, Self::Error> {
        let mut schema = self.inner.read_schema()?;
        for object in &mut schema.objects {
            if object.name == "authors" {
                object.sql = Some("CREATE TABLE authors (id INTEGER PRIMARY KEY, code".into());
            }
        }
        Ok(schema)
    }

    fn read_ledger(&mut self) -> Result<Vec<LedgerRow>, Self::Error> {
        self.inner.read_ledger()
    }

    fn run_atomic(
        &mut self,
        statements: &[String],
        suspend_foreign_keys: bool,
    ) -> Result<(), AtomicError<Self::Error>> {
        self.ran = true;
        self.inner.run_atomic(statements, suspend_foreign_keys)
    }

    fn snapshot(&mut self, path: &Path) -> Result<bool, Self::Error> {
        self.inner.snapshot(path)
    }
}

#[test]
fn an_unparseable_create_table_is_refused_by_name_and_nothing_changes() {
    let mut ex = Mangled {
        inner: seeded(LIBRARY),
        ran: false,
    };
    let before = dump(ex.inner.connection());
    let (_dir, migrations) = migration(&format!(
        r#"{{"op": "raw_sql", "up": "INSERT INTO audit VALUES ('first')"}}, {}"#,
        drop_column("authors", "code")
    ));

    let err = up(&mut ex, &migrations, None).expect_err("refused");

    assert!(
        matches!(err, MigrateError::UnparseableTable { ref table } if table == "authors"),
        "{err:?}"
    );
    assert!(err.to_string().contains("authors"), "{err}");
    assert!(!ex.ran);
    assert_eq!(dump(ex.inner.connection()), before);
}

#[test]
fn a_table_changed_earlier_in_the_migration_drops_in_place_without_losing_the_change() {
    let mut ex = seeded("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)");
    let (_dir, migrations) = migration(&format!(
        r#"{{"op": "add_column", "table": "t", "column": {{"name": "c", "type": "TEXT"}}}},
           {{"op": "create_index", "name": "t_c", "table": "t", "columns": ["c"]}},
           {}"#,
        drop_column("t", "a")
    ));

    up(&mut ex, &migrations, None).expect("up");

    let conn = ex.connection();
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('t')")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("columns");
    assert_eq!(columns, ["id", "b", "c"]);
    assert_eq!(
        query_i64(
            conn,
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 't_c'"
        ),
        1
    );
}

#[test]
fn only_the_ledger_remains_beside_the_rebuilt_schema() {
    let mut ex = seeded(LIBRARY);
    let before: Vec<String> = names(ex.connection());
    let (_dir, migrations) = migration(&drop_column("authors", "code"));

    up(&mut ex, &migrations, None).expect("up");

    let mut expected = before;
    expected.push("_migratr_migrations".to_string());
    expected.sort();
    assert_eq!(names(ex.connection()), expected);
    assert_eq!(
        query_i64(ex.connection(), "SELECT count(*) FROM sqlite_temp_master"),
        0
    );
}

fn names(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT name FROM sqlite_master WHERE name NOT LIKE 'sqlite_autoindex%' ORDER BY name",
    )
    .expect("prepare")
    .query_map([], |r| r.get(0))
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("names")
}
