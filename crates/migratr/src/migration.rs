use std::collections::BTreeMap;
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
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

    let mut migrations = Vec::new();
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
        migrations.push(parse(&file, &contents)?);
    }

    migrations.sort_by_key(|m| m.version);

    let mut by_version: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for m in &migrations {
        by_version
            .entry(m.version)
            .or_default()
            .push(file_name(m.version, &m.name));
    }
    if let Some((version, files)) = by_version.into_iter().find(|(_, files)| files.len() > 1) {
        return Err(MigrateError::DuplicateVersion { version, files });
    }

    Ok(migrations)
}

fn file_name(version: u64, name: &str) -> String {
    format!("{version}_{name}.json")
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

    let mut de = serde_json::Deserializer::from_str(contents);
    let body: FileBody = serde_path_to_error::deserialize(&mut de).map_err(|e| {
        let message = match op_name(contents, e.path()) {
            Some(op) => format!("operation `{op}`: {}", e.inner()),
            None => e.inner().to_string(),
        };
        parse_err(&e.path().to_string(), message)
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

/// The `op` tag of the operation an error path points into, when it points into one.
fn op_name(contents: &str, path: &serde_path_to_error::Path) -> Option<String> {
    let mut segments = path.iter();
    let (Some(Segment::Map { key }), Some(Segment::Seq { index })) =
        (segments.next(), segments.next())
    else {
        return None;
    };
    if key != "up" {
        return None;
    }
    let doc: serde_json::Value = serde_json::from_str(contents).ok()?;
    doc.get("up")?
        .get(index)?
        .get("op")?
        .as_str()
        .map(str::to_string)
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
    fn unknown_field_is_refused_with_file_and_path() {
        let (path, message) =
            parse_err(r#"{"up": [{"op": "rename_table", "from": "a", "to": "b", "extra": 1}]}"#);
        assert_eq!(path, "up[0]");
        assert!(message.contains("extra"), "{message}");

        let (path, message) = parse_err(
            r#"{"up": [{"op": "add_column", "table": "t", "column":
                {"name": "c", "type": "TEXT", "bogus": 1}}]}"#,
        );
        assert!(path.starts_with("up[0]"), "{path}");
        assert!(message.contains("bogus"), "{message}");

        let (path, message) = parse_err(r#"{"up": [], "author": "x"}"#);
        assert_eq!(path, "author");
        assert!(message.contains("author"), "{message}");
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

        match load_dir(dir.path()) {
            Err(MigrateError::DuplicateVersion { version, mut files }) => {
                files.sort();
                assert_eq!(version, 20260101000000);
                assert_eq!(files, ["20260101000000_a.json", "20260101000000_b.json"]);
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
