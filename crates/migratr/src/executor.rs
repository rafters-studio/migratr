use std::path::PathBuf;

use thiserror::Error;

use crate::migration::ForeignKeyDef;

/// One row of `sqlite_master`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaObject {
    pub kind: String,
    pub name: String,
    pub tbl_name: String,
    /// `None` for objects SQLite creates implicitly, such as autoindexes.
    pub sql: Option<String>,
}

/// A failed `run_atomic`: the executor's error and the statement it failed on.
#[derive(Debug, Error)]
#[error("{source}")]
pub struct AtomicError<E: std::error::Error + Send + Sync + 'static> {
    /// Position in the statement list of the statement that failed. `None` when the failure was
    /// not in a statement, such as beginning or committing the transaction.
    pub index: Option<usize>,
    pub source: E,
}

impl<E: std::error::Error + Send + Sync + 'static> AtomicError<E> {
    /// A failure outside any statement.
    pub fn outside_statements(source: E) -> Self {
        Self {
            index: None,
            source,
        }
    }
}

/// One applied migration as recorded in the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    pub version: u64,
    pub name: String,
    pub checksum: String,
}

/// One row of `PRAGMA table_xinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub cid: i64,
    pub name: String,
    pub decl_type: String,
    pub not_null: bool,
    pub default_value: Option<String>,
    /// 1-based position within the primary key, or 0 when not part of it.
    pub pk: i64,
    /// 0 normal, 1 hidden, 2 generated virtual, 3 generated stored.
    pub hidden: i64,
}

/// One row of `PRAGMA foreign_key_list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    pub id: i64,
    pub seq: i64,
    pub table: String,
    pub from: String,
    pub to: Option<String>,
    pub on_update: String,
    pub on_delete: String,
    pub match_clause: String,
}

/// A table's pragma rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub name: String,
    pub columns: Vec<ColumnInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
}

/// Everything migratr reads from a database's schema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaSnapshot {
    pub objects: Vec<SchemaObject>,
    pub tables: Vec<TableInfo>,
}

/// The boundary between migratr and a SQLite connection.
pub trait Executor {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Every row of sqlite_master plus the pragma rows migratr reads.
    fn read_schema(&mut self) -> Result<SchemaSnapshot, Self::Error>;

    /// Every row of the `_migratr_migrations` ledger, ordered by version.
    /// Empty when the ledger table does not exist.
    fn read_ledger(&mut self) -> Result<Vec<LedgerRow>, Self::Error>;

    /// Each of `tables` that has rows a foreign key refuses, by name, with the number of those
    /// rows, as [`foreign_key_violations_through`](crate::foreign_key_violations_through)
    /// reads them. `None`, the default, when the executor does not count them; a rebuild's
    /// own copy then refuses such rows inside its atomic run.
    fn foreign_key_violations(
        &mut self,
        tables: &[&str],
    ) -> Result<Option<Vec<(String, u64)>>, Self::Error> {
        let _ = tables;
        Ok(None)
    }

    /// The number of rows of `key`'s table whose key columns are all non-NULL and name no row
    /// of its parent, as [`missing_parents_through`](crate::missing_parents_through) counts
    /// them. `None`, the default, when the executor does not count them; the rebuild that adds
    /// the key then refuses such rows inside its atomic run.
    fn missing_parents(&mut self, key: &ForeignKeyDef) -> Result<Option<u64>, Self::Error> {
        let _ = key;
        Ok(None)
    }

    /// Run all statements in one transaction; all apply or none do. A failure reports the index
    /// of the statement that failed.
    /// With `suspend_foreign_keys`, foreign_keys is turned off before the
    /// transaction and restored to its prior value after, on success or failure.
    fn run_atomic(
        &mut self,
        statements: &[String],
        suspend_foreign_keys: bool,
    ) -> Result<(), AtomicError<Self::Error>>;

    /// Write a consistent copy of the whole database named `file_name`, to a location the
    /// executor chooses beside the database, and return its path. Returns `Ok(None)` when the
    /// database is not durable and needs no snapshot. An executor that cannot snapshot a
    /// durable database returns an error.
    fn snapshot(&mut self, file_name: &str) -> Result<Option<PathBuf>, Self::Error>;
}

#[cfg(feature = "rusqlite")]
pub use rusqlite_executor::RusqliteExecutor;

#[cfg(feature = "rusqlite")]
mod rusqlite_executor {
    use std::path::{Path, PathBuf};

    use rusqlite::Connection;
    use rusqlite::types::ValueRef;
    use serde_json::Value;

    use super::{AtomicError, Executor, LedgerRow, SchemaSnapshot};
    use crate::migration::ForeignKeyDef;
    use crate::reader::{
        SqlReader, foreign_key_violations_through, missing_parents_through, read_ledger_through,
        read_schema_through,
    };

    /// An [`Executor`] over a rusqlite connection.
    pub struct RusqliteExecutor {
        conn: Connection,
    }

    impl RusqliteExecutor {
        pub fn new(conn: Connection) -> Self {
            Self { conn }
        }

        pub fn connection(&self) -> &Connection {
            &self.conn
        }
    }

    fn foreign_keys_enabled(conn: &Connection) -> rusqlite::Result<bool> {
        conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))
    }

    /// A connection as a [`SqlReader`].
    struct Reader<'a>(&'a Connection);

    impl SqlReader for Reader<'_> {
        type Error = rusqlite::Error;

        fn rows(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, Self::Error> {
            let mut statement = self.0.prepare(sql)?;
            let columns = statement.column_count();
            statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|at| {
                            Ok(match row.get_ref(at)? {
                                ValueRef::Integer(n) => Value::from(n),
                                ValueRef::Real(n) => Value::from(n),
                                ValueRef::Text(text) => {
                                    Value::from(String::from_utf8_lossy(text).into_owned())
                                }
                                ValueRef::Null | ValueRef::Blob(_) => Value::Null,
                            })
                        })
                        .collect()
                })?
                .collect()
        }

        fn malformed(&self, what: &str) -> Self::Error {
            rusqlite::Error::InvalidColumnName(what.to_owned())
        }
    }

    impl Executor for RusqliteExecutor {
        type Error = rusqlite::Error;

        fn read_schema(&mut self) -> Result<SchemaSnapshot, Self::Error> {
            read_schema_through(&mut Reader(&self.conn), |_| false)
        }

        fn read_ledger(&mut self) -> Result<Vec<LedgerRow>, Self::Error> {
            read_ledger_through(&mut Reader(&self.conn))
        }

        fn foreign_key_violations(
            &mut self,
            tables: &[&str],
        ) -> Result<Option<Vec<(String, u64)>>, Self::Error> {
            foreign_key_violations_through(&mut Reader(&self.conn), tables).map(Some)
        }

        fn missing_parents(&mut self, key: &ForeignKeyDef) -> Result<Option<u64>, Self::Error> {
            missing_parents_through(&mut Reader(&self.conn), key).map(Some)
        }

        fn run_atomic(
            &mut self,
            statements: &[String],
            suspend_foreign_keys: bool,
        ) -> Result<(), AtomicError<Self::Error>> {
            let prior = if suspend_foreign_keys {
                let prior =
                    foreign_keys_enabled(&self.conn).map_err(AtomicError::outside_statements)?;
                self.conn
                    .pragma_update(None, "foreign_keys", false)
                    .map_err(AtomicError::outside_statements)?;
                Some(prior)
            } else {
                None
            };

            let applied = (|| {
                let tx = self
                    .conn
                    .transaction()
                    .map_err(AtomicError::outside_statements)?;
                for (index, statement) in statements.iter().enumerate() {
                    tx.execute_batch(statement).map_err(|source| AtomicError {
                        index: Some(index),
                        source,
                    })?;
                }
                tx.commit().map_err(AtomicError::outside_statements)
            })();

            if let Some(prior) = prior {
                let restored = self
                    .conn
                    .pragma_update(None, "foreign_keys", prior)
                    .map_err(AtomicError::outside_statements);
                applied?;
                return restored;
            }
            applied
        }

        fn snapshot(&mut self, file_name: &str) -> Result<Option<PathBuf>, Self::Error> {
            let db_file = match self.conn.path() {
                Some(path) if !path.is_empty() => Path::new(path),
                _ => return Ok(None),
            };
            let dir = db_file
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .join(".migratr")
                .join("snapshots");
            std::fs::create_dir_all(&dir)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let target = dir.join(file_name);
            let target_str = target
                .to_str()
                .ok_or_else(|| rusqlite::Error::InvalidPath(target.clone()))?;
            self.conn.execute("VACUUM INTO ?1", [target_str])?;
            Ok(Some(target))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn executor() -> RusqliteExecutor {
            RusqliteExecutor::new(Connection::open_in_memory().expect("open in-memory db"))
        }

        fn s(sql: &str) -> String {
            sql.to_string()
        }

        #[test]
        fn read_schema_returns_master_rows_and_pragma_rows() {
            let mut ex = executor();
            ex.run_atomic(
                &[
                    s("CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT 'x')"),
                    s("CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE)"),
                    s("CREATE INDEX child_parent ON child(parent_id)"),
                ],
                false,
            )
            .expect("create schema");

            let snap = ex.read_schema().expect("read schema");
            let kinds: Vec<_> = snap
                .objects
                .iter()
                .map(|o| (o.kind.as_str(), o.name.as_str()))
                .collect();
            assert!(kinds.contains(&("table", "parent")));
            assert!(kinds.contains(&("table", "child")));
            assert!(kinds.contains(&("index", "child_parent")));

            let parent = snap
                .tables
                .iter()
                .find(|t| t.name == "parent")
                .expect("parent");
            assert_eq!(parent.columns.len(), 2);
            assert_eq!(parent.columns[0].pk, 1);
            assert!(parent.columns[1].not_null);
            assert_eq!(parent.columns[1].default_value.as_deref(), Some("'x'"));
            assert!(parent.foreign_keys.is_empty());

            let child = snap
                .tables
                .iter()
                .find(|t| t.name == "child")
                .expect("child");
            assert_eq!(child.foreign_keys.len(), 1);
            assert_eq!(child.foreign_keys[0].table, "parent");
            assert_eq!(child.foreign_keys[0].from, "parent_id");
            assert_eq!(child.foreign_keys[0].on_delete, "CASCADE");
        }

        #[test]
        fn read_schema_handles_quoted_table_names() {
            let mut ex = executor();
            ex.run_atomic(&[s("CREATE TABLE \"we\"\"ird\" (a)")], false)
                .expect("create");
            let snap = ex.read_schema().expect("read");
            assert_eq!(snap.tables[0].name, "we\"ird");
            assert_eq!(snap.tables[0].columns.len(), 1);
        }

        #[test]
        fn read_ledger_is_empty_without_the_table_and_lists_rows_with_it() {
            let mut ex = executor();
            assert!(ex.read_ledger().expect("no table").is_empty());
            ex.run_atomic(
                &[
                    s("CREATE TABLE _migratr_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL)"),
                    s("INSERT INTO _migratr_migrations VALUES (2, 'b', 'cb', 't')"),
                    s("INSERT INTO _migratr_migrations VALUES (1, 'a', 'ca', 't')"),
                ],
                false,
            )
            .expect("seed");
            let rows = ex.read_ledger().expect("rows");
            assert_eq!(
                rows,
                vec![
                    LedgerRow {
                        version: 1,
                        name: "a".into(),
                        checksum: "ca".into()
                    },
                    LedgerRow {
                        version: 2,
                        name: "b".into(),
                        checksum: "cb".into()
                    },
                ]
            );
        }

        #[test]
        fn run_atomic_applies_none_when_one_fails() {
            let mut ex = executor();
            let result = ex.run_atomic(&[s("CREATE TABLE a (x)"), s("THIS IS NOT SQL")], false);
            assert!(result.is_err());
            assert!(ex.read_schema().expect("read").objects.is_empty());
        }

        #[test]
        fn run_atomic_reports_the_index_of_the_failing_statement() {
            let mut ex = executor();
            let err = ex
                .run_atomic(
                    &[
                        s("CREATE TABLE a (x)"),
                        s("CREATE TABLE b (y)"),
                        s("NOT SQL"),
                    ],
                    false,
                )
                .expect_err("third statement fails");
            assert_eq!(err.index, Some(2));
        }

        #[test]
        fn run_atomic_applies_all_on_success() {
            let mut ex = executor();
            ex.run_atomic(&[s("CREATE TABLE a (x)"), s("CREATE TABLE b (y)")], false)
                .expect("apply");
            assert_eq!(ex.read_schema().expect("read").tables.len(), 2);
        }

        #[test]
        fn suspend_foreign_keys_restores_prior_value_on_success() {
            let mut ex = executor();
            ex.connection()
                .pragma_update(None, "foreign_keys", true)
                .expect("enable fk");
            ex.run_atomic(&[s("CREATE TABLE a (x)")], true)
                .expect("apply");
            assert!(foreign_keys_enabled(ex.connection()).expect("pragma"));
        }

        #[test]
        fn suspend_foreign_keys_restores_prior_value_on_failure() {
            let mut ex = executor();
            ex.connection()
                .pragma_update(None, "foreign_keys", true)
                .expect("enable fk");
            assert!(ex.run_atomic(&[s("NOT SQL")], true).is_err());
            assert!(foreign_keys_enabled(ex.connection()).expect("pragma"));
        }

        #[test]
        fn suspend_foreign_keys_leaves_disabled_stays_disabled() {
            let mut ex = executor();
            ex.connection()
                .pragma_update(None, "foreign_keys", false)
                .expect("disable fk");
            ex.run_atomic(&[s("CREATE TABLE a (x)")], true)
                .expect("apply");
            assert!(!foreign_keys_enabled(ex.connection()).expect("pragma"));
        }

        #[test]
        fn foreign_keys_are_off_inside_suspended_transaction() {
            let mut ex = executor();
            ex.connection()
                .pragma_update(None, "foreign_keys", true)
                .expect("enable fk");
            ex.run_atomic(
                &[
                    s("CREATE TABLE p (id INTEGER PRIMARY KEY)"),
                    s("CREATE TABLE c (pid INTEGER REFERENCES p(id))"),
                    s("INSERT INTO c VALUES (99)"),
                ],
                true,
            )
            .expect("orphan row allowed while suspended");
        }

        #[test]
        fn snapshot_writes_a_consistent_copy_beside_a_file_database() {
            let dir = tempfile::TempDir::new().expect("tmp");
            let db = dir.path().join("app.db");
            let mut ex = RusqliteExecutor::new(Connection::open(&db).expect("open"));
            ex.run_atomic(
                &[s("CREATE TABLE a (x)"), s("INSERT INTO a VALUES (7)")],
                false,
            )
            .expect("seed");

            let path = ex
                .snapshot("copy.db")
                .expect("snapshot")
                .expect("a file database is durable");

            // SQLite reports the database's resolved path, so compare resolved paths.
            assert_eq!(
                path.canonicalize().expect("snapshot exists"),
                dir.path()
                    .join(".migratr/snapshots/copy.db")
                    .canonicalize()
                    .expect("expected path exists")
            );
            let copy = Connection::open(&path).expect("open copy");
            let x: i64 = copy
                .query_row("SELECT x FROM a", [], |r| r.get(0))
                .expect("read copy");
            assert_eq!(x, 7);
        }

        #[test]
        fn snapshot_of_an_in_memory_database_reports_none_needed() {
            assert_eq!(executor().snapshot("copy.db").expect("snapshot"), None);
        }
    }
}
