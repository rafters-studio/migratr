//! The migration file format: its types, parser, checksum and ordering. Shared by the
//! migratr library and the `embed!` macro, which validates migrations at compile time.

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

/// The table and columns a foreign key references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentKey {
    pub table: String,
    pub columns: Vec<String>,
}

/// A foreign key from columns of a table to a parent key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKeyDef {
    pub table: String,
    pub columns: Vec<String>,
    pub references: ParentKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_update: Option<String>,
}

/// One key of an index: a column name, written in a file as a bare string, or an object naming
/// a column or an expression, either of which may be `desc` and may carry a `collate`.
///
/// A column that is only a name parses as [`IndexColumn::Name`] however it is written (`"a"`,
/// `{"name": "a"}` or `{"name": "a", "desc": false}`), so every spelling hashes the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum IndexColumn {
    Name(String),
    Column {
        name: String,
        desc: bool,
        /// The collation, written as `COLLATE <name>` after the column.
        #[serde(skip_serializing_if = "Option::is_none")]
        collate: Option<String>,
    },
    /// The expression, verbatim.
    Expr {
        expr: String,
        desc: bool,
        /// The collation, written as `COLLATE <name>` after the expression.
        #[serde(skip_serializing_if = "Option::is_none")]
        collate: Option<String>,
    },
}

/// The object form of an [`IndexColumn`] as a file writes it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexKey {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    expr: Option<String>,
    #[serde(default)]
    desc: bool,
    #[serde(default)]
    collate: Option<String>,
}

impl<'de> Deserialize<'de> for IndexColumn {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = IndexColumn;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(
                    "a column name, or an object with `name` or `expr`, `desc` and `collate`",
                )
            }

            fn visit_str<E: serde::de::Error>(self, name: &str) -> Result<IndexColumn, E> {
                Ok(IndexColumn::Name(name.to_string()))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<IndexColumn, A::Error> {
                let IndexKey {
                    name,
                    expr,
                    desc,
                    collate,
                } = IndexKey::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                match (name, expr) {
                    (Some(name), None) if !desc && collate.is_none() => Ok(IndexColumn::Name(name)),
                    (Some(name), None) => Ok(IndexColumn::Column {
                        name,
                        desc,
                        collate,
                    }),
                    (None, Some(expr)) => Ok(IndexColumn::Expr {
                        expr,
                        desc,
                        collate,
                    }),
                    _ => Err(serde::de::Error::custom(
                        "an index column has exactly one of `name` and `expr`",
                    )),
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// A table-level CHECK: an expression, written in a file as a string, or an object naming the
/// constraint, written as `CONSTRAINT "name" CHECK (expr)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Check {
    /// The expression, verbatim.
    Expr(String),
    Named {
        name: String,
        /// The expression, verbatim.
        expr: String,
    },
}

/// The object form of a [`Check`] as a file writes it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedCheck {
    name: String,
    expr: String,
}

impl<'de> Deserialize<'de> for Check {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Check;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an expression, or an object with `name` and `expr`")
            }

            fn visit_str<E: serde::de::Error>(self, expr: &str) -> Result<Check, E> {
                Ok(Check::Expr(expr.to_string()))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Check, A::Error> {
                let NamedCheck { name, expr } =
                    NamedCheck::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(Check::Named { name, expr })
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// An index with everything needed to recreate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexDef {
    pub name: String,
    pub table: String,
    pub columns: Vec<IndexColumn>,
    #[serde(default)]
    pub unique: bool,
    /// The WHERE expression of a partial index, verbatim.
    #[serde(default, rename = "where")]
    pub where_clause: Option<String>,
}

/// A trigger with everything needed to recreate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerDef {
    pub name: String,
    pub table: String,
    /// The CREATE TRIGGER statement, verbatim. It is hashed exactly as written, whitespace
    /// included, so reformatting it changes the migration's checksum. A create_trigger whose
    /// `name` or `table` differ from the ones the statement names is refused before anything
    /// runs.
    pub sql: String,
}

/// One schema operation.
///
/// Serde represents it externally tagged (`{"add_column": {...}}`); a migration file
/// writes the tag as an `op` field instead, and the parser converts between the two.
///
/// The list of operations grows, so code outside migratr that matches on it handles an
/// operation it does not know:
///
/// ```compile_fail
/// fn name(op: &migratr_format::Op) -> &'static str {
///     use migratr_format::Op::*;
///     match op {
///         CreateTable { .. } => "create_table",
///         DropTable { .. } => "drop_table",
///         RenameTable { .. } => "rename_table",
///         AddColumn { .. } => "add_column",
///         DropColumn { .. } => "drop_column",
///         RenameColumn { .. } => "rename_column",
///         CreateIndex { .. } => "create_index",
///         DropIndex { .. } => "drop_index",
///         AddForeignKey(_) => "add_foreign_key",
///         DropForeignKey { .. } => "drop_foreign_key",
///         CreateTrigger(_) => "create_trigger",
///         DropTrigger { .. } => "drop_trigger",
///         RawSql { .. } => "raw_sql",
///     }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Op {
    CreateTable {
        table: String,
        columns: Vec<Column>,
        #[serde(default)]
        without_rowid: bool,
        /// Table-level CHECKs, written in this order. The order is significant: reordering
        /// them changes the migration's checksum.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        checks: Vec<Check>,
        /// Table-level UNIQUE constraints, each a list of columns, written in this order. The
        /// order of the lists and of the columns in each is significant: reordering either
        /// changes the migration's checksum.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unique: Vec<Vec<String>>,
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
        columns: Vec<IndexColumn>,
        #[serde(default)]
        unique: bool,
        /// The WHERE expression of a partial index, verbatim.
        #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
        where_clause: Option<String>,
    },
    DropIndex {
        definition: IndexDef,
    },
    AddForeignKey(ForeignKeyDef),
    DropForeignKey {
        definition: ForeignKeyDef,
    },
    CreateTrigger(TriggerDef),
    DropTrigger {
        definition: TriggerDef,
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
    /// SHA-256 hex digest of the canonical JSON of `version`, `name` and `up`. An index column
    /// that is only a name has one canonical form however it is written; the order of checks,
    /// of unique lists and of their columns is significant, and a trigger's statement is
    /// hashed exactly as written.
    pub checksum: String,
}

/// A migration file that does not parse, or a set of files that repeats a version.
#[derive(Debug, Error)]
pub enum FormatError {
    #[error("{file}: at {path}: {message}")]
    Parse {
        file: String,
        path: String,
        message: String,
    },

    #[error("duplicate migration version {version}: {}", files.join(", "))]
    DuplicateVersion { version: u64, files: Vec<String> },
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

/// Parses (file name, contents) pairs and orders the migrations by version.
pub fn parse_all(sources: &[(&str, &str)]) -> Result<Vec<Migration>, FormatError> {
    let found = sources
        .iter()
        .map(|(file, contents)| Ok((file.to_string(), parse(file, contents)?)))
        .collect::<Result<_, FormatError>>()?;
    sort(found)
}

/// Orders (file name, migration) pairs by version and refuses a repeated version.
fn sort(mut found: Vec<(String, Migration)>) -> Result<Vec<Migration>, FormatError> {
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
        return Err(FormatError::DuplicateVersion { version, files });
    }

    Ok(found.into_iter().map(|(_, m)| m).collect())
}

/// Splits `<YYYYMMDDHHMMSS>_<snake_name>.json` into version and name.
pub fn split_file_name(file: &str) -> Option<(u64, String)> {
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

/// Parses one migration file's contents.
pub fn parse(file: &str, contents: &str) -> Result<Migration, FormatError> {
    let parse_err = |path: &str, message: String| FormatError::Parse {
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
