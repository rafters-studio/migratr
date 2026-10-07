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
    for prior in [true, false] {
        let mut fail_at = 0;
        loop {
            let inner = seeded(LIBRARY);
            inner
                .connection()
                .pragma_update(None, "foreign_keys", prior)
                .expect("set fk");
            let mut ex = FailAt {
                inner,
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
            assert_eq!(
                foreign_keys(ex.inner.connection()),
                prior,
                "position {fail_at}"
            );
            assert_eq!(
                query_i64(
                    ex.inner.connection(),
                    "SELECT count(*) FROM sqlite_temp_master"
                ),
                0,
                "position {fail_at} left temporary objects"
            );

            fail_at += 1;
            if fail_at == ex.statement_count {
                break;
            }
        }
        // The ledger create, the foreign-key baseline, the view drop, create, copy, drop,
        // rename, one index, one view, three triggers, the foreign-key check, and the ledger
        // insert.
        assert_eq!(fail_at, 14);
    }
}

const PARENTS_AND_KIDS: &str = "
    CREATE TABLE parents (id INTEGER PRIMARY KEY, code TEXT UNIQUE);
    CREATE TABLE kids (parent_id INTEGER REFERENCES parents (id));
    INSERT INTO parents VALUES (1, 'a');
    INSERT INTO kids VALUES (1);
";

#[test]
fn an_orphaning_insert_in_the_same_migration_is_refused_and_leaves_the_database_unchanged() {
    for prior in [true, false] {
        let mut ex = seeded(PARENTS_AND_KIDS);
        ex.connection()
            .pragma_update(None, "foreign_keys", prior)
            .expect("set fk");
        let before = dump(ex.connection());
        let (_dir, migrations) = migration(&format!(
            r#"{{"op": "raw_sql", "up": "INSERT INTO kids VALUES (7), (8)"}}, {}"#,
            drop_column("parents", "code")
        ));

        let err = up(&mut ex, &migrations, None).expect_err("violations");

        assert!(
            matches!(
                err,
                MigrateError::ForeignKeyViolation { ref table, rows: 2 } if table == "kids"
            ),
            "{err:?}"
        );
        assert_eq!(dump(ex.connection()), before);
        assert_eq!(foreign_keys(ex.connection()), prior);
    }
}

#[test]
fn violations_that_existed_before_the_migration_do_not_block_it() {
    let mut ex = seeded(PARENTS_AND_KIDS);
    ex.connection()
        .execute_batch("PRAGMA foreign_keys = OFF; INSERT INTO kids VALUES (7), (8); PRAGMA foreign_keys = ON;")
        .expect("orphans");
    let (_dir, migrations) = migration(&drop_column("parents", "code"));

    up(&mut ex, &migrations, None).expect("up");

    assert_eq!(query_i64(ex.connection(), "SELECT count(*) FROM kids"), 3);
    assert_eq!(
        query_i64(
            ex.connection(),
            "SELECT count(*) FROM pragma_table_info('parents')"
        ),
        1
    );
}

/// `gone` is UNIQUE, so only the rebuild can drop it.
const GRAPH: &str = "
    CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, gone TEXT UNIQUE);
    CREATE TABLE other (x);
    CREATE TABLE log (x);
    CREATE VIEW v1 AS SELECT id, name FROM t;
    CREATE VIEW v2 AS SELECT name FROM v1;
    CREATE TRIGGER on_other AFTER INSERT ON other BEGIN UPDATE t SET name = 'x'; END;
    CREATE TRIGGER via_view AFTER INSERT ON log BEGIN INSERT INTO other SELECT name FROM v2; END;
    INSERT INTO t (name) VALUES ('a');
";

#[test]
fn views_over_views_and_triggers_on_other_tables_survive_the_rebuild() {
    let mut ex = seeded(GRAPH);
    let (_dir, migrations) = migration(&drop_column("t", "gone"));

    up(&mut ex, &migrations, None).expect("up");

    let conn = ex.connection();
    conn.execute("INSERT INTO log VALUES (1)", [])
        .expect("chain of triggers and views runs");
    assert_eq!(query_i64(conn, "SELECT count(*) FROM other"), 1);
    assert_eq!(
        query_i64(conn, "SELECT count(*) FROM v2 WHERE name = 'x'"),
        1
    );
}

#[test]
fn a_recreated_object_naming_the_dropped_column_is_refused_and_nothing_changes() {
    let dependents = [
        (
            "CREATE TRIGGER tr AFTER UPDATE ON t BEGIN SELECT NEW.gone; END",
            "tr",
        ),
        (
            "CREATE TRIGGER tr AFTER UPDATE ON t WHEN NEW.gone > 0 BEGIN SELECT 1; END",
            "tr",
        ),
        ("CREATE VIEW v AS SELECT gone FROM t", "v"),
        (
            "CREATE VIEW v AS SELECT id FROM t;
             CREATE TRIGGER vt INSTEAD OF INSERT ON v BEGIN INSERT INTO t (gone) VALUES (1); END",
            "vt",
        ),
    ];
    for (dependent, object) in dependents {
        let mut ex = seeded(&format!(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, gone TEXT UNIQUE); {dependent}"
        ));
        let before = dump(ex.connection());
        let (_dir, migrations) = migration(&drop_column("t", "gone"));

        let err = up(&mut ex, &migrations, None).expect_err(dependent);

        assert!(
            matches!(
                err,
                MigrateError::ColumnInUse { ref table, ref column, object: ref o }
                    if table == "t" && column == "gone" && o == object
            ),
            "{dependent}: {err:?}"
        );
        assert_eq!(dump(ex.connection()), before, "{dependent}");
    }
}

#[test]
fn a_column_sqlite_can_drop_in_place_is_dropped_in_place() {
    let mut ex = FailAt {
        inner: seeded(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, plain TEXT, b TEXT);
             CREATE TABLE after (x);
             CREATE VIEW v AS SELECT id, b FROM t;",
        ),
        fail_at: usize::MAX,
        statement_count: 0,
    };
    let at = |ex: &FailAt| {
        query_i64(
            ex.inner.connection(),
            "SELECT rowid FROM sqlite_master WHERE name = 't'",
        )
    };
    let position = at(&ex);
    let (_dir, migrations) = migration(&drop_column("t", "plain"));

    up(&mut ex, &migrations, None).expect("up");

    // The ledger create, the drop, and the ledger insert: no rebuild, no foreign-key check.
    assert_eq!(ex.statement_count, 3);
    assert_eq!(at(&ex), position);
}

#[test]
fn an_in_place_drop_that_a_view_or_trigger_depends_on_is_refused_by_sqlite() {
    let mut ex = seeded(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, plain TEXT);
         CREATE VIEW v AS SELECT plain FROM t;",
    );
    let before = dump(ex.connection());
    let (_dir, migrations) = migration(&drop_column("t", "plain"));

    let err = up(&mut ex, &migrations, None).expect_err("refused");

    assert!(matches!(err, MigrateError::Apply { .. }), "{err:?}");
    assert_eq!(dump(ex.connection()), before);
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

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("columns")
}

#[test]
fn a_column_added_earlier_in_the_migration_is_in_the_rebuild() {
    let mut ex = seeded(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE, b TEXT);
         INSERT INTO t VALUES (1, 'u', 'b');",
    );
    let (_dir, migrations) = migration(&format!(
        r#"{{"op": "add_column", "table": "t", "column": {{"name": "c", "type": "TEXT"}}}},
           {{"op": "create_index", "name": "t_c", "table": "t", "columns": ["c"]}},
           {}"#,
        drop_column("t", "u")
    ));

    up(&mut ex, &migrations, None).expect("up");

    let conn = ex.connection();
    assert_eq!(columns(conn, "t"), ["id", "b", "c"]);
    assert_eq!(
        query_i64(
            conn,
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 't_c'"
        ),
        1
    );
    assert_eq!(query_i64(conn, "SELECT count(*) FROM t WHERE b = 'b'"), 1);
}

#[test]
fn a_child_table_dropped_earlier_in_the_migration_is_not_checked_after() {
    let mut ex = seeded(PARENTS_AND_KIDS);
    let (_dir, migrations) = migration(&format!(
        r#"{{"op": "drop_table", "table": "kids", "definition": {{"name": "kids", "columns": [], "sql": ""}}}},
           {}"#,
        drop_column("parents", "code")
    ));

    up(&mut ex, &migrations, None).expect("up");

    assert_eq!(columns(ex.connection(), "parents"), ["id"]);
    assert_eq!(
        query_i64(
            ex.connection(),
            "SELECT count(*) FROM sqlite_master WHERE name = 'kids'"
        ),
        0
    );
}

#[test]
fn a_renamed_table_and_column_are_rebuilt_under_their_new_names() {
    let mut ex = seeded(
        "CREATE TABLE parents (id INTEGER PRIMARY KEY, name TEXT, code TEXT UNIQUE);
         CREATE TABLE kids (parent_id INTEGER REFERENCES parents (id) ON DELETE CASCADE);
         CREATE INDEX parents_name ON parents (name);
         INSERT INTO parents VALUES (1, 'a', 'x');
         INSERT INTO kids VALUES (1);",
    );
    let (_dir, migrations) = migration(&format!(
        r#"{{"op": "rename_table", "from": "parents", "to": "people"}},
           {{"op": "rename_column", "table": "people", "from": "name", "to": "label"}},
           {}"#,
        drop_column("people", "code")
    ));

    up(&mut ex, &migrations, None).expect("up");

    let conn = ex.connection();
    assert_eq!(columns(conn, "people"), ["id", "label"]);
    let index: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'parents_name'",
            [],
            |r| r.get(0),
        )
        .expect("index");
    assert!(
        index.contains("people") && index.contains("label"),
        "{index}"
    );
    conn.execute("DELETE FROM people WHERE id = 1", [])
        .expect("delete");
    assert_eq!(query_i64(conn, "SELECT count(*) FROM kids"), 0);
}

#[test]
fn raw_sql_naming_the_table_before_a_rebuild_is_refused_and_nothing_changes() {
    let mut ex = seeded(LIBRARY);
    let before = dump(ex.connection());
    let (_dir, migrations) = migration(&format!(
        r#"{{"op": "raw_sql", "up": "CREATE VIEW late AS SELECT id FROM authors"}}, {}"#,
        drop_column("authors", "code")
    ));

    let err = up(&mut ex, &migrations, None).expect_err("refused");

    assert!(
        matches!(
            err,
            MigrateError::UntrackedChange { ref table, operation: 0, .. } if table == "authors"
        ),
        "{err:?}"
    );
    assert_eq!(dump(ex.connection()), before);
}

#[test]
fn a_view_that_an_earlier_rename_rewrote_blocks_the_rebuild() {
    let mut ex = seeded(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, gone TEXT UNIQUE);
         CREATE VIEW v AS SELECT name FROM t;",
    );
    let before = dump(ex.connection());
    let (_dir, migrations) = migration(&format!(
        r#"{{"op": "rename_column", "table": "t", "from": "name", "to": "label"}}, {}"#,
        drop_column("t", "gone")
    ));

    let err = up(&mut ex, &migrations, None).expect_err("refused");

    assert!(
        matches!(err, MigrateError::UntrackedChange { operation: 0, .. }),
        "{err:?}"
    );
    assert_eq!(dump(ex.connection()), before);
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
