//! Helpers shared by the integration tests.

use std::path::Path;

use migratr::{AtomicError, Executor, LedgerRow, RusqliteExecutor, SchemaSnapshot};
use rusqlite::Connection;

/// Runs the statements through a real executor with statement `fail_at` replaced by invalid
/// SQL, so the real transaction fails at that position and rolls back.
pub struct FailAt {
    pub inner: RusqliteExecutor,
    pub fail_at: usize,
    /// How many statements the last `run_atomic` call received.
    pub statement_count: usize,
}

impl Executor for FailAt {
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
        self.statement_count = statements.len();
        let mut broken = statements.to_vec();
        if let Some(statement) = broken.get_mut(self.fail_at) {
            *statement = "FAULT INJECTED HERE".to_string();
        }
        self.inner.run_atomic(&broken, suspend_foreign_keys)
    }

    fn snapshot(&mut self, path: &Path) -> Result<bool, Self::Error> {
        self.inner.snapshot(path)
    }
}

/// Every sqlite_master row and every table's rows, as text.
pub fn dump(conn: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    let rows: Vec<(String, String, String, Option<String>)> = conn
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name")
        .expect("prepare")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    for (kind, name, tbl_name, sql) in rows {
        out.push(format!("{kind}|{name}|{tbl_name}|{sql:?}"));
        if kind == "table" {
            let mut all = conn
                .prepare(&format!("SELECT rowid, * FROM \"{name}\" ORDER BY 1"))
                .or_else(|_| conn.prepare(&format!("SELECT * FROM \"{name}\" ORDER BY 1")))
                .expect("select");
            let width = all.column_count();
            let cells: Vec<String> = all
                .query_map([], |r| {
                    (0..width)
                        .map(|i| {
                            r.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<Result<Vec<_>, _>>()
                        .map(|c| c.join(","))
                })
                .expect("cells")
                .collect::<Result<_, _>>()
                .expect("cells");
            out.extend(cells);
        }
    }
    out
}
