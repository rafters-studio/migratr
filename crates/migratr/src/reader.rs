//! The one reader of a database's schema and ledger, for anything that can run a statement and
//! answer with rows.

use serde_json::Value;

use crate::executor::{
    ColumnInfo, ForeignKeyInfo, LedgerRow, SchemaObject, SchemaSnapshot, TableInfo,
};

const OBJECTS: &str = "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY rowid";
const LEDGER_EXISTS: &str =
    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_migratr_migrations'";
const LEDGER: &str = "SELECT version, name, checksum FROM _migratr_migrations ORDER BY version";

/// What can run a statement and answer with its rows, each value as JSON.
pub trait SqlReader {
    type Error;

    /// The rows of `sql`.
    fn rows(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, Self::Error>;

    /// The error for a row that is not what its statement promises.
    fn malformed(&self, what: &str) -> Self::Error;
}

fn cell<'a, R: SqlReader>(reader: &R, row: &'a [Value], at: usize) -> Result<&'a Value, R::Error> {
    row.get(at)
        .ok_or_else(|| reader.malformed("a row with too few columns"))
}

fn text<R: SqlReader>(reader: &R, row: &[Value], at: usize) -> Result<String, R::Error> {
    cell(reader, row, at)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| reader.malformed("a value that is not text"))
}

fn maybe_text<R: SqlReader>(
    reader: &R,
    row: &[Value],
    at: usize,
) -> Result<Option<String>, R::Error> {
    match cell(reader, row, at)? {
        Value::Null => Ok(None),
        _ => text(reader, row, at).map(Some),
    }
}

fn number<R: SqlReader>(reader: &R, row: &[Value], at: usize) -> Result<i64, R::Error> {
    cell(reader, row, at)?
        .as_i64()
        .ok_or_else(|| reader.malformed("a value that is not an integer"))
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Everything migratr reads from a schema. `skip` names tables the reader must not touch, such as
/// a host's own: neither they nor anything on them are read.
pub fn read_schema_through<R: SqlReader>(
    reader: &mut R,
    skip: impl Fn(&str) -> bool,
) -> Result<SchemaSnapshot, R::Error> {
    let mut objects = Vec::new();
    for row in reader.rows(OBJECTS)? {
        let object = SchemaObject {
            kind: text(reader, &row, 0)?,
            name: text(reader, &row, 1)?,
            tbl_name: text(reader, &row, 2)?,
            sql: maybe_text(reader, &row, 3)?,
        };
        if !skip(&object.tbl_name) {
            objects.push(object);
        }
    }
    let mut tables = Vec::new();
    for object in objects.iter().filter(|o| o.kind == "table") {
        let table = quoted(&object.name);
        let mut columns = Vec::new();
        for row in reader.rows(&format!("PRAGMA table_xinfo({table})"))? {
            columns.push(ColumnInfo {
                cid: number(reader, &row, 0)?,
                name: text(reader, &row, 1)?,
                decl_type: text(reader, &row, 2)?,
                not_null: number(reader, &row, 3)? != 0,
                default_value: maybe_text(reader, &row, 4)?,
                pk: number(reader, &row, 5)?,
                hidden: number(reader, &row, 6)?,
            });
        }
        let mut foreign_keys = Vec::new();
        for row in reader.rows(&format!("PRAGMA foreign_key_list({table})"))? {
            foreign_keys.push(ForeignKeyInfo {
                id: number(reader, &row, 0)?,
                seq: number(reader, &row, 1)?,
                table: text(reader, &row, 2)?,
                from: text(reader, &row, 3)?,
                to: maybe_text(reader, &row, 4)?,
                on_update: text(reader, &row, 5)?,
                on_delete: text(reader, &row, 6)?,
                match_clause: text(reader, &row, 7)?,
            });
        }
        tables.push(TableInfo {
            name: object.name.clone(),
            columns,
            foreign_keys,
        });
    }
    Ok(SchemaSnapshot { objects, tables })
}

/// Every row of the ledger, ordered by version. Empty when the ledger table does not exist.
pub fn read_ledger_through<R: SqlReader>(reader: &mut R) -> Result<Vec<LedgerRow>, R::Error> {
    if reader.rows(LEDGER_EXISTS)?.is_empty() {
        return Ok(Vec::new());
    }
    let mut ledger = Vec::new();
    for row in reader.rows(LEDGER)? {
        ledger.push(LedgerRow {
            version: u64::try_from(number(reader, &row, 0)?)
                .map_err(|_| reader.malformed("a negative ledger version"))?,
            name: text(reader, &row, 1)?,
            checksum: text(reader, &row, 2)?,
        });
    }
    Ok(ledger)
}
