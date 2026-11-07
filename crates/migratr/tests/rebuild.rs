use std::fs;

use migratr::{
    AtomicError, Executor, LedgerRow, MigrateError, Migration, RusqliteExecutor, SchemaSnapshot,
    load_dir, rebuild_tables, up,
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
        // The ledger create, the view drop, then for authors and its child books: two
        // creates, two copies, two drops and two renames; one index, one view, three
        // triggers, and the ledger insert.
        assert_eq!(fail_at, 16);
    }
}

const PARENTS_AND_KIDS: &str = "
    CREATE TABLE parents (id INTEGER PRIMARY KEY, code TEXT UNIQUE);
    CREATE TABLE kids (parent_id INTEGER REFERENCES parents (id));
    INSERT INTO parents VALUES (1, 'a');
    INSERT INTO kids VALUES (1);
";

#[test]
fn an_orphaning_insert_in_the_same_migration_is_refused_by_the_foreign_key_and_changes_nothing() {
    let mut ex = seeded(PARENTS_AND_KIDS);
    let before = dump(ex.connection());
    let (_dir, migrations) = migration(&format!(
        r#"{}, {{"op": "raw_sql", "up": "INSERT INTO kids VALUES (7), (8)"}}"#,
        drop_column("parents", "code")
    ));

    let err = up(&mut ex, &migrations, None).expect_err("refused");

    assert!(
        matches!(
            err,
            MigrateError::Apply {
                operation: Some(1),
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(dump(ex.connection()), before);
    assert!(foreign_keys(ex.connection()));
}

/// Leaves orphans in `kids`, which needs foreign keys off to insert them.
fn orphan(ex: &RusqliteExecutor, rows: &str) {
    ex.connection()
        .execute_batch(&format!(
            "PRAGMA foreign_keys = OFF; INSERT INTO kids VALUES {rows}; PRAGMA foreign_keys = ON;"
        ))
        .expect("orphans");
}

#[test]
fn violations_that_existed_before_the_migration_refuse_it_before_any_change_with_the_exact_count() {
    for prior in [true, false] {
        let inner = seeded(PARENTS_AND_KIDS);
        orphan(&inner, "(7), (8)");
        inner
            .connection()
            .pragma_update(None, "foreign_keys", prior)
            .expect("set fk");
        let mut ex = Recorded {
            inner,
            sent: Vec::new(),
            snapshots: 0,
        };
        let before = dump(ex.inner.connection());
        let (_dir, migrations) = migration(&drop_column("parents", "code"));

        let err = up(&mut ex, &migrations, None).expect_err("violations");

        assert!(
            matches!(
                err,
                MigrateError::ForeignKeyViolation { ref table, rows: 2 } if table == "kids"
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("kids") && err.to_string().contains('2'));
        assert!(
            ex.sent.is_empty() && ex.snapshots == 0,
            "asked for a change"
        );
        assert_eq!(dump(ex.inner.connection()), before);
        assert_eq!(foreign_keys(ex.inner.connection()), prior);
    }
}

#[test]
fn orphans_in_a_without_rowid_child_are_counted_one_per_row() {
    let mut ex = seeded(
        "CREATE TABLE parents (id INTEGER PRIMARY KEY, code TEXT UNIQUE);
         CREATE TABLE kids (k TEXT PRIMARY KEY, parent_id INTEGER REFERENCES parents (id)) WITHOUT ROWID;
         INSERT INTO parents VALUES (1, 'a');",
    );
    orphan(&ex, "('a', 7), ('b', 7), ('c', 8)");
    let (_dir, migrations) = migration(&drop_column("parents", "code"));

    let err = up(&mut ex, &migrations, None).expect_err("violations");

    assert!(
        matches!(
            err,
            MigrateError::ForeignKeyViolation { ref table, rows: 3 } if table == "kids"
        ),
        "{err:?}"
    );
    assert_eq!(query_i64(ex.connection(), "SELECT count(*) FROM kids"), 3);
}

/// The table and column each foreign key of `table` names, by table.
fn references(conn: &Connection, table: &str) -> Vec<(String, String)> {
    conn.prepare(&format!(
        "SELECT \"table\", \"to\" FROM pragma_foreign_key_list('{table}') ORDER BY 1"
    ))
    .expect("prepare")
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("references")
}

const FAMILY: &str = "
    CREATE TABLE orders (id INTEGER PRIMARY KEY, code TEXT UNIQUE, previous INTEGER REFERENCES orders (id));
    CREATE TABLE products (id INTEGER PRIMARY KEY);
    CREATE TABLE order_details (
      order_id INTEGER NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
      product_id INTEGER REFERENCES products (id),
      qty INTEGER
    );
    CREATE INDEX order_details_order ON order_details (order_id);
    INSERT INTO products VALUES (1), (2);
    INSERT INTO orders VALUES (1, 'a', NULL), (2, 'b', 1), (3, 'c', 2);
    INSERT INTO order_details VALUES (1, 1, 5), (2, 2, 6), (3, 1, 7), (3, 2, 8);
";

#[test]
fn a_referenced_table_rebuilds_with_its_family() {
    for prior in [true, false] {
        let mut ex = seeded(FAMILY);
        ex.connection()
            .pragma_update(None, "foreign_keys", prior)
            .expect("set fk");
        let (_dir, migrations) = migration(&drop_column("orders", "code"));

        up(&mut ex, &migrations, None).expect("up");

        let conn = ex.connection();
        assert_eq!(columns(conn, "orders"), ["id", "previous"]);
        assert_eq!(query_i64(conn, "SELECT count(*) FROM orders"), 3);
        assert_eq!(query_i64(conn, "SELECT count(*) FROM order_details"), 4);
        assert_eq!(
            references(conn, "order_details"),
            [
                ("orders".to_string(), "id".to_string()),
                ("products".to_string(), "id".to_string())
            ]
        );
        assert_eq!(
            references(conn, "orders"),
            [("orders".to_string(), "id".to_string())]
        );
        assert_eq!(
            query_i64(
                conn,
                "SELECT count(*) FROM sqlite_master WHERE name LIKE '%migratr_rebuild%'"
            ),
            0
        );
        assert_eq!(
            query_i64(conn, "SELECT count(*) FROM pragma_foreign_key_check"),
            0
        );
        assert!(names(conn).contains(&"order_details_order".to_string()));
        assert_eq!(foreign_keys(conn), prior);

        // The child's key works against the final table.
        conn.execute_batch("PRAGMA foreign_keys = ON; DELETE FROM orders WHERE id = 3;")
            .expect("cascade");
        assert_eq!(query_i64(conn, "SELECT count(*) FROM order_details"), 2);
        assert!(
            conn.execute("INSERT INTO order_details VALUES (9, 1, 1)", [])
                .is_err()
        );
    }
}

/// An executor that behaves as D1 does toward a rebuild: it refuses to turn foreign keys off,
/// any TEMP object and any `PRAGMA foreign_keys`, runs with foreign keys on, and records the
/// statements it receives.
struct D1Like {
    inner: RusqliteExecutor,
    sent: Vec<String>,
}

impl Executor for D1Like {
    type Error = rusqlite::Error;

    fn read_schema(&mut self) -> Result<SchemaSnapshot, Self::Error> {
        self.inner.read_schema()
    }

    fn read_ledger(&mut self) -> Result<Vec<LedgerRow>, Self::Error> {
        self.inner.read_ledger()
    }

    fn run_atomic(
        &mut self,
        statements: &[String],
        suspend_foreign_keys: bool,
    ) -> Result<(), AtomicError<Self::Error>> {
        let refused = |what: &str| {
            AtomicError::outside_statements(rusqlite::Error::InvalidParameterName(what.into()))
        };
        if suspend_foreign_keys {
            return Err(refused("foreign keys cannot be turned off"));
        }
        for statement in statements {
            let upper = statement.to_ascii_uppercase();
            if upper.contains("TEMP ") || upper.contains("TEMPORARY ") {
                return Err(refused(statement));
            }
            if upper.contains("PRAGMA") && upper.contains("FOREIGN_KEYS") {
                return Err(refused(statement));
            }
        }
        self.sent.extend_from_slice(statements);
        self.inner.run_atomic(statements, false)
    }

    fn snapshot(&mut self, _file_name: &str) -> Result<Option<std::path::PathBuf>, Self::Error> {
        Ok(None)
    }
}

/// Records the statements and snapshot requests a plain executor receives.
struct Recorded {
    inner: RusqliteExecutor,
    sent: Vec<String>,
    snapshots: usize,
}

impl Executor for Recorded {
    type Error = rusqlite::Error;

    fn read_schema(&mut self) -> Result<SchemaSnapshot, Self::Error> {
        self.inner.read_schema()
    }

    fn read_ledger(&mut self) -> Result<Vec<LedgerRow>, Self::Error> {
        self.inner.read_ledger()
    }

    fn foreign_key_violations(
        &mut self,
        tables: &[&str],
    ) -> Result<Option<Vec<(String, u64)>>, Self::Error> {
        self.inner.foreign_key_violations(tables)
    }

    fn run_atomic(
        &mut self,
        statements: &[String],
        suspend_foreign_keys: bool,
    ) -> Result<(), AtomicError<Self::Error>> {
        self.sent.extend_from_slice(statements);
        self.inner.run_atomic(statements, suspend_foreign_keys)
    }

    fn snapshot(&mut self, file_name: &str) -> Result<Option<std::path::PathBuf>, Self::Error> {
        self.snapshots += 1;
        self.inner.snapshot(file_name)
    }
}

#[test]
fn the_same_rebuild_statements_run_on_a_file_and_on_a_place_like_d1() {
    for (schema, table, column) in [
        (LIBRARY, "authors", "code"),
        (FAMILY, "orders", "code"),
        (GRAPH, "t", "gone"),
    ] {
        let dir = TempDir::new().expect("tempdir");
        let conn = Connection::open(dir.path().join("local.db")).expect("open file");
        conn.execute_batch(schema).expect("seed");
        let mut file = Recorded {
            inner: RusqliteExecutor::new(conn),
            sent: Vec::new(),
            snapshots: 0,
        };
        let mut place = D1Like {
            inner: seeded(schema),
            sent: Vec::new(),
        };
        let (_m, migrations) = migration(&drop_column(table, column));

        up(&mut file, &migrations, None).expect("file");
        up(&mut place, &migrations, None).expect("place");

        assert_eq!(file.sent, place.sent, "{table}");
        // The ledger rows differ only in when they were applied.
        let state = |conn: &Connection| -> Vec<String> {
            dump(conn)
                .into_iter()
                .filter(|line| !line.starts_with("Integer(20260101000001)"))
                .collect()
        };
        assert_eq!(
            state(file.inner.connection()),
            state(place.inner.connection()),
            "{table}"
        );
    }
}

#[test]
fn a_place_refuses_rows_a_foreign_key_refuses_at_the_copy_and_nothing_changes() {
    let mut place = D1Like {
        inner: seeded(FAMILY),
        sent: Vec::new(),
    };
    place
        .inner
        .connection()
        .execute_batch(
            "PRAGMA foreign_keys = OFF; INSERT INTO order_details VALUES (9, 1, 1); PRAGMA foreign_keys = ON;",
        )
        .expect("orphan");
    let before = dump(place.inner.connection());
    let (_dir, migrations) = migration(&drop_column("orders", "code"));

    let err = up(&mut place, &migrations, None).expect_err("refused");

    assert!(matches!(err, MigrateError::Apply { .. }), "{err:?}");
    assert_eq!(dump(place.inner.connection()), before);
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

/// What `rebuild_tables` reports for `ops` on a database seeded with `schema`.
fn reported(schema: &str, ops: &str) -> Vec<String> {
    let mut ex = seeded(schema);
    let snapshot = ex.read_schema().expect("schema");
    let (_dir, migrations) = migration(ops);
    rebuild_tables(&snapshot, &migrations[0]).expect("reported")
}

const REPORTED_SCHEMA: &str =
    "CREATE TABLE t (id INTEGER PRIMARY KEY, gone TEXT UNIQUE, plain TEXT);
     CREATE TABLE u (id INTEGER PRIMARY KEY, gone TEXT UNIQUE, indexed TEXT);
     CREATE INDEX u_indexed ON u (indexed);";

#[test]
fn a_dropped_column_is_reported_only_when_it_needs_the_rebuild() {
    assert_eq!(reported(REPORTED_SCHEMA, &drop_column("t", "gone")), ["t"]);
    assert!(reported(REPORTED_SCHEMA, &drop_column("t", "plain")).is_empty());
}

#[test]
fn an_added_column_is_reported_only_when_it_needs_the_rebuild() {
    let add = |column: &str| {
        format!(
            r#"{{"op": "add_column", "table": "t", "column": {{"name": "c", "type": "TEXT"{column}}}}}"#
        )
    };
    assert_eq!(
        reported(REPORTED_SCHEMA, &add(r#", "unique": true"#)),
        ["t"]
    );
    assert!(reported(REPORTED_SCHEMA, &add("")).is_empty());
}

#[test]
fn each_rebuilt_table_is_reported_once_in_operation_order() {
    let ops = format!(
        "{}, {}, {}",
        drop_column("u", "gone"),
        drop_column("t", "gone"),
        drop_column("u", "indexed"),
    );
    assert_eq!(reported(REPORTED_SCHEMA, &ops), ["u", "t"]);
}

#[test]
fn an_index_the_migration_dropped_first_does_not_count() {
    let ops = format!(
        r#"{{"op": "drop_index", "definition": {{"name": "u_indexed", "table": "u", "columns": ["indexed"]}}}}, {}"#,
        drop_column("u", "indexed"),
    );
    assert!(reported(REPORTED_SCHEMA, &ops).is_empty());
}

const SHOP: &str = "
    CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT);
    CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, note TEXT);
    CREATE INDEX orders_customer ON orders (customer_id);
    INSERT INTO customers VALUES (1, 'ann'), (2, 'bob');
    INSERT INTO orders VALUES (10, 1, 'x'), (11, 2, 'y'), (12, NULL, 'z');
";

fn add_foreign_key(table: &str, column: &str, parent: &str) -> String {
    format!(
        r#"{{"op": "add_foreign_key", "table": "{table}", "columns": ["{column}"],
            "references": {{"table": "{parent}", "columns": ["id"]}}, "on_delete": "CASCADE"}}"#
    )
}

fn table_sql(conn: &Connection, table: &str) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )
    .expect("table sql")
}

#[test]
fn an_added_foreign_key_is_in_the_create_statement_keeps_rows_and_indexes_and_is_enforced() {
    let mut ex = seeded(SHOP);
    let (_dir, migrations) = migration(&add_foreign_key("orders", "customer_id", "customers"));
    let schema = ex.read_schema().expect("schema");
    assert_eq!(
        rebuild_tables(&schema, &migrations[0]).expect("tables"),
        ["orders"]
    );

    up(&mut ex, &migrations, None).expect("up");

    let conn = ex.connection();
    assert_eq!(
        table_sql(conn, "orders"),
        "CREATE TABLE \"orders\" (id INTEGER PRIMARY KEY, customer_id INTEGER, note TEXT, \
         FOREIGN KEY (\"customer_id\") REFERENCES \"customers\" (\"id\") ON DELETE CASCADE)"
    );
    assert_eq!(query_i64(conn, "SELECT count(*) FROM orders"), 3);
    assert_eq!(
        query_i64(
            conn,
            "SELECT count(*) FROM sqlite_master WHERE name = 'orders_customer'"
        ),
        1
    );
    assert!(
        conn.execute("INSERT INTO orders VALUES (13, 99, 'w')", [])
            .is_err(),
        "a row naming a missing parent was accepted"
    );
}

#[test]
fn an_added_foreign_key_over_rows_naming_a_missing_parent_is_refused_with_their_count() {
    for prior in [true, false] {
        let mut ex = seeded(SHOP);
        ex.connection()
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO orders VALUES (13, 99, 'w'), (14, 98, 'v');",
            )
            .expect("orphans");
        ex.connection()
            .pragma_update(None, "foreign_keys", prior)
            .expect("set fk");
        let before = dump(ex.connection());
        let (_dir, migrations) = migration(&add_foreign_key("orders", "customer_id", "customers"));

        let err = up(&mut ex, &migrations, None).expect_err("refused");

        assert!(
            matches!(
                err,
                MigrateError::ForeignKeyViolation { ref table, rows: 2 } if table == "orders"
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("orders") && err.to_string().contains('2'));
        assert_eq!(dump(ex.connection()), before);
    }
}

#[test]
fn an_added_foreign_key_to_its_own_table_adds_the_self_reference_and_keeps_every_row() {
    let mut ex = seeded(
        "CREATE TABLE employees (id INTEGER PRIMARY KEY, reports_to INTEGER);
         INSERT INTO employees VALUES (1, NULL), (2, 1), (3, 2);",
    );
    let (_dir, migrations) = migration(&add_foreign_key("employees", "reports_to", "employees"));

    up(&mut ex, &migrations, None).expect("up");

    let conn = ex.connection();
    let sql = table_sql(conn, "employees");
    assert!(
        sql.ends_with("REFERENCES \"employees\" (\"id\") ON DELETE CASCADE)")
            && !sql.contains("_migratr"),
        "{sql}"
    );
    assert_eq!(
        references(conn, "employees"),
        [("employees".into(), "id".into())]
    );
    assert_eq!(query_i64(conn, "SELECT count(*) FROM employees"), 3);
    assert!(
        conn.execute("INSERT INTO employees VALUES (4, 99)", [])
            .is_err()
    );
}

fn drop_foreign_key(table: &str, column: &str, parent: &str) -> String {
    format!(
        r#"{{"op": "drop_foreign_key", "definition": {{"table": "{table}", "columns": ["{column}"],
            "references": {{"table": "{parent}", "columns": ["id"]}}, "on_delete": "CASCADE"}}}}"#
    )
}

#[test]
fn a_dropped_foreign_key_goes_from_either_spelling_keeps_every_row_and_its_down_adds_it_back() {
    for schema in [
        "CREATE TABLE authors (id INTEGER PRIMARY KEY);
         CREATE TABLE books (id INTEGER PRIMARY KEY,
           author_id INTEGER NOT NULL CONSTRAINT by REFERENCES authors (id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
           title TEXT);",
        "CREATE TABLE authors (id INTEGER PRIMARY KEY);
         CREATE TABLE books (id INTEGER PRIMARY KEY, author_id INTEGER NOT NULL, title TEXT,
           FOREIGN KEY (author_id) REFERENCES authors (id) ON DELETE CASCADE);",
    ] {
        let mut ex = seeded(&format!(
            "{schema} INSERT INTO authors VALUES (1); INSERT INTO books VALUES (1, 1, 'x'), (2, 1, 'y');"
        ));
        let (_dir, migrations) = migration(&drop_foreign_key("books", "author_id", "authors"));

        up(&mut ex, &migrations, None).expect("up");

        let conn = ex.connection();
        assert!(references(conn, "books").is_empty(), "{schema}");
        let sql = table_sql(conn, "books");
        assert!(sql.contains("author_id INTEGER NOT NULL") && !sql.contains("REFERENCES"), "{sql}");
        assert_eq!(query_i64(conn, "SELECT count(*) FROM books"), 2);

        migratr::down(&mut ex, &migrations, 1).expect("down");

        let conn = ex.connection();
        assert_eq!(references(conn, "books"), [("authors".into(), "id".into())]);
        assert_eq!(query_i64(conn, "SELECT count(*) FROM books"), 2);
        assert!(conn.execute("INSERT INTO books VALUES (3, 99, 'z')", []).is_err());
    }
}

#[test]
fn dropping_a_foreign_key_the_table_does_not_have_is_refused_and_nothing_changes() {
    let mut ex = seeded(SHOP);
    let before = dump(ex.connection());
    let (_dir, migrations) = migration(&drop_foreign_key("orders", "customer_id", "customers"));

    let err = up(&mut ex, &migrations, None).expect_err("refused");

    assert!(
        matches!(err, MigrateError::NoSuchForeignKey { ref table, .. } if table == "orders"),
        "{err:?}"
    );
    assert_eq!(dump(ex.connection()), before);
}

#[test]
fn a_table_made_with_checks_unique_and_a_trigger_keeps_all_three_after_a_rebuild() {
    let dir = TempDir::new().expect("tempdir");
    let file = |name: &str, ops: &str| {
        fs::write(dir.path().join(name), format!(r#"{{"up": [{ops}]}}"#)).expect("write");
    };
    file(
        "20260101000001_tables.json",
        r#"{"op": "create_table", "table": "teams", "columns": [
             {"name": "id", "type": "INTEGER", "primary_key": 1}]},
           {"op": "create_table", "table": "seats", "columns": [
             {"name": "id", "type": "INTEGER", "primary_key": 1},
             {"name": "team", "type": "INTEGER"},
             {"name": "num", "type": "INTEGER"}],
            "checks": ["num > 0"], "unique": [["team", "num"]]},
           {"op": "create_table", "table": "log", "columns": [{"name": "seat", "type": "INTEGER"}]},
           {"op": "create_trigger", "name": "seats_log", "table": "seats",
            "sql": "CREATE TRIGGER seats_log AFTER INSERT ON seats BEGIN INSERT INTO log VALUES (new.id); END"}"#,
    );
    let mut ex = seeded("");
    up(&mut ex, &load_dir(dir.path()).expect("load"), None).expect("make the table");
    ex.connection()
        .execute_batch("INSERT INTO teams VALUES (1); INSERT INTO seats VALUES (1, 1, 1);")
        .expect("rows");

    file(
        "20260101000002_key.json",
        &add_foreign_key("seats", "team", "teams"),
    );
    let migrations = load_dir(dir.path()).expect("load");
    let schema = ex.read_schema().expect("schema");
    assert_eq!(
        rebuild_tables(&schema, &migrations[1]).expect("tables"),
        ["seats"]
    );
    up(&mut ex, &migrations, None).expect("rebuild");

    let conn = ex.connection();
    let sql = table_sql(conn, "seats");
    assert!(
        sql.contains("CHECK (num > 0)") && sql.contains("UNIQUE (\"team\", \"num\")"),
        "{sql}"
    );
    assert!(
        conn.execute("INSERT INTO seats VALUES (2, 1, 0)", [])
            .is_err(),
        "a row breaking the check was accepted"
    );
    assert!(
        conn.execute("INSERT INTO seats VALUES (3, 1, 1)", [])
            .is_err(),
        "a row repeating the unique pair was accepted"
    );
    let logged = query_i64(conn, "SELECT count(*) FROM log");
    conn.execute("INSERT INTO seats VALUES (4, 1, 2)", [])
        .expect("a valid row");
    assert_eq!(query_i64(conn, "SELECT count(*) FROM log"), logged + 1);
}
