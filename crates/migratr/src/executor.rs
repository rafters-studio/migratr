use std::path::Path;

/// One row of `sqlite_master`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaObject {
    pub kind: String,
    pub name: String,
    pub tbl_name: String,
    /// `None` for objects SQLite creates implicitly, such as autoindexes.
    pub sql: Option<String>,
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

    /// Run all statements in one transaction; all apply or none do.
    /// With `suspend_foreign_keys`, foreign_keys is turned off before the
    /// transaction and restored to its prior value after, on success or failure.
    fn run_atomic(
        &mut self,
        statements: &[String],
        suspend_foreign_keys: bool,
    ) -> Result<(), Self::Error>;

    /// Write a consistent copy of the whole database to `path`.
    /// Returns Ok(false) when this executor cannot write snapshots.
    fn snapshot(&mut self, path: &Path) -> Result<bool, Self::Error>;
}

#[cfg(feature = "rusqlite")]
pub use rusqlite_executor::RusqliteExecutor;

#[cfg(feature = "rusqlite")]
mod rusqlite_executor {
    use std::path::Path;

    use rusqlite::Connection;

    use super::{
        ColumnInfo, Executor, ForeignKeyInfo, LedgerRow, SchemaObject, SchemaSnapshot, TableInfo,
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

    fn quote_ident(name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    impl Executor for RusqliteExecutor {
        type Error = rusqlite::Error;

        fn read_schema(&mut self) -> Result<SchemaSnapshot, Self::Error> {
            let objects = self
                .conn
                .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY rowid")?
                .query_map([], |row| {
                    Ok(SchemaObject {
                        kind: row.get(0)?,
                        name: row.get(1)?,
                        tbl_name: row.get(2)?,
                        sql: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut tables = Vec::new();
            for object in objects.iter().filter(|o| o.kind == "table") {
                let quoted = quote_ident(&object.name);
                let columns = self
                    .conn
                    .prepare(&format!("PRAGMA table_xinfo({quoted})"))?
                    .query_map([], |row| {
                        Ok(ColumnInfo {
                            cid: row.get(0)?,
                            name: row.get(1)?,
                            decl_type: row.get(2)?,
                            not_null: row.get(3)?,
                            default_value: row.get(4)?,
                            pk: row.get(5)?,
                            hidden: row.get(6)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let foreign_keys = self
                    .conn
                    .prepare(&format!("PRAGMA foreign_key_list({quoted})"))?
                    .query_map([], |row| {
                        Ok(ForeignKeyInfo {
                            id: row.get(0)?,
                            seq: row.get(1)?,
                            table: row.get(2)?,
                            from: row.get(3)?,
                            to: row.get(4)?,
                            on_update: row.get(5)?,
                            on_delete: row.get(6)?,
                            match_clause: row.get(7)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                tables.push(TableInfo {
                    name: object.name.clone(),
                    columns,
                    foreign_keys,
                });
            }

            Ok(SchemaSnapshot { objects, tables })
        }

        fn read_ledger(&mut self) -> Result<Vec<LedgerRow>, Self::Error> {
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_migratr_migrations')",
                [],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(Vec::new());
            }
            self.conn
                .prepare(
                    "SELECT version, name, checksum FROM _migratr_migrations ORDER BY version",
                )?
                .query_map([], |row| {
                    let version: i64 = row.get(0)?;
                    Ok(LedgerRow {
                        version: u64::try_from(version)
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, version))?,
                        name: row.get(1)?,
                        checksum: row.get(2)?,
                    })
                })?
                .collect()
        }

        fn run_atomic(
            &mut self,
            statements: &[String],
            suspend_foreign_keys: bool,
        ) -> Result<(), Self::Error> {
            let prior = if suspend_foreign_keys {
                let prior = foreign_keys_enabled(&self.conn)?;
                self.conn.pragma_update(None, "foreign_keys", false)?;
                Some(prior)
            } else {
                None
            };

            let applied = (|| {
                let tx = self.conn.transaction()?;
                for statement in statements {
                    tx.execute_batch(statement)?;
                }
                tx.commit()
            })();

            if let Some(prior) = prior {
                let restored = self.conn.pragma_update(None, "foreign_keys", prior);
                applied?;
                return restored;
            }
            applied
        }

        fn snapshot(&mut self, path: &Path) -> Result<bool, Self::Error> {
            let target = path
                .to_str()
                .ok_or_else(|| rusqlite::Error::InvalidPath(path.to_path_buf()))?;
            self.conn.execute("VACUUM INTO ?1", [target])?;
            Ok(true)
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
        fn snapshot_writes_a_consistent_copy() {
            let dir = std::env::temp_dir().join(format!("migratr-snap-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let path = dir.join("copy.db");
            let _ = std::fs::remove_file(&path);

            let mut ex = executor();
            ex.run_atomic(
                &[s("CREATE TABLE a (x)"), s("INSERT INTO a VALUES (7)")],
                false,
            )
            .expect("seed");
            assert!(ex.snapshot(&path).expect("snapshot"));

            let copy = Connection::open(&path).expect("open copy");
            let x: i64 = copy
                .query_row("SELECT x FROM a", [], |r| r.get(0))
                .expect("read copy");
            assert_eq!(x, 7);
            drop(copy);
            std::fs::remove_dir_all(&dir).expect("cleanup");
        }
    }
}
