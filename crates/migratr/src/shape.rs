//! Two schemas compared by structure: each table's columns, foreign keys and indexes as SQLite's
//! pragmas give them, its CHECK and UNIQUE items from its CREATE text, and its trigger and view
//! text, each normalized for identifier quotes, case outside literals and whitespace. Text written
//! by hand and text written by migratr for the same schema compare equal; a real difference is
//! named.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::executor::{SchemaObject, SchemaSnapshot};
use crate::ledger::LEDGER;
use crate::reader::{SqlReader, read_schema_through};
use crate::sql_ddl::{quote_ident, table_body};

/// D1's own table, which is not the place's.
const D1_TABLE: &str = "_cf_KV";

/// Each way `place` differs by structure from `made`, the schema the migrations make, as one
/// line naming the table (or view) and the difference. Empty when they are the same.
pub(crate) fn differences_through<R: SqlReader>(
    place: &mut R,
    made: &mut R,
) -> Result<Vec<String>, R::Error> {
    let place = Shape::read(place)?;
    let made = Shape::read(made)?;
    Ok(compare(&place, &made))
}

/// A column as `table_xinfo` gives it, normalized.
#[derive(Debug, PartialEq, Eq)]
struct Column {
    decl_type: String,
    not_null: bool,
    default: Option<String>,
    pk: i64,
    hidden: i64,
}

#[derive(Debug, Default)]
struct Table {
    columns: BTreeMap<String, Column>,
    /// Foreign keys, indexes, and CHECK and UNIQUE items, each as one normalized line.
    items: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct Shape {
    tables: BTreeMap<String, Table>,
    /// Each trigger by name: its table and its normalized text.
    triggers: BTreeMap<String, (String, String)>,
    views: BTreeMap<String, String>,
}

fn skipped(table: &str) -> bool {
    let lower = table.to_ascii_lowercase();
    lower.starts_with("sqlite_") || lower == LEDGER || table == D1_TABLE
}

impl Shape {
    fn read<R: SqlReader>(reader: &mut R) -> Result<Self, R::Error> {
        let schema = read_schema_through(reader, skipped)?;
        let mut shape = Shape::default();
        for info in &schema.tables {
            let mut table = Table::default();
            for c in info.columns.iter() {
                table.columns.insert(
                    c.name.to_ascii_lowercase(),
                    Column {
                        decl_type: normalize(&c.decl_type),
                        not_null: c.not_null,
                        default: c.default_value.as_deref().map(normalize),
                        pk: c.pk,
                        hidden: c.hidden,
                    },
                );
            }
            table.items.extend(foreign_keys(&schema, &info.name));
            table.items.extend(indexes(reader, &info.name)?);
            if let Some(sql) = object(&schema, "table", &info.name).and_then(|o| o.sql.as_deref()) {
                table.items.extend(check_and_unique(sql));
            }
            shape.tables.insert(info.name.to_ascii_lowercase(), table);
        }
        for o in &schema.objects {
            let Some(sql) = &o.sql else { continue };
            match o.kind.as_str() {
                "trigger" => {
                    shape.triggers.insert(
                        o.name.to_ascii_lowercase(),
                        (o.tbl_name.to_ascii_lowercase(), normalize(sql)),
                    );
                }
                "view" => {
                    shape
                        .views
                        .insert(o.name.to_ascii_lowercase(), normalize(sql));
                }
                _ => {}
            }
        }
        Ok(shape)
    }
}

fn object<'a>(schema: &'a SchemaSnapshot, kind: &str, name: &str) -> Option<&'a SchemaObject> {
    schema
        .objects
        .iter()
        .find(|o| o.kind == kind && o.name.eq_ignore_ascii_case(name))
}

/// The table's foreign keys, one line each. A key that names no parent column refers to the
/// parent's primary key, and is written with it.
fn foreign_keys(schema: &SchemaSnapshot, table: &str) -> Vec<String> {
    let Some(info) = schema.tables.iter().find(|t| t.name == table) else {
        return Vec::new();
    };
    let mut keys: BTreeMap<i64, Vec<&crate::executor::ForeignKeyInfo>> = BTreeMap::new();
    for key in &info.foreign_keys {
        keys.entry(key.id).or_default().push(key);
    }
    keys.values()
        .map(|parts| {
            let first = parts[0];
            let parent = first.table.to_ascii_lowercase();
            let from: Vec<String> = parts.iter().map(|p| p.from.to_ascii_lowercase()).collect();
            let named: Option<Vec<String>> = parts
                .iter()
                .map(|p| p.to.as_ref().map(|t| t.to_ascii_lowercase()))
                .collect();
            let to = named.unwrap_or_else(|| primary_key(schema, &parent));
            format!(
                "foreign key ({}) references {parent}({}) on update {} on delete {}",
                from.join(","),
                to.join(","),
                first.on_update.to_ascii_lowercase(),
                first.on_delete.to_ascii_lowercase(),
            )
        })
        .collect()
}

fn primary_key(schema: &SchemaSnapshot, table: &str) -> Vec<String> {
    let Some(info) = schema
        .tables
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(table))
    else {
        return Vec::new();
    };
    let mut pk: Vec<_> = info.columns.iter().filter(|c| c.pk > 0).collect();
    pk.sort_by_key(|c| c.pk);
    pk.iter().map(|c| c.name.to_ascii_lowercase()).collect()
}

/// The table's indexes from `index_list` and `index_xinfo`, one line each. An index made by a
/// UNIQUE item or the primary key is known by where it comes from, not by SQLite's name for it.
fn indexes<R: SqlReader>(reader: &mut R, table: &str) -> Result<Vec<String>, R::Error> {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let mut out = Vec::new();
    for row in reader.rows(&format!("PRAGMA index_list({})", quote_ident(table)))? {
        let name = row.get(1).map(text).unwrap_or_default();
        let unique = row.get(2).and_then(Value::as_i64) == Some(1);
        let origin = row.get(3).map(text).unwrap_or_default();
        let partial = row.get(4).and_then(Value::as_i64) == Some(1);
        let mut columns = Vec::new();
        for key in reader.rows(&format!("PRAGMA index_xinfo({})", quote_ident(&name)))? {
            if key.get(5).and_then(Value::as_i64) != Some(1) {
                continue;
            }
            let mut column = match key.get(2).and_then(Value::as_str) {
                Some(c) => c.to_ascii_lowercase(),
                None if key.get(1).and_then(Value::as_i64) == Some(-1) => "rowid".to_string(),
                None => "<expression>".to_string(),
            };
            if key.get(3).and_then(Value::as_i64) == Some(1) {
                column.push_str(" desc");
            }
            let collation = key.get(4).map(text).unwrap_or_default();
            if !collation.eq_ignore_ascii_case("binary") {
                column.push_str(&format!(" collate {}", collation.to_ascii_lowercase()));
            }
            columns.push(column);
        }
        let columns = columns.join(",");
        let unique = if unique { "unique " } else { "" };
        let partial = if partial { " (partial)" } else { "" };
        out.push(match origin.as_str() {
            "u" => format!("unique index on ({columns}) from a UNIQUE item"),
            "pk" => format!("primary key index on ({columns})"),
            _ => format!(
                "{unique}index {} on ({columns}){partial}",
                name.to_ascii_lowercase()
            ),
        });
    }
    Ok(out)
}

/// The table's CHECK and UNIQUE items, read from its CREATE text split at top-level commas: each
/// table constraint that is a CHECK or a UNIQUE, and each CHECK written on a column.
fn check_and_unique(create_sql: &str) -> Vec<String> {
    let Some(body) = table_body(create_sql) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for range in body.items {
        let item = normalize(&create_sql[range]);
        let head = match item.strip_prefix("constraint ") {
            Some(rest) => rest.split_once(' ').map_or(rest, |(_, r)| r),
            None => item.as_str(),
        };
        if head.starts_with("check") {
            out.push(format!("CHECK {item}"));
        } else if head.starts_with("unique") {
            out.push(format!("UNIQUE {item}"));
        } else {
            out.extend(
                column_checks(&item)
                    .into_iter()
                    .map(|c| format!("CHECK {c}")),
            );
        }
    }
    out
}

/// Each `check(...)` in a normalized column definition.
fn column_checks(item: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = item[from..].find("check(").map(|i| i + from) {
        let starts_word = item[..at]
            .chars()
            .last()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in item[at..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(at + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        if starts_word {
            out.push(item[at..end].to_string());
        }
        from = end;
    }
    out
}

/// `sql` with identifier quotes removed, case folded outside string literals, comments dropped,
/// runs of whitespace made one space, and no space beside `(`, `)` or `,`.
pub(crate) fn normalize(sql: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    let push = |out: &mut String, space: &mut bool, text: &str| {
        let next = text.chars().next();
        if *space
            && !out.is_empty()
            && !out.ends_with(['(', ',', ' '])
            && !matches!(next, Some('(' | ')' | ','))
        {
            out.push(' ');
        }
        *space = false;
        out.push_str(text);
    };
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => space = true,
            '\'' => {
                let mut literal = String::from('\'');
                while let Some(n) = chars.next() {
                    literal.push(n);
                    if n == '\'' {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            literal.push('\'');
                            continue;
                        }
                        break;
                    }
                }
                push(&mut out, &mut space, &literal);
            }
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let mut ident = String::new();
                while let Some(n) = chars.next() {
                    if n == close {
                        if c != '[' && chars.peek() == Some(&close) {
                            chars.next();
                            ident.push(close);
                            continue;
                        }
                        break;
                    }
                    ident.push(n.to_ascii_lowercase());
                }
                push(&mut out, &mut space, &ident);
            }
            '-' if chars.peek() == Some(&'-') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                space = true;
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                space = true;
            }
            c => {
                let lower = c.to_ascii_lowercase();
                push(&mut out, &mut space, lower.encode_utf8(&mut [0; 4]));
            }
        }
    }
    out
}

fn compare(place: &Shape, made: &Shape) -> Vec<String> {
    let mut out = Vec::new();
    for name in keys(&place.tables, &made.tables) {
        match (place.tables.get(name), made.tables.get(name)) {
            (None, Some(_)) => out.push(format!("table {name}: the place lacks it")),
            (Some(_), None) => out.push(format!(
                "table {name}: the place has it, and the migrations do not make it"
            )),
            (Some(p), Some(m)) => compare_table(name, p, m, &mut out),
            (None, None) => {}
        }
    }
    for name in keys(&place.triggers, &made.triggers) {
        match (place.triggers.get(name), made.triggers.get(name)) {
            (None, Some((table, _))) => {
                out.push(format!("table {table}: the place lacks trigger {name}"));
            }
            (Some((table, _)), None) => out.push(format!(
                "table {table}: the place has trigger {name}, which the migrations do not make"
            )),
            (Some((table, p)), Some((_, m))) if p != m => out.push(format!(
                "table {table}: trigger {name} is not the one the migrations make"
            )),
            _ => {}
        }
    }
    for name in keys(&place.views, &made.views) {
        match (place.views.get(name), made.views.get(name)) {
            (None, Some(_)) => out.push(format!("view {name}: the place lacks it")),
            (Some(_), None) => out.push(format!(
                "view {name}: the place has it, and the migrations do not make it"
            )),
            (Some(p), Some(m)) if p != m => out.push(format!(
                "view {name}: it is not the one the migrations make"
            )),
            _ => {}
        }
    }
    out
}

fn keys<'a, V>(a: &'a BTreeMap<String, V>, b: &'a BTreeMap<String, V>) -> BTreeSet<&'a String> {
    a.keys().chain(b.keys()).collect()
}

fn compare_table(name: &str, place: &Table, made: &Table, out: &mut Vec<String>) {
    for column in keys(&place.columns, &made.columns) {
        match (place.columns.get(column), made.columns.get(column)) {
            (None, Some(_)) => out.push(format!("table {name}: the place lacks column {column}")),
            (Some(_), None) => out.push(format!(
                "table {name}: the place has column {column}, which the migrations do not make"
            )),
            (Some(p), Some(m)) => {
                let mut differs = |what: &str, p: String, m: String| {
                    if p != m {
                        out.push(format!(
                            "table {name}: column {column} has {what} {p} in the place and {m} \
                             from the migrations"
                        ));
                    }
                };
                let none = || "none".to_string();
                differs("type", p.decl_type.clone(), m.decl_type.clone());
                differs("not null", p.not_null.to_string(), m.not_null.to_string());
                differs(
                    "default",
                    p.default.clone().unwrap_or_else(none),
                    m.default.clone().unwrap_or_else(none),
                );
                differs("primary key position", p.pk.to_string(), m.pk.to_string());
                differs("hidden", p.hidden.to_string(), m.hidden.to_string());
            }
            (None, None) => {}
        }
    }
    for item in made.items.difference(&place.items) {
        out.push(format!("table {name}: the place lacks {item}"));
    }
    for item in place.items.difference(&made.items) {
        out.push(format!(
            "table {name}: the place has {item}, which the migrations do not make"
        ));
    }
}

#[cfg(all(test, feature = "rusqlite"))]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::{RusqliteExecutor, load_dir, up};

    /// org, member and the org_name index, as migratr writes them.
    const MIGRATION: &str = r#"{"up": [
      {"op": "create_table", "table": "org", "columns": [
        {"name": "id", "type": "TEXT", "primary_key": 1},
        {"name": "name", "type": "TEXT", "not_null": true}]},
      {"op": "create_index", "name": "org_name", "table": "org", "columns": ["name"]},
      {"op": "create_table", "table": "member", "columns": [
        {"name": "id", "type": "TEXT", "primary_key": 1},
        {"name": "org", "type": "TEXT", "not_null": true,
         "references": {"table": "org", "column": "id", "on_delete": "CASCADE"}},
        {"name": "email", "type": "TEXT", "not_null": true},
        {"name": "role", "type": "TEXT", "not_null": true, "default": "'member'"},
        {"name": "qty", "type": "INTEGER", "not_null": true, "default": "0"}],
       "checks": ["qty >= 0"], "unique": [["org", "email"]]},
      {"op": "create_trigger", "name": "member_stamp", "table": "member",
       "sql": "CREATE TRIGGER member_stamp AFTER INSERT ON member BEGIN UPDATE org SET name = name WHERE id = new.org; END"}
    ]}"#;

    /// The same schema written by hand: other quoting, case and whitespace.
    const HAND: &str = "
      create table org (id text primary key, name text not null);
      create index org_name on org(name);
      CREATE TABLE member (
        id   text PRIMARY KEY,
        org  text NOT NULL REFERENCES org ON DELETE cascade,
        email TEXT not null,
        role text NOT NULL DEFAULT 'member',
        qty integer not null default 0,
        check (qty >= 0),
        UNIQUE (org, email)
      );
      create trigger member_stamp after insert on member
        begin update org set name = name where id = new.org; end;";

    fn made() -> RusqliteExecutor {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("20260101000001_init.json"), MIGRATION).expect("write");
        let migrations = load_dir(dir.path()).expect("load");
        let mut exec = RusqliteExecutor::new(Connection::open_in_memory().expect("open"));
        up(&mut exec, &migrations, None).expect("up");
        exec
    }

    fn place(sql: &str) -> RusqliteExecutor {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(sql).expect("place");
        RusqliteExecutor::new(conn)
    }

    fn master_text(exec: &RusqliteExecutor) -> Vec<String> {
        let conn = exec.connection();
        let mut statement = conn
            .prepare(
                "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL \
                 AND name NOT LIKE '_migratr%' ORDER BY name",
            )
            .expect("prepare");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .map(|s| {
                s.expect("sql")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    #[test]
    fn hand_sql_and_migratr_sql_differ_as_text_and_are_equal_by_structure() {
        let made = made();
        let hand = place(HAND);
        assert_ne!(master_text(&hand), master_text(&made));
        assert_eq!(
            hand.shape_differences(&made).expect("compare"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn check_and_unique_items_come_from_the_create_text_split_at_top_level_commas() {
        let items = check_and_unique(
            "CREATE TABLE t (a INTEGER CHECK (a IN (1, 2)), org TEXT, email TEXT, \
             CHECK (\"qty\" >= 0), CONSTRAINT u UNIQUE (\"org\", email), PRIMARY KEY (a))",
        );
        assert_eq!(
            items,
            vec![
                "CHECK check(a in(1,2))",
                "CHECK check(qty >= 0)",
                "UNIQUE constraint u unique(org,email)",
            ]
        );
    }

    fn differences(sql: &str) -> Vec<String> {
        place(sql).shape_differences(&made()).expect("compare")
    }

    #[test]
    fn a_missing_table_check_and_unique_are_named() {
        let sql = HAND
            .replace(",\n        UNIQUE (org, email)", "")
            .replace(",\n        check (qty >= 0)", "");
        assert_eq!(
            differences(&sql),
            vec![
                "table member: the place lacks CHECK check(qty >= 0)",
                "table member: the place lacks UNIQUE unique(org,email)",
                "table member: the place lacks unique index on (org,email) from a UNIQUE item",
            ]
        );
    }

    #[test]
    fn a_changed_default_is_named() {
        let sql = HAND.replace("DEFAULT 'member'", "DEFAULT 'owner'");
        assert_eq!(
            differences(&sql),
            vec![
                "table member: column role has default 'owner' in the place and 'member' from \
                 the migrations"
            ]
        );
    }

    #[test]
    fn a_unique_item_against_a_unique_index_is_named() {
        let sql = HAND.replace(",\n        UNIQUE (org, email)", "")
            + "create unique index member_org_email on member (org, email);";
        assert_eq!(
            differences(&sql),
            vec![
                "table member: the place lacks UNIQUE unique(org,email)",
                "table member: the place lacks unique index on (org,email) from a UNIQUE item",
                "table member: the place has unique index member_org_email on (org,email), \
                 which the migrations do not make",
            ]
        );
    }

    #[test]
    fn a_missing_trigger_is_named() {
        let at = HAND.find("create trigger").expect("trigger");
        assert_eq!(
            differences(&HAND[..at]),
            vec!["table member: the place lacks trigger member_stamp"]
        );
    }

    #[test]
    fn normalize_folds_quotes_case_and_whitespace_but_not_literals() {
        assert_eq!(
            normalize("CHECK (\"Qty\"  >= 0 AND `x` = [Y] AND z = 'Ab''c')"),
            "check(qty >= 0 and x = y and z = 'Ab''c')"
        );
    }
}
