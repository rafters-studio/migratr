//! `new`: scaffolds a migration file from a Rails-style name and column specs.

use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::migration::{
    Column, ForeignKey, GeneratedColumn, IndexDef, MigrateError, Op, TableDef, split_file_name,
};
use crate::schema_file::{self, SchemaFile, SchemaFileObject};
use crate::sql_ddl::{Token, TokenKind, has_keyword, table_body, tokens};

/// Writes `<timestamp>_<name>.json` into `dir` and returns its path.
///
/// The name picks the operation: `create_<table>` and `add_<column>_to_<table>` build columns
/// from `specs` (`name[:type][:modifier...]`, with modifiers `pk`, `notnull`, `unique` and
/// `default=<expr>`); `drop_<table or index>` and `remove_<column>_from_<table>` copy the
/// dropped object's definition from `dir`'s schema.json. Any other name gets an empty `up`.
pub fn scaffold(dir: &Path, name: &str, specs: &[String]) -> Result<PathBuf, MigrateError> {
    let ops = operations(dir, name, specs)?;

    let up: Vec<Value> = ops
        .iter()
        .map(|op| file_form(op).map_err(|message| parse_error(name, ".", message)))
        .collect::<Result<_, _>>()?;
    let mut text = serde_json::to_string_pretty(&json!({ "up": up }))
        .map_err(|e| parse_error(name, ".", e.to_string()))?;
    text.push('\n');

    let io = |path: &Path, source| MigrateError::Io {
        path: path.to_path_buf(),
        source,
    };
    fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    let version = next_version(dir)?;
    let file = format!("{version}_{name}.json");
    if split_file_name(&file).is_none() {
        return Err(parse_error(
            &file,
            ".",
            "file name must be <YYYYMMDDHHMMSS>_<snake_name>.json".to_string(),
        ));
    }
    let path = dir.join(&file);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| io(&path, e))?;
    Ok(path)
}

fn parse_error(file: &str, path: &str, message: String) -> MigrateError {
    MigrateError::Parse {
        file: file.to_string(),
        path: path.to_string(),
        message,
    }
}

fn operations(dir: &Path, name: &str, specs: &[String]) -> Result<Vec<Op>, MigrateError> {
    let spec_error = |spec: &str, message: &str| parse_error(name, spec, message.to_string());

    if let Some(table) = name.strip_prefix("create_").filter(|t| !t.is_empty()) {
        if specs.is_empty() {
            return Err(spec_error(".", "a table needs at least one column spec"));
        }
        let mut columns = parse_specs(name, specs)?;
        let mut position = 0;
        for column in columns.iter_mut().filter(|c| c.primary_key.is_some()) {
            position += 1;
            column.primary_key = Some(position);
        }
        return Ok(vec![Op::CreateTable {
            table: table.to_string(),
            columns,
            without_rowid: false,
        }]);
    }

    if let Some((column, table)) = name
        .strip_prefix("add_")
        .and_then(|rest| rest.rsplit_once("_to_"))
        .filter(|(c, t)| !c.is_empty() && !t.is_empty())
    {
        let columns = if specs.is_empty() {
            parse_specs(name, &[format!("{column}:text")])?
        } else {
            parse_specs(name, specs)?
        };
        return Ok(columns
            .into_iter()
            .map(|column| Op::AddColumn {
                table: table.to_string(),
                column,
            })
            .collect());
    }

    let drop_table = name.strip_prefix("drop_").filter(|t| !t.is_empty());
    let remove = name
        .strip_prefix("remove_")
        .and_then(|rest| rest.rsplit_once("_from_"))
        .filter(|(c, t)| !c.is_empty() && !t.is_empty());
    if let Some(spec) = specs.first() {
        return Err(spec_error(
            spec,
            "column specs are only read by create_ and add_ names",
        ));
    }
    if drop_table.is_none() && remove.is_none() {
        return Ok(Vec::new());
    }

    let schema = schema_file::load(dir)?;
    if let Some((column, table)) = remove {
        let object = find(&schema, "table", table).ok_or_else(|| unknown("table", table))?;
        let columns = table_columns(&object.sql).ok_or_else(|| unreadable(object))?;
        let column = columns
            .into_iter()
            .find(|c| c.name.eq_ignore_ascii_case(column))
            .ok_or_else(|| unknown("column", &format!("{column} of table {table}")))?;
        return Ok(vec![Op::DropColumn {
            table: object.name.clone(),
            column,
        }]);
    }

    let target = drop_table.unwrap_or_default();
    if let Some(object) = find(&schema, "table", target) {
        let columns = table_columns(&object.sql).ok_or_else(|| unreadable(object))?;
        let without_rowid = table_body(&object.sql)
            .is_some_and(|body| has_keyword(&object.sql[body.close + 1..], "ROWID"));
        return Ok(vec![Op::DropTable {
            table: object.name.clone(),
            definition: TableDef {
                name: object.name.clone(),
                columns,
                without_rowid,
                sql: object.sql.clone(),
            },
        }]);
    }
    let object = find(&schema, "index", target).ok_or_else(|| unknown("table or index", target))?;
    let definition = index_def(object).ok_or_else(|| unreadable(object))?;
    Ok(vec![Op::DropIndex { definition }])
}

fn find<'a>(schema: &'a SchemaFile, kind: &str, name: &str) -> Option<&'a SchemaFileObject> {
    schema
        .objects
        .iter()
        .find(|o| o.kind == kind && o.name.eq_ignore_ascii_case(name))
}

fn unknown(kind: &str, name: &str) -> MigrateError {
    MigrateError::UnknownObject {
        kind: kind.to_string(),
        name: name.to_string(),
    }
}

fn unreadable(object: &SchemaFileObject) -> MigrateError {
    parse_error(
        "schema.json",
        &format!("{} {}", object.kind, object.name),
        "its SQL has a form scaffolding cannot copy".to_string(),
    )
}

fn parse_specs(name: &str, specs: &[String]) -> Result<Vec<Column>, MigrateError> {
    specs
        .iter()
        .map(|spec| parse_spec(spec).map_err(|message| parse_error(name, spec, message)))
        .collect()
}

/// A column with a name and type and no constraints.
fn bare_column(name: &str, type_name: &str) -> Column {
    Column {
        name: name.to_string(),
        type_name: type_name.to_string(),
        not_null: false,
        primary_key: None,
        unique: false,
        default: None,
        collation: None,
        check: None,
        references: None,
        generated: None,
    }
}

/// One `name[:type][:modifier...]` column spec. Without a type the column is TEXT. A `pk`
/// column is marked with position 1; the caller numbers a composite key.
fn parse_spec(spec: &str) -> Result<Column, String> {
    let mut parts = spec.split(':');
    let name = parts.next().unwrap_or_default();
    if name.is_empty() {
        return Err("column spec has no name".to_string());
    }
    let mut column = bare_column(name, "TEXT");
    for (i, part) in parts.enumerate() {
        match part {
            "pk" => column.primary_key = Some(1),
            "notnull" => column.not_null = true,
            "unique" => column.unique = true,
            _ => {
                if let Some(expr) = part.strip_prefix("default=") {
                    column.default = Some(expr.to_string());
                } else if i == 0 {
                    column.type_name = match part {
                        "text" => "TEXT",
                        "int" | "integer" => "INTEGER",
                        "real" => "REAL",
                        "blob" => "BLOB",
                        _ => return Err(format!("unknown type `{part}`")),
                    }
                    .to_string();
                } else {
                    return Err(format!("unknown modifier `{part}`"));
                }
            }
        }
    }
    Ok(column)
}

/// The migration file's JSON for `op`: serde's externally tagged form rewritten with an `op`
/// field, absent values left out.
fn file_form(op: &Op) -> Result<Value, String> {
    let Value::Object(tagged) = serde_json::to_value(op).map_err(|e| e.to_string())? else {
        return Err("operation is not an object".to_string());
    };
    let mut fields = Map::new();
    for (tag, body) in tagged {
        fields.insert("op".to_string(), Value::String(tag));
        if let Value::Object(body) = without_nulls(body) {
            fields.extend(body);
        }
    }
    Ok(Value::Object(fields))
}

fn without_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, without_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(without_nulls).collect()),
        other => other,
    }
}

/// The version for a new file: the current UTC time as `YYYYMMDDHHMMSS`, or one past the
/// newest version in `dir` when that is later, so versions stay unique.
fn next_version(dir: &Path) -> Result<u64, MigrateError> {
    let io = |source| MigrateError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let mut newest = 0;
    for entry in fs::read_dir(dir).map_err(io)? {
        let entry = entry.map_err(io)?;
        if let Some((version, _)) = split_file_name(&entry.file_name().to_string_lossy()) {
            newest = newest.max(version);
        }
    }
    Ok(utc_timestamp().max(newest + 1))
}

fn utc_timestamp() -> u64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let (hour, minute, second) = (rest / 3_600, rest % 3_600 / 60, rest % 60);
    ((((year * 100 + month) * 100 + day) * 100 + hour) * 100 + minute) * 100 + second
}

// Reading definitions back out of stored SQL.

/// The columns of a stored CREATE TABLE, with a table-level PRIMARY KEY applied to them.
/// `None` when the statement does not split or a column has a form [`parse_column`] refuses.
fn table_columns(sql: &str) -> Option<Vec<Column>> {
    let body = table_body(sql)?;
    let mut columns = Vec::new();
    let mut key: Vec<String> = Vec::new();
    for range in body.items {
        let item = &sql[range];
        let toks = tokens(item);
        let first = toks.first()?;
        let is_constraint = first.kind == TokenKind::Bare
            && ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"]
                .iter()
                .any(|w| first.is_keyword(w));
        if !is_constraint {
            columns.push(parse_column(item)?);
        } else if let Some(at) = toks.iter().position(|t| t.is_keyword("PRIMARY")) {
            let (inner, _) = group(&toks, at + 2, item)?;
            key = tokens(&item[inner])
                .into_iter()
                .filter(|t| t.kind != TokenKind::Punct)
                .filter(|t| !t.is_keyword("ASC") && !t.is_keyword("DESC"))
                .map(|t| t.text)
                .collect();
        }
    }
    for (i, name) in key.iter().enumerate() {
        let column = columns
            .iter_mut()
            .find(|c| c.name.eq_ignore_ascii_case(name))?;
        column.primary_key = Some(u32::try_from(i + 1).ok()?);
    }
    Some(columns)
}

const CONSTRAINT_WORDS: [&str; 15] = [
    "CONSTRAINT",
    "PRIMARY",
    "NOT",
    "NULL",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "COLLATE",
    "REFERENCES",
    "GENERATED",
    "AS",
    "ON",
    "MATCH",
    "DEFERRABLE",
    "INITIALLY",
];

/// One column definition: its name, its declared type verbatim, then its constraints. `None`
/// for a form the column model cannot hold, such as a REFERENCES with no column list.
fn parse_column(item: &str) -> Option<Column> {
    let toks = tokens(item);
    let name = toks.first().filter(|t| t.kind != TokenKind::Punct)?;
    let mut column = bare_column(&name.text, "");

    let type_end = toks[1..]
        .iter()
        .position(|t| CONSTRAINT_WORDS.iter().any(|w| t.is_keyword(w)))
        .map_or(toks.len(), |p| p + 1);
    if type_end > 1 {
        column.type_name = item[toks[1].span.start..toks[type_end - 1].span.end].to_string();
    }

    let mut i = type_end;
    while i < toks.len() {
        let word = |k: usize, w: &str| toks.get(i + k).is_some_and(|t| t.is_keyword(w));
        if word(0, "PRIMARY") {
            column.primary_key = Some(1);
            i += 1;
        } else if word(0, "NOT") && word(1, "NULL") {
            column.not_null = true;
            i += 2;
        } else if word(0, "UNIQUE") {
            column.unique = true;
            i += 1;
        } else if word(0, "CHECK") {
            let (inner, next) = group(&toks, i + 1, item)?;
            column.check = Some(item[inner].trim().to_string());
            i = next;
        } else if word(0, "DEFAULT") {
            let (value, next) = default_value(item, &toks, i)?;
            column.default = Some(value);
            i = next;
        } else if word(0, "COLLATE") {
            column.collation = Some(toks.get(i + 1)?.text.clone());
            i += 2;
        } else if word(0, "REFERENCES") {
            let mut at = i + 1;
            let mut table = toks.get(at)?.text.clone();
            if toks.get(at + 1).is_some_and(|t| t.text == ".") {
                at += 2;
                table = toks.get(at)?.text.clone();
            }
            let (inner, next) = group(&toks, at + 1, item)?;
            let mut parent = tokens(&item[inner]);
            if parent.len() != 1 {
                return None;
            }
            column.references = Some(ForeignKey {
                table,
                column: parent.remove(0).text,
                on_update: None,
                on_delete: None,
            });
            i = next;
        } else if word(0, "ON") && (word(1, "UPDATE") || word(1, "DELETE")) {
            let is_update = word(1, "UPDATE");
            let width = if word(2, "SET") || word(2, "NO") {
                2
            } else {
                1
            };
            let action = toks
                .get(i + 2..i + 2 + width)?
                .iter()
                .map(|t| t.text.to_ascii_uppercase())
                .collect::<Vec<_>>()
                .join(" ");
            let fk = column.references.as_mut()?;
            if is_update {
                fk.on_update = Some(action);
            } else {
                fk.on_delete = Some(action);
            }
            i += 2 + width;
        } else if word(0, "AS") {
            let (inner, next) = group(&toks, i + 1, item)?;
            column.generated = Some(GeneratedColumn {
                expr: item[inner].trim().to_string(),
                stored: toks.get(next).is_some_and(|t| t.is_keyword("STORED")),
            });
            i = next;
        } else if word(0, "CONSTRAINT") {
            i += 2;
        } else if [
            "KEY",
            "NULL",
            "ASC",
            "DESC",
            "AUTOINCREMENT",
            "GENERATED",
            "ALWAYS",
            "STORED",
            "VIRTUAL",
        ]
        .iter()
        .any(|w| word(0, w))
        {
            i += 1;
        } else {
            // A clause the column model cannot hold, such as ON CONFLICT or DEFERRABLE.
            return None;
        }
    }
    Some(column)
}

/// The parenthesised group whose `(` is token `open`: the byte range between the
/// parentheses and the index of the token after the closing one.
fn group(toks: &[Token], open: usize, item: &str) -> Option<(Range<usize>, usize)> {
    let opening = toks.get(open).filter(|t| t.text == "(")?;
    let mut depth = 0usize;
    for (at, token) in toks.iter().enumerate().skip(open) {
        if token.kind != TokenKind::Punct {
            continue;
        }
        match token.text.as_str() {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    let inner = opening.span.end..token.span.start;
                    return item.get(inner.clone()).map(|_| (inner, at + 1));
                }
            }
            _ => {}
        }
    }
    None
}

/// The default expression after the DEFAULT keyword at token `at`, as the column model keeps
/// it: a parenthesised expression without its parentheses, or a literal as written. Also the
/// index of the token after it.
fn default_value(item: &str, toks: &[Token], at: usize) -> Option<(String, usize)> {
    let from = toks[at].span.end;
    let start = from + (item[from..].len() - item[from..].trim_start().len());
    let bytes = item.as_bytes();

    if bytes.get(start) == Some(&b'(') {
        let open = toks.iter().position(|t| t.span.start == start)?;
        let (inner, next) = group(toks, open, item)?;
        return Some((item[inner].trim().to_string(), next));
    }

    let quoted_end = |from: usize| -> Option<usize> {
        let quote = *bytes.get(from)?;
        let mut j = from + 1;
        while j < bytes.len() {
            if bytes[j] == quote {
                if bytes.get(j + 1) == Some(&quote) {
                    j += 2;
                    continue;
                }
                return Some(j + 1);
            }
            j += 1;
        }
        None
    };
    let end = if matches!(bytes.get(start)?, b'\'' | b'"' | b'`') {
        quoted_end(start)?
    } else {
        let mut j = start;
        while bytes.get(j).is_some_and(|b| matches!(b, b'+' | b'-')) {
            j += 1;
        }
        let words = j;
        while bytes.get(j).is_some_and(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'_' | b'.' | b'$')
                // The sign of a decimal exponent, as in 1.5e-3.
                || (matches!(b, b'+' | b'-')
                    && j > words
                    && matches!(bytes[j - 1], b'e' | b'E')
                    && bytes[words].is_ascii_digit())
        }) {
            j += 1;
        }
        if j == words {
            return None;
        }
        // A blob literal, X'..', carries its quoted body.
        if bytes.get(j) == Some(&b'\'') {
            j = quoted_end(j)?;
        }
        j
    };
    // A literal followed by anything but whitespace or the item's end was not read whole.
    if bytes.get(end).is_some_and(|b| !b.is_ascii_whitespace()) {
        return None;
    }
    let next = toks
        .iter()
        .position(|t| t.span.start >= end)
        .unwrap_or(toks.len());
    Some((item[start..end].to_string(), next))
}

/// An index's definition from its stored CREATE INDEX. `None` when a key is anything but a
/// plain column name, since the definition holds column names only.
fn index_def(object: &SchemaFileObject) -> Option<IndexDef> {
    let sql = object.sql.as_str();
    let toks = tokens(sql);
    let on = toks.iter().position(|t| t.is_keyword("ON"))?;
    let mut at = on + 1;
    let mut table = toks.get(at)?.text.clone();
    if toks.get(at + 1).is_some_and(|t| t.text == ".") {
        at += 2;
        table = toks.get(at)?.text.clone();
    }
    let (inner, next) = group(&toks, at + 1, sql)?;
    let columns = sql[inner]
        .split(',')
        .map(|key| {
            let mut key = tokens(key);
            (key.len() == 1).then(|| key.remove(0).text)
        })
        .collect::<Option<Vec<_>>>()?;
    let where_clause = match toks.get(next) {
        Some(t) if t.is_keyword("WHERE") => Some(sql[t.span.end..].trim().to_string()),
        Some(_) => return None,
        None => None,
    };
    Some(IndexDef {
        name: object.name.clone(),
        table,
        columns,
        unique: toks.get(1).is_some_and(|t| t.is_keyword("UNIQUE")),
        where_clause,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_dir;

    fn specs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn schema_dir(objects: &[(&str, &str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let objects: Vec<Value> = objects
            .iter()
            .map(|(kind, name, sql)| json!({"type": kind, "name": name, "sql": sql}))
            .collect();
        let text = json!({"version": 1, "objects": objects}).to_string();
        fs::write(dir.path().join("schema.json"), text).expect("write schema.json");
        dir
    }

    fn only_op(dir: &Path, path: &Path) -> Op {
        let migrations = load_dir(dir).expect("scaffolded file loads");
        assert_eq!(migrations.len(), 1, "{path:?}");
        migrations[0].up[0].clone()
    }

    #[test]
    fn create_writes_a_loadable_create_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = scaffold(
            dir.path(),
            "create_users",
            &specs(&["id:pk", "email:text:notnull", "age:int:default=0"]),
        )
        .expect("scaffold");
        assert!(path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            n.ends_with("_create_users.json") && n.len() == 14 + "_create_users.json".len()
        }));
        let Op::CreateTable { table, columns, .. } = only_op(dir.path(), &path) else {
            panic!("expected create_table");
        };
        assert_eq!(table, "users");
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[0].type_name, "TEXT");
        assert_eq!(columns[0].primary_key, Some(1));
        assert_eq!(columns[1].type_name, "TEXT");
        assert!(columns[1].not_null);
        assert_eq!(columns[2].type_name, "INTEGER");
        assert_eq!(columns[2].default.as_deref(), Some("0"));
    }

    #[test]
    fn a_composite_key_numbers_its_columns_in_spec_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = scaffold(
            dir.path(),
            "create_links",
            &specs(&["a:int:pk", "b:text", "c:text:pk"]),
        )
        .expect("scaffold");
        let Op::CreateTable { columns, .. } = only_op(dir.path(), &path) else {
            panic!("expected create_table");
        };
        let keys: Vec<_> = columns.iter().map(|c| c.primary_key).collect();
        assert_eq!(keys, [Some(1), None, Some(2)]);
    }

    #[test]
    fn add_builds_one_add_column_per_spec_and_defaults_to_the_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        scaffold(
            dir.path(),
            "add_age_to_users",
            &specs(&["age:int:notnull:default=0"]),
        )
        .expect("scaffold");
        let ops = &load_dir(dir.path()).expect("load")[0].up;
        assert!(matches!(
            &ops[0],
            Op::AddColumn { table, column }
                if table == "users" && column.name == "age" && column.type_name == "INTEGER"
                    && column.not_null && column.default.as_deref() == Some("0")
        ));

        let other = tempfile::tempdir().expect("tempdir");
        scaffold(other.path(), "add_nickname_to_user_accounts", &[]).expect("scaffold");
        let ops = &load_dir(other.path()).expect("load")[0].up;
        assert!(matches!(
            &ops[0],
            Op::AddColumn { table, column }
                if table == "user_accounts" && column.name == "nickname"
        ));
    }

    #[test]
    fn other_names_get_an_empty_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        scaffold(dir.path(), "backfill_slugs", &[]).expect("scaffold");
        assert!(load_dir(dir.path()).expect("load")[0].up.is_empty());
    }

    #[test]
    fn specs_on_a_name_that_does_not_read_them_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["backfill_slugs", "drop_users"] {
            let err = scaffold(dir.path(), name, &specs(&["a:text"])).expect_err("refused");
            assert!(err.to_string().contains("a:text"), "{err}");
        }
        assert!(fs::read_dir(dir.path()).expect("read dir").next().is_none());
    }

    #[test]
    fn bad_specs_are_refused_naming_the_spec() {
        let dir = tempfile::tempdir().expect("tempdir");
        for spec in ["", ":text", "a:bogus", "a:text:wat"] {
            let err = scaffold(dir.path(), "create_t", &specs(&[spec])).expect_err("refused");
            assert!(matches!(err, MigrateError::Parse { .. }), "{spec}");
        }
        let err = scaffold(dir.path(), "create_t", &[]).expect_err("no columns");
        assert!(matches!(err, MigrateError::Parse { .. }));
    }

    #[test]
    fn a_name_that_cannot_be_a_file_name_is_refused_before_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = scaffold(dir.path(), "Create-Users", &[]).expect_err("refused");
        assert!(matches!(err, MigrateError::Parse { .. }));
        assert!(fs::read_dir(dir.path()).expect("read dir").next().is_none());
    }

    #[test]
    fn two_scaffolds_in_one_second_get_distinct_versions() {
        let dir = tempfile::tempdir().expect("tempdir");
        scaffold(dir.path(), "first", &[]).expect("first");
        scaffold(dir.path(), "second", &[]).expect("second");
        assert_eq!(load_dir(dir.path()).expect("load").len(), 2);
    }

    #[test]
    fn the_timestamp_is_a_14_digit_utc_date() {
        let stamp = utc_timestamp();
        assert!(
            (20_260_000_000_000..30_000_000_000_000).contains(&stamp),
            "{stamp}"
        );
    }

    const USERS: &str = "CREATE TABLE users (id INTEGER PRIMARY KEY, \
        email TEXT COLLATE NOCASE NOT NULL UNIQUE DEFAULT 'a@b.c' CHECK (length(email) > 3), \
        team_id INTEGER REFERENCES teams (id) ON DELETE SET NULL ON UPDATE CASCADE, \
        score REAL DEFAULT (1 + 1), \
        slug TEXT GENERATED ALWAYS AS (lower(email)) STORED, \
        delta INTEGER DEFAULT -1, \
        expo REAL DEFAULT 1.5e-3, \
        raw BLOB DEFAULT X'00ff')";

    #[test]
    fn remove_copies_the_columns_constraints_exactly() {
        let dir = schema_dir(&[("table", "users", USERS)]);
        let path = scaffold(dir.path(), "remove_email_from_users", &[]).expect("scaffold");
        let Op::DropColumn { table, column } = only_op(dir.path(), &path) else {
            panic!("expected drop_column");
        };
        assert_eq!(table, "users");
        assert_eq!(
            column,
            Column {
                name: "email".into(),
                type_name: "TEXT".into(),
                not_null: true,
                primary_key: None,
                unique: true,
                default: Some("'a@b.c'".into()),
                collation: Some("NOCASE".into()),
                check: Some("length(email) > 3".into()),
                references: None,
                generated: None,
            }
        );
    }

    fn column_of(name: &str) -> Column {
        let dir = schema_dir(&[("table", "users", USERS)]);
        let spec = format!("remove_{name}_from_users");
        let path = scaffold(dir.path(), &spec, &[]).expect("scaffold");
        let Op::DropColumn { column, .. } = only_op(dir.path(), &path) else {
            panic!("expected drop_column");
        };
        column
    }

    #[test]
    fn remove_reads_references_defaults_and_generated_columns() {
        let team = column_of("team_id");
        assert_eq!(
            team.references,
            Some(ForeignKey {
                table: "teams".into(),
                column: "id".into(),
                on_update: Some("CASCADE".into()),
                on_delete: Some("SET NULL".into()),
            })
        );
        assert_eq!(column_of("score").default.as_deref(), Some("1 + 1"));
        assert_eq!(column_of("delta").default.as_deref(), Some("-1"));
        assert_eq!(column_of("expo").default.as_deref(), Some("1.5e-3"));
        assert_eq!(column_of("raw").default.as_deref(), Some("X'00ff'"));
        assert_eq!(
            column_of("slug").generated,
            Some(GeneratedColumn {
                expr: "lower(email)".into(),
                stored: true
            })
        );
        assert_eq!(column_of("id").primary_key, Some(1));
    }

    #[test]
    fn a_column_with_a_clause_the_model_cannot_hold_is_refused() {
        for body in [
            "x TEXT DEFERRABLE INITIALLY DEFERRED",
            "x TEXT NOT NULL ON CONFLICT REPLACE",
            "x TEXT UNIQUE ON CONFLICT IGNORE",
            "x TEXT NOT DEFERRABLE",
        ] {
            let sql = format!("CREATE TABLE t (id INTEGER, {body})");
            let dir = schema_dir(&[("table", "t", &sql)]);
            let err = scaffold(dir.path(), "remove_x_from_t", &[]).expect_err(body);
            assert!(err.to_string().contains("table t"), "{body}: {err}");
        }
    }

    #[test]
    fn drop_table_copies_the_whole_definition() {
        let sql = "CREATE TABLE links (a TEXT, b TEXT NOT NULL, PRIMARY KEY (b, a)) WITHOUT ROWID";
        let dir = schema_dir(&[("table", "links", sql)]);
        let path = scaffold(dir.path(), "drop_links", &[]).expect("scaffold");
        let Op::DropTable { table, definition } = only_op(dir.path(), &path) else {
            panic!("expected drop_table");
        };
        assert_eq!(table, "links");
        assert_eq!(definition.sql, sql);
        assert!(definition.without_rowid);
        let keys: Vec<_> = definition.columns.iter().map(|c| c.primary_key).collect();
        assert_eq!(keys, [Some(2), Some(1)]);
        assert!(definition.columns[1].not_null);
    }

    #[test]
    fn drop_index_copies_the_definition() {
        let dir = schema_dir(&[
            ("table", "users", USERS),
            (
                "index",
                "users_email",
                "CREATE UNIQUE INDEX users_email ON users (email, \"id\") WHERE id > 0",
            ),
        ]);
        let path = scaffold(dir.path(), "drop_users_email", &[]).expect("scaffold");
        let Op::DropIndex { definition } = only_op(dir.path(), &path) else {
            panic!("expected drop_index");
        };
        assert_eq!(
            definition,
            IndexDef {
                name: "users_email".into(),
                table: "users".into(),
                columns: vec!["email".into(), "id".into()],
                unique: true,
                where_clause: Some("id > 0".into()),
            }
        );
    }

    #[test]
    fn an_index_on_an_expression_is_refused_rather_than_copied_wrong() {
        let dir = schema_dir(&[(
            "index",
            "lower_email",
            "CREATE INDEX lower_email ON users (lower(email))",
        )]);
        let err = scaffold(dir.path(), "drop_lower_email", &[]).expect_err("refused");
        assert!(err.to_string().contains("lower_email"), "{err}");
    }

    #[test]
    fn an_object_absent_from_schema_json_fails_naming_it() {
        let dir = schema_dir(&[("table", "users", USERS)]);
        for (name, missing) in [
            ("drop_ghosts", "ghosts"),
            ("remove_email_from_ghosts", "ghosts"),
            ("remove_nope_from_users", "nope"),
        ] {
            let err = scaffold(dir.path(), name, &[]).expect_err("refused");
            assert!(
                matches!(&err, MigrateError::UnknownObject { name, .. } if name.contains(missing)),
                "{err}"
            );
        }
        let none = tempfile::tempdir().expect("tempdir");
        let err = scaffold(none.path(), "drop_users", &[]).expect_err("no schema.json");
        assert!(matches!(err, MigrateError::UnknownObject { .. }));
    }
}
