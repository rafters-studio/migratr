use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

pub use crate::format::{
    Column, ForeignKey, GeneratedColumn, IndexDef, Migration, Op, TableDef, split_file_name,
};
use crate::format::{FormatError, parse_all};

#[derive(Debug, Error)]
pub enum MigrateError {
    #[error("{file}: at {path}: {message}")]
    Parse {
        file: String,
        path: String,
        message: String,
    },

    #[error("duplicate migration version {version}: {}", files.join(", "))]
    DuplicateVersion { version: u64, files: Vec<String> },

    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error(
        "migration {version}_{name} was edited after it was applied. Applied migrations are not edited: restore the file to what was applied, then write a new migration for the change"
    )]
    ChecksumMismatch { version: u64, name: String },

    #[error("migration {version}_{name} is in the ledger but its file is missing")]
    MissingFile { version: u64, name: String },

    #[error("migration {version} failed and was rolled back{}: {source}", failure_site(.operation, .statement))]
    Apply {
        version: u64,
        /// The statement that failed. `None` when the failure was not in a statement.
        statement: Option<String>,
        /// Position within the migration's operations of the operation that produced
        /// `statement`. `None` for the ledger's own statements and for failures outside a
        /// statement.
        operation: Option<usize>,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("migration {version} cannot be reversed: operation {op_index} is raw SQL with no down")]
    Irreversible { version: u64, op_index: usize },

    #[error("cannot rebuild table {table}: its CREATE TABLE statement could not be parsed")]
    UnparseableTable { table: String },

    #[error("table {table}: found {rows} rows violating foreign keys")]
    ForeignKeyViolation { table: String, rows: u64 },

    #[error("cannot drop column {column} from table {table}: {object} uses it")]
    ColumnInUse {
        table: String,
        column: String,
        object: String,
    },

    #[error(
        "migration {version}: operation {operation} changes column {column} of table {table} \
         in a way that needs a table rebuild, and a rebuild must run before any other \
         operation of its migration in that direction; put operation {operation} in its own \
         migration"
    )]
    RebuildNotFirst {
        version: u64,
        table: String,
        column: String,
        operation: usize,
    },

    #[error("snapshot {}: {source}", path.display())]
    SnapshotFailed {
        path: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("restore needs confirmation; it would discard {}", report.discards())]
    RestoreNotConfirmed { report: Box<crate::RestoreReport> },

    #[error("{kind} {name} is not in schema.json")]
    UnknownObject { kind: String, name: String },

    #[error(transparent)]
    Executor(Box<dyn std::error::Error + Send + Sync>),
}

/// Names the failing operation and statement in an `Apply` message.
fn failure_site(operation: &Option<usize>, statement: &Option<String>) -> String {
    match (operation, statement) {
        (Some(operation), Some(statement)) => format!(" at operation {operation} ({statement})"),
        (None, Some(statement)) => format!(" at ({statement})"),
        _ => String::new(),
    }
}

/// Reads every `<YYYYMMDDHHMMSS>_<snake_name>.json` file in `path`, sorted by version.
/// Files without a `.json` extension, and `schema.json`, are ignored.
pub fn load_dir(path: &Path) -> Result<Vec<Migration>, MigrateError> {
    let io_err = |source| MigrateError::Io {
        path: path.to_path_buf(),
        source,
    };

    let mut found: Vec<(String, String)> = Vec::new();
    for entry in fs::read_dir(path).map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        let file_path = entry.path();
        if file_path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let file = entry.file_name().to_string_lossy().into_owned();
        if file == "schema.json" {
            continue;
        }
        let contents = fs::read_to_string(&file_path).map_err(|source| MigrateError::Io {
            path: file_path.clone(),
            source,
        })?;
        found.push((file, contents));
    }

    let sources: Vec<(&str, &str)> = found
        .iter()
        .map(|(file, contents)| (file.as_str(), contents.as_str()))
        .collect();
    Ok(parse_all(&sources)?)
}

/// Parses migration files given as (file name, contents) pairs, sorted by version.
pub(crate) fn load_sources(sources: &[(&str, &str)]) -> Result<Vec<Migration>, MigrateError> {
    Ok(parse_all(sources)?)
}

impl From<FormatError> for MigrateError {
    fn from(error: FormatError) -> Self {
        match error {
            FormatError::Parse {
                file,
                path,
                message,
            } => MigrateError::Parse {
                file,
                path,
                message,
            },
            FormatError::DuplicateVersion { version, files } => {
                MigrateError::DuplicateVersion { version, files }
            }
        }
    }
}

#[cfg(test)]
fn parse(file: &str, contents: &str) -> Result<Migration, MigrateError> {
    Ok(crate::format::parse(file, contents)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_OPS: &str = r#"{
      "up": [
        {"op": "create_table", "table": "users", "columns": [
          {"name": "id", "type": "INTEGER", "primary_key": 1},
          {"name": "email", "type": "TEXT", "not_null": true, "unique": true, "collation": "NOCASE"}
        ], "without_rowid": false},
        {"op": "rename_table", "from": "users", "to": "people"},
        {"op": "add_column", "table": "people", "column":
          {"name": "age", "type": "INTEGER", "default": "0", "check": "age >= 0"}},
        {"op": "rename_column", "table": "people", "from": "age", "to": "years"},
        {"op": "create_index", "name": "people_email", "table": "people", "columns": ["email"], "unique": true},
        {"op": "drop_index", "definition":
          {"name": "people_email", "table": "people", "columns": ["email"], "unique": true}},
        {"op": "drop_column", "table": "people", "column": {"name": "years", "type": "INTEGER"}},
        {"op": "drop_table", "table": "people", "definition": {
          "name": "people", "columns": [{"name": "id", "type": "INTEGER", "primary_key": 1}],
          "sql": "CREATE TABLE people (id INTEGER PRIMARY KEY)"}},
        {"op": "raw_sql", "up": "SELECT 1", "down": "SELECT 2"},
        {"op": "raw_sql", "up": "SELECT 3"}
      ]
    }"#;

    const FILE: &str = "20260101000000_init.json";

    fn parse_err(contents: &str) -> (String, String) {
        match parse(FILE, contents) {
            Err(MigrateError::Parse { path, message, .. }) => (path, message),
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    #[test]
    fn every_operation_parses() {
        let m = parse(FILE, ALL_OPS).expect("parse");
        assert_eq!(m.version, 20260101000000);
        assert_eq!(m.name, "init");
        assert_eq!(m.up.len(), 10);
        assert!(matches!(m.up[8], Op::RawSql { down: Some(_), .. }));
        assert!(matches!(m.up[9], Op::RawSql { down: None, .. }));
    }

    #[test]
    fn unknown_operation_is_refused_with_file_and_path() {
        let err = parse(FILE, r#"{"up": [{"op": "explode"}]}"#).expect_err("refused");
        let text = err.to_string();
        assert!(text.contains(FILE), "{text}");
        assert!(text.contains("up[0]"), "{text}");
        assert!(text.contains("explode"), "{text}");
    }

    #[test]
    fn unknown_and_malformed_fields_are_refused_at_their_exact_path() {
        let cases = [
            (
                r#"{"up": [{"op": "rename_table", "from": "a", "to": "b", "extra": 1}]}"#,
                "up[0].extra",
                "extra",
            ),
            (
                r#"{"up": [{"op": "add_column", "table": "t", "column":
                    {"name": "c", "type": "TEXT", "bogus": 1}}]}"#,
                "up[0].column.bogus",
                "bogus",
            ),
            (
                r#"{"up": [{"op": "add_column", "table": "t", "column": {"name": "c"}}]}"#,
                "up[0].column",
                "type",
            ),
            (
                r#"{"up": [{"op": "drop_table", "table": "t", "definition":
                    {"name": "t", "columns": [], "sql": "x", "bogus": 1}}]}"#,
                "up[0].definition.bogus",
                "bogus",
            ),
            (
                r#"{"up": [{"op": "create_table", "table": "t", "columns":
                    [{"name": "c", "type": "TEXT", "primary_key": "x"}]}]}"#,
                "up[0].columns[0].primary_key",
                "primary_key",
            ),
            (r#"{"up": [], "author": "x"}"#, "author", "author"),
        ];
        for (contents, expected_path, expected_text) in cases {
            let (path, message) = parse_err(contents);
            assert_eq!(path, expected_path, "{message}");
            assert!(
                message.contains(expected_text) || path.contains(expected_text),
                "{message}"
            );
        }
    }

    #[test]
    fn operation_without_op_tag_is_refused() {
        let (path, message) = parse_err(r#"{"up": [{"table": "t"}]}"#);
        assert_eq!(path, "up[0]");
        assert!(message.contains("op"), "{message}");
    }

    #[test]
    fn drop_without_definition_is_refused_naming_the_operation() {
        for (op, body) in [
            ("drop_table", r#"{"op": "drop_table", "table": "t"}"#),
            ("drop_column", r#"{"op": "drop_column", "table": "t"}"#),
            ("drop_index", r#"{"op": "drop_index"}"#),
        ] {
            let (_, message) = parse_err(&format!(r#"{{"up": [{body}]}}"#));
            assert!(message.contains(op), "{message}");
        }
    }

    #[test]
    fn checksum_ignores_whitespace_and_tracks_content() {
        let compact: serde_json::Value = serde_json::from_str(ALL_OPS).expect("json");
        let compact = serde_json::to_string(&compact).expect("to_string");
        let a = parse(FILE, ALL_OPS).expect("parse");
        let b = parse(FILE, &compact).expect("parse");
        assert_eq!(a.checksum, b.checksum);
        assert_eq!(a.checksum.len(), 64);

        let changed = ALL_OPS.replace("SELECT 3", "SELECT 4");
        assert_ne!(a.checksum, parse(FILE, &changed).expect("parse").checksum);
    }

    #[test]
    fn bad_file_names_are_refused() {
        for file in [
            "init.json",
            "2026_init.json",
            "20260101000000_Init.json",
            "20260101000000_.json",
            "20260101000000-init.json",
        ] {
            assert!(
                matches!(
                    parse(file, r#"{"up": []}"#),
                    Err(MigrateError::Parse { .. })
                ),
                "{file}"
            );
        }
    }

    fn write(dir: &Path, file: &str, contents: &str) {
        fs::write(dir.join(file), contents).expect("write");
    }

    #[test]
    fn load_dir_sorts_by_version_and_ignores_other_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "20260301000000_third.json", r#"{"up": []}"#);
        write(dir.path(), "20260101000000_first.json", r#"{"up": []}"#);
        write(dir.path(), "20260201000000_second.json", r#"{"up": []}"#);
        write(dir.path(), "README.md", "not a migration");

        let names: Vec<_> = load_dir(dir.path())
            .expect("load")
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, ["first", "second", "third"]);
    }

    #[test]
    fn load_dir_ignores_schema_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "schema.json",
            r#"{"version": null, "objects": []}"#,
        );
        write(dir.path(), "20260101000000_first.json", r#"{"up": []}"#);
        assert_eq!(load_dir(dir.path()).expect("load").len(), 1);
    }

    #[test]
    fn load_dir_refuses_duplicate_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "20260101000000_a.json", r#"{"up": []}"#);
        write(dir.path(), "20260101000000_b.json", r#"{"up": []}"#);
        write(dir.path(), "20260101000000_c.json", r#"{"up": []}"#);
        write(dir.path(), "20260201000000_d.json", r#"{"up": []}"#);

        match load_dir(dir.path()) {
            Err(MigrateError::DuplicateVersion { version, mut files }) => {
                files.sort();
                assert_eq!(version, 20260101000000);
                assert_eq!(
                    files,
                    [
                        "20260101000000_a.json",
                        "20260101000000_b.json",
                        "20260101000000_c.json"
                    ]
                );
            }
            other => panic!("expected duplicate version, got {other:?}"),
        }
    }

    #[test]
    fn load_dir_reports_the_bad_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "20260101000000_bad.json",
            r#"{"up": [{"op": "nope"}]}"#,
        );

        let err = load_dir(dir.path()).expect_err("refused");
        assert!(err.to_string().contains("20260101000000_bad.json"));
    }
}
