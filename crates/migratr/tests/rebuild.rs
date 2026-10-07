use std::fs;

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
        // The ledger create, the view drop, create, copy, drop,
        // rename, one index, one view, three triggers, the foreign-key check, and the ledger
        // insert.
        assert_eq!(fail_at, 13);
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
            r#"{}, {{"op": "raw_sql", "up": "INSERT INTO kids VALUES (7), (8)"}}"#,
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
fn violations_that_existed_before_the_migration_refuse_it_with_the_exact_count() {
    let mut ex = seeded(PARENTS_AND_KIDS);
    ex.connection()
        .execute_batch("PRAGMA foreign_keys = OFF; INSERT INTO kids VALUES (7), (8); PRAGMA foreign_keys = ON;")
        .expect("orphans");
    let before = dump(ex.connection());
    let (_dir, migrations) = migration(&drop_column("parents", "code"));

    let err = up(&mut ex, &migrations, None).expect_err("violations");

    assert!(
        matches!(
            err,
            MigrateError::ForeignKeyViolation { ref table, rows: 2 } if table == "kids"
        ),
        "{err:?}"
    );
    assert_eq!(dump(ex.connection()), before);
    assert!(foreign_keys(ex.connection()));
}

#[test]
fn several_new_orphans_in_a_without_rowid_child_are_all_counted() {
    let mut ex = seeded(
        "CREATE TABLE parents (id INTEGER PRIMARY KEY, code TEXT UNIQUE);
         CREATE TABLE kids (k TEXT PRIMARY KEY, parent_id INTEGER REFERENCES parents (id)) WITHOUT ROWID;
         INSERT INTO parents VALUES (1, 'a');",
    );
    let (_dir, migrations) = migration(&format!(
        r#"{}, {{"op": "raw_sql", "up": "INSERT INTO kids VALUES ('a', 7), ('b', 7), ('c', 8)"}}"#,
        drop_column("parents", "code")
    ));

    let err = up(&mut ex, &migrations, None).expect_err("violations");

    assert!(
        matches!(
            err,
            MigrateError::ForeignKeyViolation { ref table, rows: 3 } if table == "kids"
        ),
        "{err:?}"
    );
    assert_eq!(query_i64(ex.connection(), "SELECT count(*) FROM kids"), 0);
}

#[test]
fn a_column_named_by_a_constraint_or_another_column_is_refused_and_nothing_changes() {
    let tables = [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, gone TEXT, UNIQUE (gone))",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, gone INTEGER, FOREIGN KEY (gone) REFERENCES t (id))",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, gone INTEGER, CHECK (gone > 0))",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, gone INTEGER, other INTEGER CHECK (other > gone))",
        "CREATE TABLE t (id INTEGER PRIMARY KEY, gone INTEGER, twice INTEGER GENERATED ALWAYS AS (gone * 2))",
    ];
    for table in tables {
        let mut ex = seeded(table);
        let before = dump(ex.connection());
        let (_dir, migrations) = migration(&drop_column("t", "gone"));

        let err = up(&mut ex, &migrations, None).expect_err(table);

        assert!(
            matches!(
                err,
                MigrateError::ColumnInUse { ref table, ref column, ref object }
                    if table == "t" && column == "gone" && object.contains("gone")
            ),
            "{table}: {err:?}"
        );
        assert_eq!(dump(ex.connection()), before, "{table}");
    }
}

#[test]
fn a_column_that_a_child_table_references_is_refused_naming_the_child() {
    for parent in [
        "CREATE TABLE parents (id INTEGER PRIMARY KEY, code TEXT UNIQUE)",
        "CREATE TABLE parents (code TEXT PRIMARY KEY, id INTEGER)",
    ] {
        let mut ex = seeded(&format!(
            "{parent}; CREATE TABLE kids (c TEXT REFERENCES parents (code))"
        ));
        let before = dump(ex.connection());
        let (_dir, migrations) = migration(&drop_column("parents", "code"));

        let err = up(&mut ex, &migrations, None).expect_err(parent);

        assert!(
            matches!(
                err,
                MigrateError::ColumnInUse { ref table, ref object, .. }
                    if table == "parents" && object == "kids"
            ),
            "{parent}: {err:?}"
        );
        assert_eq!(dump(ex.connection()), before, "{parent}");
    }
}

#[test]
fn a_column_named_like_a_keyword_or_a_type_is_dropped_when_nothing_uses_it() {
    let mut ex = seeded(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, key TEXT UNIQUE);
         CREATE TABLE t (
           id INTEGER PRIMARY KEY,
           key TEXT UNIQUE,
           text TEXT,
           owner TEXT REFERENCES users (key),
           UNIQUE (owner)
         );
         INSERT INTO t (key, text) VALUES ('k', 'x');",
    );
    let (_dir, migrations) = migration(&drop_column("t", "key"));

    up(&mut ex, &migrations, None).expect("up");

    assert_eq!(columns(ex.connection(), "t"), ["id", "text", "owner"]);
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
        // These use the column by position, without naming it.
        (
            "CREATE TABLE src (x);
             CREATE TRIGGER pos AFTER INSERT ON src BEGIN INSERT INTO t VALUES (NEW.x, 'a'); END",
            "pos",
        ),
        (
            "CREATE TABLE src (x);
             CREATE TRIGGER pos AFTER INSERT ON src BEGIN INSERT INTO t SELECT NEW.x, 'a'; END",
            "pos",
        ),
        ("CREATE VIEW tv (a, b) AS SELECT * FROM t", "tv"),
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

    fn snapshot(&mut self, file_name: &str) -> Result<Option<std::path::PathBuf>, Self::Error> {
        self.inner.snapshot(file_name)
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

/// Runs `ops` then a rebuild-only drop of `t.gone` against `schema` and expects the drop,
/// operation `operation`, to be refused with the database unchanged.
fn assert_refused_after(schema: &str, ops: &str, operation: usize) {
    let mut ex = seeded(schema);
    let before = dump(ex.connection());
    let (_dir, migrations) = migration(&format!("{ops}, {}", drop_column("t", "gone")));

    let err = up(&mut ex, &migrations, None).expect_err(ops);

    assert!(
        matches!(
            err,
            MigrateError::RebuildNotFirst { ref table, ref column, operation: n, .. }
                if table == "t" && column == "gone" && n == operation
        ),
        "{ops}: {err:?}"
    );
    assert!(err.to_string().contains("own migration"), "{err}");
    assert_eq!(dump(ex.connection()), before, "{ops}");
}

#[test]
fn a_rebuild_after_an_earlier_operation_changed_the_table_is_refused() {
    let schema = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, gone TEXT UNIQUE)";
    for ops in [
        r#"{"op": "add_column", "table": "t", "column": {"name": "c", "type": "TEXT"}}"#,
        r#"{"op": "create_index", "name": "t_name", "table": "t", "columns": ["name"]}"#,
        r#"{"op": "rename_column", "table": "t", "from": "name", "to": "label"}"#,
        r#"{"op": "rename_table", "from": "t", "to": "u"}"#,
        r#"{"op": "drop_column", "table": "t", "column": {"name": "name", "type": "TEXT"}}"#,
    ] {
        assert_refused_after(schema, ops, 1);
    }
}

#[test]
fn dropping_a_column_after_dropping_its_index_runs_in_place() {
    // The drop would need a rebuild only for the index, which the earlier operation removes.
    let mut ex = seeded(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, gone TEXT, b TEXT); CREATE INDEX t_gone ON t (gone);",
    );
    let (_dir, migrations) = migration(&format!(
        r#"{{"op": "drop_index", "definition": {{"name": "t_gone", "table": "t", "columns": ["gone"]}}}}, {}"#,
        drop_column("t", "gone")
    ));

    up(&mut ex, &migrations, None).expect("up");

    assert_eq!(columns(ex.connection(), "t"), ["id", "b"]);
}

#[test]
fn a_unique_column_of_a_table_created_or_renamed_earlier_is_refused_by_sqlite_in_place() {
    for (ops, schema) in [
        (
            r#"{"op": "create_table", "table": "t", "columns": [{"name": "id", "type": "INTEGER", "primary_key": 1}, {"name": "gone", "type": "TEXT", "unique": true}]}"#,
            "",
        ),
        (
            r#"{"op": "rename_table", "from": "old", "to": "t"}"#,
            "CREATE TABLE old (id INTEGER PRIMARY KEY, gone TEXT UNIQUE)",
        ),
    ] {
        let mut ex = seeded(schema);
        let before = dump(ex.connection());
        let (_dir, migrations) = migration(&format!("{ops}, {}", drop_column("t", "gone")));

        let err = up(&mut ex, &migrations, None).expect_err(ops);

        assert!(
            matches!(
                err,
                MigrateError::Apply {
                    operation: Some(1),
                    ..
                }
            ),
            "{ops}: {err:?}"
        );
        assert_eq!(dump(ex.connection()), before, "{ops}");
    }
}

#[test]
fn a_rebuild_after_an_earlier_operation_changed_a_recreated_object_is_refused() {
    let schema = "CREATE TABLE t (id INTEGER PRIMARY KEY, gone TEXT UNIQUE);
         CREATE TABLE log (x);
         CREATE INDEX t_id ON t (id);
         CREATE VIEW v AS SELECT id FROM t;
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (1); END";
    for ops in [
        r#"{"op": "drop_index", "definition": {"name": "t_id", "table": "t", "columns": ["id"]}}"#,
        r#"{"op": "rename_table", "from": "log", "to": "history"}"#,
        r#"{"op": "raw_sql", "up": "DROP VIEW v"}"#,
        r#"{"op": "raw_sql", "up": "ALTER TABLE log RENAME TO history"}"#,
    ] {
        assert_refused_after(schema, ops, 1);
    }
}

#[test]
fn a_refused_drop_succeeds_in_its_own_following_migration() {
    let mut ex = seeded(PARENTS_AND_KIDS);
    let drop_kids = r#"{"op": "drop_table", "table": "kids", "definition": {"name": "kids", "columns": [], "sql": ""}}"#;
    let (_dir, together) = migration(&format!("{drop_kids}, {}", drop_column("parents", "code")));
    let err = up(&mut ex, &together, None).expect_err("refused");
    assert!(
        matches!(err, MigrateError::RebuildNotFirst { operation: 1, .. }),
        "{err:?}"
    );

    let dir = TempDir::new().expect("tempdir");
    fs::write(
        dir.path().join("20260101000001_drop_kids.json"),
        format!(r#"{{"up": [{drop_kids}]}}"#),
    )
    .expect("write");
    fs::write(
        dir.path().join("20260101000002_drop_code.json"),
        format!(r#"{{"up": [{}]}}"#, drop_column("parents", "code")),
    )
    .expect("write");
    let split = load_dir(dir.path()).expect("load");

    up(&mut ex, &split, None).expect("up");

    assert_eq!(columns(ex.connection(), "parents"), ["id"]);
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
            MigrateError::RebuildNotFirst { ref table, operation: 1, .. } if table == "authors"
        ),
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
