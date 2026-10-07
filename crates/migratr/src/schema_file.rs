//! `schema.json`: the head schema of a migrations directory, read from `sqlite_master`.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::executor::Executor;
use crate::migration::MigrateError;

const FILE_NAME: &str = "schema.json";
const LEDGER_TABLE: &str = "_migratr_migrations";

/// The contents of `schema.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaFile {
    /// The newest applied migration's version; `None` when none is applied.
    pub version: Option<u64>,
    pub objects: Vec<SchemaFileObject>,
}

/// One table, index, trigger or view, with the SQL SQLite stored for it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SchemaFileObject {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub sql: String,
}

/// Writes `schema.json` into `dir`: every table, index, trigger and view of the database
/// except the ledger and SQLite's own objects, sorted by type then name, and the ledger's
/// latest version. The same schema and version always produce the same bytes.
pub fn write_schema(exec: &mut impl Executor, dir: &Path) -> Result<(), MigrateError> {
    let snapshot = exec
        .read_schema()
        .map_err(|e| MigrateError::Executor(Box::new(e)))?;
    let ledger = exec
        .read_ledger()
        .map_err(|e| MigrateError::Executor(Box::new(e)))?;

    let mut objects: Vec<SchemaFileObject> = snapshot
        .objects
        .into_iter()
        .filter(|o| !is_internal(&o.name) && !is_internal(&o.tbl_name))
        .filter_map(|o| {
            Some(SchemaFileObject {
                kind: o.kind,
                name: o.name,
                sql: o.sql?,
            })
        })
        .collect();
    objects.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));

    let file = SchemaFile {
        version: ledger.iter().map(|r| r.version).max(),
        objects,
    };
    let mut text = serde_json::to_string_pretty(&file).map_err(|e| MigrateError::Parse {
        file: FILE_NAME.to_string(),
        path: ".".to_string(),
        message: e.to_string(),
    })?;
    text.push('\n');

    // Written beside the target and renamed over it, so a failed write never leaves a
    // partial schema.json.
    let target = dir.join(FILE_NAME);
    let staging = dir.join("schema.json.tmp");
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| MigrateError::Io { path, source }
    };
    fs::write(&staging, text).map_err(io(&staging))?;
    fs::rename(&staging, &target).map_err(io(&target))
}

/// The ledger and SQLite's own `sqlite_` objects.
fn is_internal(name: &str) -> bool {
    name == LEDGER_TABLE
        || name
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("sqlite_"))
}

/// Reads `schema.json` from `dir`. An absent file reads as an empty schema.
pub(crate) fn load(dir: &Path) -> Result<SchemaFile, MigrateError> {
    let path = dir.join(FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(SchemaFile::default()),
        Err(source) => return Err(MigrateError::Io { path, source }),
    };
    serde_json::from_str(&text).map_err(|e| MigrateError::Parse {
        file: FILE_NAME.to_string(),
        path: ".".to_string(),
        message: e.to_string(),
    })
}

#[cfg(all(test, feature = "rusqlite"))]
mod tests {
    use super::*;
    use crate::executor::RusqliteExecutor;
    use rusqlite::Connection;

    fn executor(statements: &[&str]) -> RusqliteExecutor {
        let mut ex = RusqliteExecutor::new(Connection::open_in_memory().expect("open"));
        let statements: Vec<String> = statements.iter().map(|s| s.to_string()).collect();
        ex.run_atomic(&statements, false).expect("seed");
        ex
    }

    fn written(ex: &mut RusqliteExecutor) -> String {
        let dir = tempfile::tempdir().expect("tempdir");
        write_schema(ex, dir.path()).expect("write");
        fs::read_to_string(dir.path().join(FILE_NAME)).expect("read")
    }

    #[test]
    fn lists_objects_sorted_by_type_then_name_with_verbatim_sql() {
        let mut ex = executor(&[
            "CREATE TABLE zebra (id INTEGER PRIMARY KEY,   name TEXT UNIQUE)",
            "CREATE TABLE apple (id INTEGER PRIMARY KEY)",
            "CREATE INDEX zebra_name ON zebra(name)",
            "CREATE VIEW v AS SELECT id FROM apple",
            "CREATE TRIGGER t AFTER INSERT ON apple BEGIN SELECT 1; END",
        ]);
        let file: SchemaFile = serde_json::from_str(&written(&mut ex)).expect("parse");
        let listed: Vec<_> = file
            .objects
            .iter()
            .map(|o| (o.kind.as_str(), o.name.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("index", "zebra_name"),
                ("table", "apple"),
                ("table", "zebra"),
                ("trigger", "t"),
                ("view", "v"),
            ]
        );
        assert_eq!(
            file.objects[2].sql,
            "CREATE TABLE zebra (id INTEGER PRIMARY KEY,   name TEXT UNIQUE)"
        );
    }

    #[test]
    fn leaves_out_the_ledger_and_sqlite_objects_and_records_the_latest_version() {
        let mut ex = executor(&[
            "CREATE TABLE _migratr_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL)",
            "INSERT INTO _migratr_migrations VALUES (1, 'a', 'x', 't'), (7, 'b', 'y', 't')",
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, u TEXT UNIQUE)",
            "INSERT INTO t (u) VALUES ('x')",
        ]);
        let file: SchemaFile = serde_json::from_str(&written(&mut ex)).expect("parse");
        assert_eq!(file.version, Some(7));
        let names: Vec<_> = file.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["t"]);
    }

    #[test]
    fn an_empty_database_has_no_version_and_no_objects() {
        let mut ex = executor(&[]);
        let text = written(&mut ex);
        assert_eq!(text, "{\n  \"version\": null,\n  \"objects\": []\n}\n");
    }

    #[test]
    fn the_same_schema_writes_the_same_bytes() {
        let schema = [
            "CREATE TABLE b (id INTEGER PRIMARY KEY)",
            "CREATE TABLE a (id INTEGER PRIMARY KEY)",
        ];
        let first = written(&mut executor(&schema));
        let second = written(&mut executor(&schema));
        assert_eq!(first, second);
    }

    #[test]
    fn leaves_no_staging_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_schema(&mut executor(&[]), dir.path()).expect("write");
        let files: Vec<_> = fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(files, ["schema.json"]);
    }
}
