use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_path_to_error::Segment;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// A foreign key on a column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKey {
    pub table: String,
    pub column: String,
    #[serde(default)]
    pub on_update: Option<String>,
    #[serde(default)]
    pub on_delete: Option<String>,
}

/// A generated column's expression and storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedColumn {
    pub expr: String,
    pub stored: bool,
}

/// A column with everything needed to recreate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub name: String,
    /// The declared type, verbatim.
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub not_null: bool,
    /// 1-based position within the primary key; absent when not part of it.
    #[serde(default)]
    pub primary_key: Option<u32>,
    #[serde(default)]
    pub unique: bool,
    /// The default expression, verbatim.
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub collation: Option<String>,
    /// The CHECK expression, verbatim.
    #[serde(default)]
    pub check: Option<String>,
    #[serde(default)]
    pub references: Option<ForeignKey>,
    #[serde(default)]
    pub generated: Option<GeneratedColumn>,
}

/// A table with everything needed to recreate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableDef {
    pub name: String,
    pub columns: Vec<Column>,
    #[serde(default)]
    pub without_rowid: bool,
    /// The CREATE TABLE statement, verbatim.
    pub sql: String,
}

/// An index with everything needed to recreate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexDef {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    #[serde(default)]
    pub unique: bool,
    /// The WHERE expression of a partial index, verbatim.
    #[serde(default, rename = "where")]
    pub where_clause: Option<String>,
}

/// One schema operation.
///
/// Serde represents it externally tagged (`{"add_column": {...}}`); a migration file
/// writes the tag as an `op` field instead, and the loader converts between the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    CreateTable {
        table: String,
        columns: Vec<Column>,
        #[serde(default)]
        without_rowid: bool,
    },
    DropTable {
        table: String,
        definition: TableDef,
    },
    RenameTable {
        from: String,
        to: String,
    },
    AddColumn {
        table: String,
        column: Column,
    },
    DropColumn {
        table: String,
        column: Column,
    },
    RenameColumn {
        table: String,
        from: String,
        to: String,
    },
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        #[serde(default)]
        unique: bool,
    },
    DropIndex {
        definition: IndexDef,
    },
    RawSql {
        up: String,
        #[serde(default)]
        down: Option<String>,
    },
}

/// A parsed migration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The UTC timestamp from the file name, `YYYYMMDDHHMMSS`.
    pub version: u64,
    /// The snake_case name from the file name.
    pub name: String,
    pub up: Vec<Op>,
    /// SHA-256 hex digest of the canonical JSON of `version`, `name` and `up`.
    pub checksum: String,
}

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
        "migration {version}_{name} was edited after it was applied: its checksum no longer matches the ledger"
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

/// The JSON body of a migration file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileBody {
    up: Vec<Op>,
}

/// The fields the checksum covers, in the order they are serialized.
#[derive(Serialize)]
struct Canonical<'a> {
    version: u64,
    name: &'a str,
    up: &'a [Op],
}

/// Reads every `<YYYYMMDDHHMMSS>_<snake_name>.json` file in `path`, sorted by version.
/// Files without a `.json` extension are ignored.
pub fn load_dir(path: &Path) -> Result<Vec<Migration>, MigrateError> {
    let io_err = |source| MigrateError::Io {
        path: path.to_path_buf(),
        source,
    };

    let mut found: Vec<(String, Migration)> = Vec::new();
    for entry in fs::read_dir(path).map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        let file_path = entry.path();
        if file_path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let file = entry.file_name().to_string_lossy().into_owned();
        let contents = fs::read_to_string(&file_path).map_err(|source| MigrateError::Io {
            path: file_path.clone(),
            source,
        })?;
        let migration = parse(&file, &contents)?;
        found.push((file, migration));
    }

    found.sort_by(|a, b| (a.1.version, &a.0).cmp(&(b.1.version, &b.0)));

    if let Some(first) = found
        .windows(2)
        .position(|w| w[0].1.version == w[1].1.version)
    {
        let version = found[first].1.version;
        let files = found[first..]
            .iter()
            .take_while(|(_, m)| m.version == version)
            .map(|(file, _)| file.clone())
            .collect();
        return Err(MigrateError::DuplicateVersion { version, files });
    }

    Ok(found.into_iter().map(|(_, m)| m).collect())
}

/// Splits `<YYYYMMDDHHMMSS>_<snake_name>.json` into version and name.
fn split_file_name(file: &str) -> Option<(u64, String)> {
    let stem = file.strip_suffix(".json")?;
    let (timestamp, name) = stem.split_once('_')?;
    let is_snake = name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if timestamp.len() != 14 || !timestamp.bytes().all(|b| b.is_ascii_digit()) || !is_snake {
        return None;
    }
    Some((timestamp.parse().ok()?, name.to_string()))
}

fn parse(file: &str, contents: &str) -> Result<Migration, MigrateError> {
    let parse_err = |path: &str, message: String| MigrateError::Parse {
        file: file.to_string(),
        path: path.to_string(),
        message,
    };

    let (version, name) = split_file_name(file).ok_or_else(|| {
        parse_err(
            ".",
            "file name must be <YYYYMMDDHHMMSS>_<snake_name>.json".to_string(),
        )
    })?;

    let mut doc: serde_json::Value =
        serde_json::from_str(contents).map_err(|e| parse_err(".", e.to_string()))?;
    let tags = retag_ops(&mut doc).map_err(|(path, message)| parse_err(&path, message))?;
    let body: FileBody = serde_path_to_error::deserialize(doc).map_err(|e| {
        let segments: Vec<_> = e.path().iter().collect();
        let tag = match segments.as_slice() {
            [
                Segment::Map { key },
                Segment::Seq { index },
                Segment::Map { key: op } | Segment::Enum { variant: op },
                ..,
            ] if key == "up" && tags.get(*index).and_then(Option::as_ref) == Some(op) => {
                Some(op.as_str())
            }
            _ => None,
        };
        let message = match tag {
            Some(op) => format!("operation `{op}`: {}", e.inner()),
            None => e.inner().to_string(),
        };
        parse_err(&render_path(&segments, tag.is_some()), message)
    })?;

    let canonical = serde_json::to_vec(&Canonical {
        version,
        name: &name,
        up: &body.up,
    })
    .map_err(|e| parse_err(".", e.to_string()))?;
    let checksum = format!("{:x}", Sha256::digest(&canonical));

    Ok(Migration {
        version,
        name,
        up: body.up,
        checksum,
    })
}

/// Rewrites each `{"op": "x", ...fields}` in `up` to the externally tagged
/// `{"x": {...fields}}` serde reads, so error paths reach into the operation's fields.
/// Returns each element's tag, or `None` for an element that is not an object.
fn retag_ops(doc: &mut serde_json::Value) -> Result<Vec<Option<String>>, (String, String)> {
    let Some(serde_json::Value::Array(ops)) = doc.get_mut("up") else {
        return Ok(Vec::new());
    };
    let mut tags = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter_mut().enumerate() {
        let serde_json::Value::Object(fields) = op else {
            tags.push(None);
            continue;
        };
        let tag = match fields.remove("op") {
            Some(serde_json::Value::String(tag)) => tag,
            Some(_) => return Err((format!("up[{i}].op"), "`op` must be a string".to_string())),
            None => return Err((format!("up[{i}]"), "missing field `op`".to_string())),
        };
        let mut wrapper = serde_json::Map::new();
        wrapper.insert(
            tag.clone(),
            serde_json::Value::Object(std::mem::take(fields)),
        );
        *op = serde_json::Value::Object(wrapper);
        tags.push(Some(tag));
    }
    Ok(tags)
}

/// Renders a path as `up[0].column.name`, leaving out the variant segment (the third)
/// when `skip_tag` is set, since the file spells that tag as an `op` field.
fn render_path(segments: &[&Segment], skip_tag: bool) -> String {
    let mut out = String::new();
    for (i, segment) in segments.iter().enumerate() {
        match segment {
            _ if skip_tag && i == 2 => {}
            Segment::Seq { index } => out.push_str(&format!("[{index}]")),
            Segment::Map { key } | Segment::Enum { variant: key } => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Segment::Unknown => out.push_str(".?"),
        }
    }
    if out.is_empty() { ".".to_string() } else { out }
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
