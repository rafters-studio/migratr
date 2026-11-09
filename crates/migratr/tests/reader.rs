//! `read_schema_through` over JSON rows reads what `RusqliteExecutor` reads.
#![cfg(feature = "rusqlite")]

use migratr::{
    Executor, RusqliteExecutor, SqlReader, read_ledger_through, read_schema_through, record_applied,
};
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde_json::{Value, json};

/// Rows as JSON, as a service speaking HTTP would answer them.
struct Json(Connection);

impl SqlReader for Json {
    type Error = String;

    fn rows(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, String> {
        let mut statement = self.0.prepare(sql).map_err(|e| e.to_string())?;
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|at| {
                        Ok(match row.get_ref(at)? {
                            ValueRef::Integer(n) => json!(n),
                            ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                            _ => Value::Null,
                        })
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(|e| e.to_string())
    }

    fn malformed(&self, what: &str) -> String {
        what.to_owned()
    }
}

const SCHEMA: &str = "CREATE TABLE parent (id TEXT PRIMARY KEY, code TEXT UNIQUE DEFAULT 'x');
    CREATE TABLE child (
        id INTEGER PRIMARY KEY,
        parent_id TEXT NOT NULL REFERENCES parent(id) ON DELETE CASCADE,
        up TEXT GENERATED ALWAYS AS (upper(parent_id)) VIRTUAL
    );
    CREATE INDEX idx_child ON child (parent_id);
    CREATE VIEW v AS SELECT id FROM parent;";

fn migration() -> Result<migratr::Migration, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("20260101000000_a.json"),
        r#"{"up": [{"op": "raw_sql", "up": "SELECT 1"}]}"#,
    )?;
    Ok(migratr::load_dir(dir.path())?.remove(0))
}

#[test]
fn the_reader_over_json_reads_what_the_rusqlite_executor_reads()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("a.db");
    let mut executor = RusqliteExecutor::new(Connection::open(&path)?);
    executor.connection().execute_batch(SCHEMA)?;
    let mut reader = Json(Connection::open(&path)?);

    assert!(read_ledger_through(&mut reader)?.is_empty());
    record_applied(&mut executor, &migration()?)?;

    assert_eq!(
        read_schema_through(&mut reader, |_| false)?,
        executor.read_schema()?
    );
    let ledger = read_ledger_through(&mut reader)?;
    assert_eq!(ledger, executor.read_ledger()?);
    assert_eq!(ledger.len(), 1);
    Ok(())
}

#[test]
fn a_skipped_table_and_what_is_on_it_are_not_read() -> Result<(), Box<dyn std::error::Error>> {
    let connection = Connection::open_in_memory()?;
    connection.execute_batch(SCHEMA)?;
    let schema = read_schema_through(&mut Json(connection), |table| table == "child")?;
    assert!(schema.objects.iter().all(|o| o.tbl_name != "child"));
    assert!(schema.tables.iter().all(|t| t.name != "child"));
    assert!(schema.tables.iter().any(|t| t.name == "parent"));
    Ok(())
}

#[test]
fn a_negative_ledger_version_and_a_wrong_value_are_errors() -> Result<(), Box<dyn std::error::Error>>
{
    let connection = Connection::open_in_memory()?;
    connection.execute_batch(
        "CREATE TABLE _migratr_migrations (version INTEGER PRIMARY KEY, name TEXT, checksum TEXT);
         INSERT INTO _migratr_migrations VALUES (-1, 'a', 'b');",
    )?;
    let mut reader = Json(connection);
    assert_eq!(
        read_ledger_through(&mut reader),
        Err("a negative ledger version".to_owned())
    );
    reader.0.execute_batch(
        "DELETE FROM _migratr_migrations; INSERT INTO _migratr_migrations VALUES (2, NULL, 'b');",
    )?;
    assert_eq!(
        read_ledger_through(&mut reader),
        Err("a value that is not text".to_owned())
    );
    Ok(())
}
