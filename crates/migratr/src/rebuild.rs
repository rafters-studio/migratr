//! SQLite's table rebuild: the procedure for schema changes ALTER TABLE cannot make in place
//! (sqlite.org/lang_altertable.html, section 7).
//!
//! The new table's CREATE statement is the stored one with the change edited into its text,
//! so everything else the author declared (types or their absence, collations, CHECK and
//! foreign-key clauses, generated columns, AUTOINCREMENT, `WITHOUT ROWID`) carries over
//! unchanged. The statements run inside the migration's transaction with foreign_keys left as
//! they are and no TEMP object, so they run the same on a local file and on a place, such as
//! D1, that refuses TEMP objects and cannot turn foreign keys off.

use crate::executor::{SchemaObject, SchemaSnapshot, TableInfo};
use crate::migration::{Column, MigrateError, Migration, Op};
use crate::sql_ddl::{
    TableBody, TokenKind, create_table_name_span, has_keyword, index_names_column,
    is_foreign_key_item, leading_identifier, mentions_identifier, names_own_column, quote_ident,
    quote_literal, references_clause, rename_references, table_body, tokens,
};

/// A change to one table that the rebuild makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TableChange {
    DropColumn {
        column: String,
    },
    /// Adds a column, as the last column item of the stored CREATE statement. `definition` is
    /// the column's SQL; existing rows get its default, or NULL.
    AddColumn {
        definition: String,
    },
    /// Adds a table constraint, as the last item of the stored CREATE statement.
    AddConstraint {
        definition: String,
    },
    /// Removes the foreign key over `columns` that references `parent`: a table-level
    /// `FOREIGN KEY` item, or the `REFERENCES` clause of the one column's definition.
    DropForeignKey {
        columns: Vec<String>,
        parent: String,
    },
}

/// Whether SQLite's in-place ADD COLUMN refuses `column`: it cannot add a UNIQUE or PRIMARY
/// KEY column or a STORED generated one.
pub(crate) fn add_needs_rebuild(column: &Column) -> bool {
    column.unique
        || column.primary_key.is_some()
        || column.generated.as_ref().is_some_and(|g| g.stored)
}

/// A table's stored CREATE statement, split and checked against its columns.
struct ParsedTable<'a> {
    object: &'a SchemaObject,
    sql: &'a str,
    info: &'a TableInfo,
    body: TableBody,
}

impl ParsedTable<'_> {
    fn item(&self, index: usize) -> &str {
        &self.sql[self.body.items[index].clone()]
    }

    fn column_index(&self, column: &str) -> Option<usize> {
        self.info
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(column))
    }

    /// The first item other than column `index`'s own that names `column` as a column of this
    /// table: another column's expressions or a table constraint.
    fn item_naming(&self, index: usize, column: &str) -> Option<&str> {
        (0..self.body.items.len())
            .filter(|&i| i != index)
            .find(|&i| names_own_column(self.item(i), column))
            .map(|i| self.item(i))
    }

    /// The CREATE statement with `definition` as a new item after item `index`.
    fn with_item_after(&self, index: usize, definition: &str) -> String {
        let end = self.body.items[index].end;
        format!("{}, {}{}", &self.sql[..end], definition, &self.sql[end..])
    }

    /// The CREATE statement without item `index`. The item goes with the comma after it,
    /// or before it when it is the last item.
    fn without_item(&self, index: usize) -> String {
        let items = &self.body.items;
        let removed = match items.get(index + 1) {
            Some(next) => items[index].start..next.start,
            None if index > 0 => items[index - 1].end..items[index].end,
            None => items[index].clone(),
        };
        format!("{}{}", &self.sql[..removed.start], &self.sql[removed.end..])
    }

    /// The CREATE statement without the foreign key over `columns` that references `parent`,
    /// or `None` when the table has no such key.
    fn without_foreign_key(&self, columns: &[String], parent: &str) -> Option<String> {
        let constraints = self.info.columns.len()..self.body.items.len();
        if let Some(index) = constraints
            .into_iter()
            .find(|&i| is_foreign_key_item(self.item(i), columns, parent))
        {
            return Some(self.without_item(index));
        }
        let [column] = columns else {
            return None;
        };
        let item = &self.body.items[self.column_index(column)?];
        let clause = references_clause(&self.sql[item.clone()], parent)?;
        Some(format!(
            "{}{}",
            &self.sql[..item.start + clause.start],
            &self.sql[item.start + clause.end..]
        ))
    }
}

/// Finds `table` in `schema` and splits its CREATE statement. Refused with
/// `UnparseableTable` when the table or its statement is absent, the statement does not
/// split, or its leading items are not the table's columns in declaration order.
fn parse<'a>(schema: &'a SchemaSnapshot, table: &str) -> Result<ParsedTable<'a>, MigrateError> {
    let unparseable = || MigrateError::UnparseableTable {
        table: table.to_string(),
    };
    let object = schema
        .objects
        .iter()
        .find(|o| o.kind == "table" && o.name.eq_ignore_ascii_case(table))
        .ok_or_else(unparseable)?;
    let sql = object.sql.as_deref().ok_or_else(unparseable)?;
    let info = schema
        .tables
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(table))
        .ok_or_else(unparseable)?;
    let body = table_body(sql).ok_or_else(unparseable)?;
    // Column definitions precede table constraints, one item per column in declaration order.
    let columns_match = body.items.len() >= info.columns.len()
        && info.columns.iter().zip(&body.items).all(|(column, item)| {
            leading_identifier(&sql[item.clone()])
                .is_some_and(|n| n.eq_ignore_ascii_case(&column.name))
        });
    if !columns_match {
        return Err(unparseable());
    }
    Ok(ParsedTable {
        object,
        sql,
        info,
        body,
    })
}

/// Whether dropping `column` from `table` needs the rebuild. SQLite's in-place DROP COLUMN
/// refuses a column that is part of the primary key, UNIQUE, a foreign key, indexed, or
/// named by another column (a generated expression or CHECK) or a table constraint. False
/// when the table or column is absent, so the in-place statement reports it. Indexes named in
/// `dropped_indexes` no longer exist when the column is dropped, so they do not count.
fn needs_rebuild(
    schema: &SchemaSnapshot,
    table: &str,
    column: &str,
    dropped_indexes: &[&str],
) -> Result<bool, MigrateError> {
    let present = schema
        .tables
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(table))
        .is_some_and(|t| {
            t.columns
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(column))
        });
    if !present {
        return Ok(false);
    }
    let parsed = parse(schema, table)?;
    let Some(index) = parsed.column_index(column) else {
        return Ok(false);
    };
    let own = parsed.item(index);
    let constrained = parsed.info.columns[index].pk > 0
        || ["PRIMARY", "UNIQUE", "REFERENCES"]
            .iter()
            .any(|k| has_keyword(own, k));
    let named_elsewhere = parsed.item_naming(index, column).is_some();
    let indexed = Dependents::of(schema, &[table])
        .indexes
        .iter()
        .filter(|o| {
            !dropped_indexes
                .iter()
                .any(|d| d.eq_ignore_ascii_case(&o.name))
        })
        .any(|o| index_names(o, column));
    Ok(constrained || named_elsewhere || indexed)
}

/// The tables `migration` would rebuild on a database whose schema is `schema`, read through
/// [`Executor::read_schema`](crate::Executor::read_schema), each named once in operation order.
/// Empty when it needs no rebuild. Decides as `up` does, and so `up` refuses a migration whose
/// rebuild is not its first operation even though its table is reported here. Refused with
/// `UnparseableTable` when a dropped column's table has a CREATE statement that does not split.
pub fn rebuild_tables(
    schema: &SchemaSnapshot,
    migration: &Migration,
) -> Result<Vec<String>, MigrateError> {
    let mut tables: Vec<String> = Vec::new();
    for i in 0..migration.up.len() {
        if let Some((table, _)) = op_rebuild(schema, &migration.up, i)?
            && !tables.iter().any(|t| t.eq_ignore_ascii_case(table))
        {
            tables.push(table.clone());
        }
    }
    Ok(tables)
}

/// The table and columns of operation `i` of `ops` when that operation needs the rebuild on a
/// database whose schema before the migration is `schema`: the one decision `up` and
/// [`rebuild_tables`] share. An earlier `DropIndex` has removed its index by the time the
/// column goes. A foreign key is always added or dropped by the rebuild.
pub(crate) fn op_rebuild<'a>(
    schema: &SchemaSnapshot,
    ops: &'a [Op],
    i: usize,
) -> Result<Option<(&'a String, String)>, MigrateError> {
    let (table, column, rebuilds) = match &ops[i] {
        Op::AddForeignKey(key) | Op::DropForeignKey { definition: key } => {
            return Ok(Some((&key.table, key.columns.join(", "))));
        }
        Op::AddColumn { table, column } => (table, column, add_needs_rebuild(column)),
        Op::DropColumn { table, column } => {
            let dropped_indexes: Vec<&str> = ops[..i]
                .iter()
                .filter_map(|o| match o {
                    Op::DropIndex { definition } => Some(definition.name.as_str()),
                    _ => None,
                })
                .collect();
            let rebuilds = needs_rebuild(schema, table, &column.name, &dropped_indexes)?;
            (table, column, rebuilds)
        }
        _ => return Ok(None),
    };
    Ok(rebuilds.then(|| (table, column.name.clone())))
}

fn index_names(object: &SchemaObject, column: &str) -> bool {
    object
        .sql
        .as_deref()
        .is_some_and(|sql| index_names_column(sql, column))
}

fn mentions(object: &SchemaObject, name: &str) -> bool {
    object
        .sql
        .as_deref()
        .is_some_and(|sql| mentions_identifier(sql, name))
}

/// Whether `object` relies on the columns of `table` by position, so dropping any column
/// breaks it without naming that column: an `INSERT INTO table` with no column list, or a
/// view that declares its own column list over a `*`.
fn uses_columns_by_position(object: &SchemaObject, table: &str) -> bool {
    let Some(sql) = object.sql.as_deref() else {
        return false;
    };
    let toks = tokens(sql);
    let punct = |i: usize, p: &str| {
        toks.get(i)
            .is_some_and(|t| t.kind == TokenKind::Punct && t.text == p)
    };
    let word = |i: usize, w: &str| {
        toks.get(i)
            .is_some_and(|t| t.kind == TokenKind::Bare && t.text.eq_ignore_ascii_case(w))
    };
    // Skips a possibly schema-qualified name starting at `i`, returning the index after it
    // and the unqualified name.
    let name_at = |i: usize| -> Option<(usize, &str)> {
        let first = toks.get(i)?;
        if punct(i + 1, ".") {
            let second = toks.get(i + 2)?;
            Some((i + 3, second.text.as_str()))
        } else {
            Some((i + 1, first.text.as_str()))
        }
    };
    let positional_insert = (0..toks.len()).any(|i| {
        word(i, "INTO")
            && name_at(i + 1)
                .is_some_and(|(next, name)| name.eq_ignore_ascii_case(table) && !punct(next, "("))
    });
    let view_over_star = object.kind == "view"
        && sql.contains('*')
        && (0..toks.len()).find(|&i| word(i, "VIEW")).is_some_and(|i| {
            let mut at = i + 1;
            if word(at, "IF") && word(at + 1, "NOT") && word(at + 2, "EXISTS") {
                at += 3;
            }
            name_at(at).is_some_and(|(next, _)| punct(next, "("))
        });
    positional_insert || view_over_star
}

/// The objects a rebuild of some tables drops and recreates.
struct Dependents<'a> {
    /// Explicit indexes on the tables.
    indexes: Vec<&'a SchemaObject>,
    /// Views naming one of the tables, or naming such a view, in sqlite_master order.
    views: Vec<&'a SchemaObject>,
    /// Triggers on one of the tables or views, or whose SQL names any of them, in
    /// sqlite_master order.
    triggers: Vec<&'a SchemaObject>,
}

impl<'a> Dependents<'a> {
    /// Renaming a rebuilt table into place fails while any view or trigger names a table
    /// or view that is missing at that moment, so every one of them is dropped first.
    fn of(schema: &'a SchemaSnapshot, tables: &[&str]) -> Self {
        let mut dropped: Vec<&str> = tables.to_vec();
        loop {
            let more: Vec<&str> = schema
                .objects
                .iter()
                .filter(|o| o.kind == "view")
                .filter(|o| !dropped.iter().any(|d| d.eq_ignore_ascii_case(&o.name)))
                .filter(|o| dropped.iter().any(|d| mentions(o, d)))
                .map(|o| o.name.as_str())
                .collect();
            if more.is_empty() {
                break;
            }
            dropped.extend(more);
        }
        let is_dropped = |name: &str| dropped.iter().any(|d| d.eq_ignore_ascii_case(name));
        let of_kind = |kind: &'static str| schema.objects.iter().filter(move |o| o.kind == kind);
        Dependents {
            indexes: of_kind("index")
                .filter(|o| {
                    o.sql.is_some() && tables.iter().any(|t| o.tbl_name.eq_ignore_ascii_case(t))
                })
                .collect(),
            views: of_kind("view").filter(|o| is_dropped(&o.name)).collect(),
            triggers: of_kind("trigger")
                .filter(|o| is_dropped(&o.tbl_name) || dropped.iter().any(|d| mentions(o, d)))
                .collect(),
        }
    }

    fn all(&self) -> impl Iterator<Item = &&'a SchemaObject> {
        self.indexes.iter().chain(&self.views).chain(&self.triggers)
    }
}

/// `table` and, after it, each other table with a foreign key that references it: the family a
/// rebuild of `table` rebuilds, since SQLite refuses to drop a table that a kept table
/// references while foreign keys are on.
pub(crate) fn family(schema: &SchemaSnapshot, table: &str) -> Vec<String> {
    let name = schema
        .tables
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(table))
        .map_or(table, |t| t.name.as_str());
    std::iter::once(name.to_string())
        .chain(
            schema
                .tables
                .iter()
                .filter(|t| !t.name.eq_ignore_ascii_case(name))
                .filter(|t| {
                    t.foreign_keys
                        .iter()
                        .any(|fk| fk.table.eq_ignore_ascii_case(name))
                })
                .map(|t| t.name.clone()),
        )
        .collect()
}

/// The column list that copies a table's rows into its new table, rowids included; `dropped`
/// is the index of a column that does not go across.
fn copied_columns(parsed: &ParsedTable, dropped: Option<usize>) -> String {
    // Generated columns compute their own values and cannot be inserted into.
    let mut copied: Vec<String> = parsed
        .info
        .columns
        .iter()
        .enumerate()
        .filter(|(i, c)| Some(*i) != dropped && c.hidden == 0)
        .map(|(_, c)| quote_ident(&c.name))
        .collect();
    let without_rowid = has_keyword(&parsed.sql[parsed.body.close + 1..], "ROWID");
    let rowid = ["rowid", "oid", "_rowid_"].into_iter().find(|alias| {
        !parsed
            .info
            .columns
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(alias))
    });
    if let (false, Some(rowid)) = (without_rowid, rowid) {
        copied.insert(0, rowid.to_string());
    }
    copied.join(", ")
}

/// The statements that rebuild `table` with `change` applied, together with each table that
/// references it (its [`family`]), in order:
///
/// 1. drop the triggers and views that name a table of the family, directly or through
///    another view
/// 2. create each new table under a free name, the table's own first, every foreign key to a
///    table of the family naming that table's new name
/// 3. copy the rows, rowids included, the table's own first, and carry each sqlite_sequence
///    value to its new table
/// 4. drop the old referencing tables, then the old table, which drops their indexes and
///    triggers
/// 5. rename the new tables, the table's own first, so SQLite rewrites the references to the
///    final names
/// 6. recreate the indexes, then the views, then the triggers
///
/// Triggers are recreated after the copy, so they never fire on copied rows. No statement
/// creates a TEMP object or sets foreign_keys: with foreign keys on, the copy refuses a row
/// they refuse.
///
/// Refused before any statement with `UnparseableTable` when the stored CREATE statement of a
/// table of the family cannot be split into column definitions that match its columns, or
/// when the table or the changed column is absent from `schema`; and with `ColumnInUse`
/// when another column's definition, a table constraint, a foreign key of any table that
/// names the column as its parent key, or a recreated index, view or trigger names the
/// dropped column. SQLite does not check a view or trigger body when it is created, so the
/// object would break on first use.
pub(crate) fn rebuild_statements(
    schema: &SchemaSnapshot,
    table: &str,
    change: &TableChange,
) -> Result<Vec<String>, MigrateError> {
    let parsed = parse(schema, table)?;
    let table_name = &parsed.object.name;
    let dependents = Dependents::of(schema, &[table_name]);

    let (edited, dropped) = match change {
        TableChange::DropColumn { column } => {
            let dropped =
                parsed
                    .column_index(column)
                    .ok_or_else(|| MigrateError::UnparseableTable {
                        table: table.to_string(),
                    })?;
            let column_in_use = |object: String| MigrateError::ColumnInUse {
                table: table_name.clone(),
                column: column.clone(),
                object,
            };
            if let Some(item) = parsed.item_naming(dropped, column) {
                return Err(column_in_use(
                    item.split_whitespace().collect::<Vec<_>>().join(" "),
                ));
            }
            // A foreign key in any table, this one included, whose parent key is the column:
            // named outright, or implied when the column is the parent's primary key.
            let in_primary_key = parsed.info.columns[dropped].pk > 0;
            let referencing = schema.tables.iter().find(|t| {
                t.foreign_keys.iter().any(|fk| {
                    fk.table.eq_ignore_ascii_case(table_name)
                        && fk
                            .to
                            .as_deref()
                            .map_or(in_primary_key, |to| to.eq_ignore_ascii_case(column))
                        && !(t.name.eq_ignore_ascii_case(table_name)
                            && fk.from.eq_ignore_ascii_case(column))
                })
            });
            if let Some(child) = referencing {
                return Err(column_in_use(child.name.clone()));
            }
            let uses_column = |o: &&&SchemaObject| match o.kind.as_str() {
                "index" => index_names(o, column),
                _ => mentions(o, column) || uses_columns_by_position(o, table_name),
            };
            if let Some(user) = dependents.all().find(uses_column) {
                return Err(column_in_use(user.name.clone()));
            }
            (parsed.without_item(dropped), Some(dropped))
        }
        TableChange::AddColumn { definition } => (
            parsed.with_item_after(parsed.info.columns.len() - 1, definition),
            None,
        ),
        TableChange::AddConstraint { definition } => (
            parsed.with_item_after(parsed.body.items.len() - 1, definition),
            None,
        ),
        TableChange::DropForeignKey { columns, parent } => (
            parsed.without_foreign_key(columns, parent).ok_or_else(|| {
                MigrateError::NoSuchForeignKey {
                    table: table_name.clone(),
                    columns: columns.clone(),
                    parent: parent.clone(),
                }
            })?,
            None,
        ),
    };

    let family_names = family(schema, table_name);
    let children = family_names[1..]
        .iter()
        .map(|child| parse(schema, child))
        .collect::<Result<Vec<_>, _>>()?;

    // Each table of the family: its stored CREATE statement as edited, and its copied columns.
    let mut members: Vec<(&ParsedTable, String, String)> =
        vec![(&parsed, edited, copied_columns(&parsed, dropped))];
    members.extend(
        children
            .iter()
            .map(|c| (c, c.sql.to_string(), copied_columns(c, None))),
    );

    let mut taken: Vec<String> = schema.objects.iter().map(|o| o.name.clone()).collect();
    let mut new_names: Vec<String> = Vec::with_capacity(members.len());
    for _ in &members {
        let refs: Vec<&str> = taken.iter().map(String::as_str).collect();
        let name = free_name(&refs, "_migratr_rebuild");
        taken.push(name.clone());
        new_names.push(name);
    }
    let renames: Vec<(&str, &str)> = members
        .iter()
        .zip(&new_names)
        .map(|((m, _, _), new)| (m.object.name.as_str(), new.as_str()))
        .collect();

    let mut creates = Vec::with_capacity(members.len());
    for ((member, sql, _), new_name) in members.iter().zip(&new_names) {
        let name_span =
            create_table_name_span(member.sql).ok_or_else(|| MigrateError::UnparseableTable {
                table: member.object.name.clone(),
            })?;
        let renamed = format!(
            "{}{}{}",
            &sql[..name_span.start],
            quote_ident(new_name),
            &sql[name_span.end..],
        );
        creates.push(rename_references(&renamed, &renames));
    }

    let family_refs: Vec<&str> = family_names.iter().map(String::as_str).collect();
    let family_dependents = Dependents::of(schema, &family_refs);
    let in_family = |name: &str| family_refs.iter().any(|f| f.eq_ignore_ascii_case(name));
    let survives_drops = |trigger: &&&SchemaObject| {
        !in_family(&trigger.tbl_name)
            && !family_dependents
                .views
                .iter()
                .any(|v| v.name.eq_ignore_ascii_case(&trigger.tbl_name))
    };
    let mut statements: Vec<String> = family_dependents
        .triggers
        .iter()
        .filter(survives_drops)
        .map(|t| format!("DROP TRIGGER {}", quote_ident(&t.name)))
        .collect();
    statements.extend(
        family_dependents
            .views
            .iter()
            .map(|v| format!("DROP VIEW {}", quote_ident(&v.name))),
    );
    statements.extend(creates.iter().cloned());
    for (((member, _, copied), new_name), create) in members.iter().zip(&new_names).zip(&creates) {
        let old = &member.object.name;
        statements.push(format!(
            "INSERT INTO {} ({copied}) SELECT {copied} FROM {}",
            quote_ident(new_name),
            quote_ident(old)
        ));
        if has_keyword(create, "AUTOINCREMENT") {
            statements.push(format!(
                "DELETE FROM sqlite_sequence WHERE name = {}",
                quote_literal(new_name)
            ));
            statements.push(format!(
                "INSERT INTO sqlite_sequence (name, seq) SELECT {}, seq FROM sqlite_sequence WHERE name = {}",
                quote_literal(new_name),
                quote_literal(old)
            ));
        }
    }
    statements.extend(
        members
            .iter()
            .skip(1)
            .chain(members.iter().take(1))
            .map(|(m, _, _)| format!("DROP TABLE {}", quote_ident(&m.object.name))),
    );
    statements.extend(members.iter().zip(&new_names).map(|((m, _, _), new_name)| {
        format!(
            "ALTER TABLE {} RENAME TO {}",
            quote_ident(new_name),
            quote_ident(&m.object.name)
        )
    }));
    statements.extend(family_dependents.all().filter_map(|o| o.sql.clone()));
    Ok(statements)
}

/// `base`, or `base` with the first numeric suffix that makes it unused.
fn free_name(taken: &[&str], base: &str) -> String {
    let is_taken = |name: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(name));
    if !is_taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}_{n}"))
        .find(|name| !is_taken(name))
        .unwrap_or_else(|| base.to_string())
}

#[cfg(all(test, feature = "rusqlite"))]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::executor::{Executor, RusqliteExecutor};

    fn executor(schema: &str) -> RusqliteExecutor {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(schema).expect("seed");
        RusqliteExecutor::new(conn)
    }

    fn drop_column(column: &str) -> TableChange {
        TableChange::DropColumn {
            column: column.to_string(),
        }
    }

    /// Rebuilds `table` without `column` through the executor, as `up` does.
    fn rebuild(ex: &mut RusqliteExecutor, table: &str, column: &str) {
        let schema = ex.read_schema().expect("schema");
        let statements =
            rebuild_statements(&schema, table, &drop_column(column)).expect("statements");
        ex.run_atomic(&statements, true).expect("rebuild");
    }

    fn table_sql(ex: &mut RusqliteExecutor, table: &str) -> String {
        ex.read_schema()
            .expect("schema")
            .objects
            .into_iter()
            .find(|o| o.kind == "table" && o.name == table)
            .and_then(|o| o.sql)
            .expect("table sql")
    }

    fn query_i64(ex: &RusqliteExecutor, sql: &str) -> i64 {
        ex.connection().query_row(sql, [], |r| r.get(0)).expect(sql)
    }

    #[test]
    fn the_edited_statement_keeps_everything_but_the_dropped_column() {
        let mut ex = executor(
            "CREATE TABLE t (
               id INTEGER PRIMARY KEY,
               gone TEXT UNIQUE,
               loose,
               name TEXT COLLATE NOCASE CHECK (length(name) > 0),
               twice INTEGER GENERATED ALWAYS AS (id * 2) VIRTUAL
             )",
        );
        rebuild(&mut ex, "t", "gone");
        assert_eq!(
            table_sql(&mut ex, "t"),
            "CREATE TABLE \"t\" (
               id INTEGER PRIMARY KEY,
               loose,
               name TEXT COLLATE NOCASE CHECK (length(name) > 0),
               twice INTEGER GENERATED ALWAYS AS (id * 2) VIRTUAL
             )"
        );
    }

    #[test]
    fn the_last_item_takes_the_comma_before_it() {
        let mut ex = executor("CREATE TABLE t (a, b UNIQUE)");
        rebuild(&mut ex, "t", "b");
        assert_eq!(table_sql(&mut ex, "t"), "CREATE TABLE \"t\" (a)");
    }

    #[test]
    fn an_added_column_goes_after_the_last_column_item_and_ahead_of_constraints() {
        let mut ex = executor(
            "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT, UNIQUE (b)); \
             INSERT INTO t VALUES (1, 'x');",
        );
        let schema = ex.read_schema().expect("schema");
        let change = TableChange::AddColumn {
            definition: "\"c\" TEXT UNIQUE".to_string(),
        };
        let statements = rebuild_statements(&schema, "t", &change).expect("statements");
        ex.run_atomic(&statements, true).expect("rebuild");

        assert_eq!(
            table_sql(&mut ex, "t"),
            "CREATE TABLE \"t\" (a INTEGER PRIMARY KEY, b TEXT, \"c\" TEXT UNIQUE, UNIQUE (b))"
        );
        assert_eq!(query_i64(&ex, "SELECT count(*) FROM t WHERE c IS NULL"), 1);
    }

    #[test]
    fn rows_and_rowids_are_copied() {
        let mut ex = executor(
            "CREATE TABLE t (v TEXT, gone TEXT UNIQUE);
             INSERT INTO t (rowid, v, gone) VALUES (10, 'a', 'x'), (20, 'b', 'y');",
        );
        rebuild(&mut ex, "t", "gone");
        let rows: Vec<(i64, String)> = ex
            .connection()
            .prepare("SELECT rowid, v FROM t ORDER BY rowid")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(rows, [(10, "a".to_string()), (20, "b".to_string())]);
    }

    #[test]
    fn a_column_named_rowid_falls_back_to_another_alias() {
        let mut ex = executor(
            "CREATE TABLE t (rowid TEXT, gone UNIQUE);
             INSERT INTO t (oid, rowid, gone) VALUES (7, 'r', 1);",
        );
        rebuild(&mut ex, "t", "gone");
        assert_eq!(query_i64(&ex, "SELECT oid FROM t"), 7);
    }

    #[test]
    fn without_rowid_tables_copy_without_a_rowid() {
        let mut ex = executor(
            "CREATE TABLE t (k TEXT PRIMARY KEY, gone UNIQUE) WITHOUT ROWID;
             INSERT INTO t VALUES ('a', 1);",
        );
        rebuild(&mut ex, "t", "gone");
        assert_eq!(query_i64(&ex, "SELECT count(*) FROM t WHERE k = 'a'"), 1);
    }

    #[test]
    fn integer_primary_key_desc_stays_apart_from_the_rowid() {
        let mut ex = executor(
            "CREATE TABLE t (id INTEGER PRIMARY KEY DESC, v TEXT, gone UNIQUE);
             INSERT INTO t (rowid, id, v) VALUES (100, 1, 'a');",
        );
        rebuild(&mut ex, "t", "gone");
        ex.connection()
            .execute("INSERT INTO t (v) VALUES ('b')", [])
            .expect("insert");
        // Not a rowid alias: an insert that omits id leaves it NULL instead of taking a rowid.
        assert_eq!(query_i64(&ex, "SELECT count(*) FROM t WHERE id IS NULL"), 1);
        assert_eq!(query_i64(&ex, "SELECT rowid FROM t WHERE id = 1"), 100);
    }

    #[test]
    fn the_new_table_name_avoids_existing_objects() {
        assert_eq!(free_name(&["t"], "_migratr_rebuild"), "_migratr_rebuild");
        assert_eq!(
            free_name(
                &["_MIGRATR_REBUILD", "_migratr_rebuild_2"],
                "_migratr_rebuild"
            ),
            "_migratr_rebuild_3"
        );
    }

    #[test]
    fn unparseable_or_mismatched_statements_are_refused() {
        let mut ex = executor("CREATE TABLE t (a, b)");
        let mut schema = ex.read_schema().expect("schema");
        for broken in [
            "CREATE TABLE t (a, b",
            "CREATE TABLE t (x, b)",
            "CREATE TABLE t (a)",
            "CREATE /* c */ TABLE t (a, b)",
        ] {
            schema.objects[0].sql = Some(broken.to_string());
            let err = rebuild_statements(&schema, "t", &drop_column("b")).expect_err(broken);
            assert!(
                matches!(err, MigrateError::UnparseableTable { ref table } if table == "t"),
                "{broken}: {err:?}"
            );
        }
    }

    #[test]
    fn an_absent_table_or_column_is_refused() {
        let mut ex = executor("CREATE TABLE t (a, b)");
        let schema = ex.read_schema().expect("schema");
        for (table, column) in [("nope", "a"), ("t", "nope")] {
            assert!(matches!(
                rebuild_statements(&schema, table, &drop_column(column)),
                Err(MigrateError::UnparseableTable { .. })
            ));
        }
    }

    #[test]
    fn an_index_names_only_its_columns_and_where_clause() {
        let mut ex = executor(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, t TEXT, idx TEXT, a TEXT, b TEXT);
             CREATE INDEX idx ON t (a) WHERE b > 0;",
        );
        let schema = ex.read_schema().expect("schema");
        for (column, expected) in [("t", false), ("idx", false), ("a", true), ("b", true)] {
            assert_eq!(
                needs_rebuild(&schema, "t", column, &[]).expect(column),
                expected,
                "{column}"
            );
        }
    }

    #[test]
    fn only_expressions_and_constraints_name_a_column() {
        let name = |item: &str| names_own_column(item, "key");
        assert!(!name("body text NOT NULL"));
        assert!(!name("owner INTEGER REFERENCES users (key)"));
        assert!(!name("id INTEGER PRIMARY KEY"));
        assert!(name("n INTEGER CHECK (n > key)"));
        assert!(name("twice INTEGER AS (key * 2)"));
        assert!(name("UNIQUE (key)"));
        assert!(name("FOREIGN KEY (key) REFERENCES p (id)"));
        assert!(!name("FOREIGN KEY (a) REFERENCES p (key)"));
    }

    #[test]
    fn dropping_a_constrained_column_needs_the_rebuild_and_a_plain_one_does_not() {
        let mut ex = executor(
            "CREATE TABLE p (id INTEGER PRIMARY KEY);
             CREATE TABLE t (
               id INTEGER PRIMARY KEY,
               plain TEXT CHECK (plain <> ''),
               uniq TEXT UNIQUE,
               fk INTEGER REFERENCES p (id),
               indexed TEXT,
               used INTEGER,
               twice INTEGER GENERATED ALWAYS AS (used * 2),
               tc TEXT,
               UNIQUE (tc)
             );
             CREATE INDEX t_indexed ON t (indexed);",
        );
        let schema = ex.read_schema().expect("schema");
        for (column, expected) in [
            ("plain", false),
            ("id", true),
            ("uniq", true),
            ("fk", true),
            ("indexed", true),
            ("used", true),
            ("tc", true),
            ("absent", false),
        ] {
            assert_eq!(
                needs_rebuild(&schema, "t", column, &[]).expect(column),
                expected,
                "{column}"
            );
        }
        assert!(!needs_rebuild(&schema, "absent", "x", &[]).expect("absent table"));
    }

    #[test]
    fn a_dependent_object_naming_the_dropped_column_is_refused() {
        for (dependent, object) in [
            (
                "CREATE TRIGGER tr AFTER UPDATE ON t BEGIN SELECT NEW.gone; END",
                "tr",
            ),
            (
                "CREATE TRIGGER tr AFTER UPDATE ON t WHEN NEW.gone > 0 BEGIN SELECT 1; END",
                "tr",
            ),
            ("CREATE VIEW v AS SELECT gone FROM t", "v"),
            (
                "CREATE VIEW v AS SELECT id FROM t;                  CREATE TRIGGER vt INSTEAD OF INSERT ON v BEGIN INSERT INTO t (gone) VALUES (1); END",
                "vt",
            ),
        ] {
            let mut ex = executor(&format!(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, gone UNIQUE); {dependent}"
            ));
            let schema = ex.read_schema().expect("schema");
            let err = rebuild_statements(&schema, "t", &drop_column("gone")).expect_err(dependent);
            assert!(
                matches!(
                    err,
                    MigrateError::ColumnInUse { ref table, ref column, object: ref o }
                        if table == "t" && column == "gone" && o == object
                ),
                "{dependent}: {err:?}"
            );
        }
    }

    #[test]
    fn dependents_include_views_over_views_and_triggers_on_other_tables() {
        let mut ex = executor(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, gone UNIQUE);
             CREATE TABLE other (x);
             CREATE TABLE log (x);
             CREATE VIEW v1 AS SELECT id, name FROM t;
             CREATE VIEW v2 AS SELECT name FROM v1;
             CREATE VIEW unrelated AS SELECT x FROM other;
             CREATE TRIGGER on_other AFTER INSERT ON other BEGIN UPDATE t SET name = 'x'; END;
             CREATE TRIGGER via_view AFTER INSERT ON log BEGIN SELECT name FROM v2; END;
             CREATE TRIGGER quiet AFTER INSERT ON log BEGIN SELECT 1; END;
             INSERT INTO t (name) VALUES ('a');",
        );
        let schema = ex.read_schema().expect("schema");
        let mut touched: Vec<&str> = std::iter::once("t")
            .chain(
                Dependents::of(&schema, &["t"])
                    .all()
                    .map(|o| o.name.as_str()),
            )
            .collect();
        touched.sort();
        assert_eq!(touched, ["on_other", "t", "v1", "v2", "via_view"]);

        rebuild(&mut ex, "t", "gone");

        ex.connection()
            .execute_batch("INSERT INTO other VALUES (1); INSERT INTO log VALUES (1);")
            .expect("triggers run");
        assert_eq!(
            query_i64(&ex, "SELECT count(*) FROM v2 WHERE name = 'x'"),
            1
        );
        let mut names: Vec<String> = ex
            .read_schema()
            .expect("schema")
            .objects
            .into_iter()
            .map(|o| o.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "log",
                "on_other",
                "other",
                "quiet",
                "t",
                "unrelated",
                "v1",
                "v2",
                "via_view"
            ]
        );
    }
}
