//! SQLite's table rebuild: the procedure for schema changes ALTER TABLE cannot make in place
//! (sqlite.org/lang_altertable.html, section 7).
//!
//! The new table's CREATE statement is the stored one with the change edited into its text,
//! so everything else the author declared (types or their absence, collations, CHECK and
//! foreign-key clauses, generated columns, AUTOINCREMENT, `WITHOUT ROWID`) carries over
//! unchanged. The statements run inside the migration's transaction with foreign_keys
//! suspended by the executor; the caller ends the migration with [`fk_check`].

use crate::executor::{SchemaObject, SchemaSnapshot, TableInfo};
use crate::migration::{Column, MigrateError};
use crate::sql_ddl::{
    TableBody, TokenKind, create_table_name_span, has_keyword, index_names_column,
    leading_identifier, mentions_identifier, names_own_column, quote_ident, quote_literal,
    table_body, tokens,
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

    /// The CREATE statement with `definition` as a new column after the last column item,
    /// ahead of any table constraints.
    fn with_column(&self, definition: &str) -> String {
        let end = self.body.items[self.info.columns.len() - 1].end;
        format!("{}, {}{}", &self.sql[..end], definition, &self.sql[end..])
    }

    /// The CREATE statement without column `index`. The item goes with the comma after it,
    /// or before it when it is the last item.
    fn without_column(&self, index: usize) -> String {
        let items = &self.body.items;
        let removed = match items.get(index + 1) {
            Some(next) => items[index].start..next.start,
            None if index > 0 => items[index - 1].end..items[index].end,
            None => items[index].clone(),
        };
        format!("{}{}", &self.sql[..removed.start], &self.sql[removed.end..])
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
pub(crate) fn needs_rebuild(
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
    let indexed = Dependents::of(schema, table)
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

/// The objects a rebuild of a table drops and recreates.
struct Dependents<'a> {
    /// Explicit indexes on the table.
    indexes: Vec<&'a SchemaObject>,
    /// Views naming the table, or naming such a view, in sqlite_master order.
    views: Vec<&'a SchemaObject>,
    /// Triggers on the table or one of those views, or whose SQL names any of them, in
    /// sqlite_master order.
    triggers: Vec<&'a SchemaObject>,
}

impl<'a> Dependents<'a> {
    /// Renaming the rebuilt table into place fails while any view or trigger names a table
    /// or view that is missing at that moment, so every one of them is dropped first.
    fn of(schema: &'a SchemaSnapshot, table: &str) -> Self {
        let mut dropped: Vec<&str> = vec![table];
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
                .filter(|o| o.sql.is_some() && o.tbl_name.eq_ignore_ascii_case(table))
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

/// The statements that rebuild `table` with `change` applied, in order:
///
/// 1. drop the triggers and views that name the table, directly or through another view
/// 2. create the new table under a free name and copy the rows, rowids included
/// 3. carry the table's sqlite_sequence value to the new table
/// 4. drop the old table, which drops its indexes and triggers, and rename the new one
/// 5. recreate the indexes, then the views, then the triggers
///
/// Triggers are recreated after the copy, so they never fire on copied rows.
///
/// Refused before any statement with `UnparseableTable` when the table's stored CREATE
/// statement cannot be split into column definitions that match the table's columns, or
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
    let dependents = Dependents::of(schema, table_name);

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
            (parsed.without_column(dropped), Some(dropped))
        }
        TableChange::AddColumn { definition } => (parsed.with_column(definition), None),
    };

    let name_span =
        create_table_name_span(parsed.sql).ok_or_else(|| MigrateError::UnparseableTable {
            table: table.to_string(),
        })?;
    let taken: Vec<&str> = schema.objects.iter().map(|o| o.name.as_str()).collect();
    let new_name = free_name(&taken, "_migratr_rebuild");
    let new_create = format!(
        "{}{}{}",
        &edited[..name_span.start],
        quote_ident(&new_name),
        &edited[name_span.end..],
    );

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
    let copied = copied.join(", ");

    let quoted_table = quote_ident(table_name);
    let quoted_new = quote_ident(&new_name);
    let survives_drops = |trigger: &&&SchemaObject| {
        !trigger.tbl_name.eq_ignore_ascii_case(table_name)
            && !dependents
                .views
                .iter()
                .any(|v| v.name.eq_ignore_ascii_case(&trigger.tbl_name))
    };
    let mut statements: Vec<String> = dependents
        .triggers
        .iter()
        .filter(survives_drops)
        .map(|t| format!("DROP TRIGGER {}", quote_ident(&t.name)))
        .collect();
    statements.extend(
        dependents
            .views
            .iter()
            .map(|v| format!("DROP VIEW {}", quote_ident(&v.name))),
    );
    statements.push(new_create.clone());
    statements.push(format!(
        "INSERT INTO {quoted_new} ({copied}) SELECT {copied} FROM {quoted_table}"
    ));
    if has_keyword(&new_create, "AUTOINCREMENT") {
        statements.push(format!(
            "DELETE FROM sqlite_sequence WHERE name = {}",
            quote_literal(&new_name)
        ));
        statements.push(format!(
            "INSERT INTO sqlite_sequence (name, seq) SELECT {}, seq FROM sqlite_sequence WHERE name = {}",
            quote_literal(&new_name),
            quote_literal(table_name)
        ));
    }
    statements.push(format!("DROP TABLE {quoted_table}"));
    statements.push(format!("ALTER TABLE {quoted_new} RENAME TO {quoted_table}"));
    statements.extend(dependents.all().filter_map(|o| o.sql.clone()));
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

/// The message prefix of [`fk_check`]'s failure; the number of violating rows in a table
/// follows it, then ` in ` and the table: the first one, by name, that has any.
pub(crate) const FK_VIOLATION_MESSAGE: &str = "migratr: foreign key violations: ";

/// One statement that aborts with [`FK_VIOLATION_MESSAGE`] when the database has any
/// foreign-key violation. SQLite reports one pragma row per violated key, so a row that
/// breaks two keys is counted once by its rowid; a WITHOUT ROWID table has no rowid to
/// tell its rows apart and counts one per violated key. RAISE works only in a trigger, so the count goes into a temporary
/// table whose trigger raises; both are dropped again before the statement ends.
pub(crate) fn fk_check() -> String {
    let message = quote_literal(FK_VIOLATION_MESSAGE);
    format!(
        "CREATE TEMP TABLE _migratr_fk_raise (n INTEGER, tbl TEXT); \
         CREATE TEMP TRIGGER _migratr_fk_raise_trigger BEFORE INSERT ON _migratr_fk_raise \
           WHEN NEW.n > 0 BEGIN SELECT RAISE(ABORT, {message} || NEW.n || ' in ' || NEW.tbl); END; \
         INSERT INTO _migratr_fk_raise SELECT count(DISTINCT \"rowid\") + count(*) FILTER (WHERE \"rowid\" IS NULL), \"table\" \
           FROM pragma_foreign_key_check \
           WHERE \"table\" = (SELECT min(\"table\") FROM pragma_foreign_key_check); \
         DROP TABLE _migratr_fk_raise"
    )
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
    fn the_foreign_key_check_counts_every_violating_row() {
        let mut ex = executor(
            "CREATE TABLE p (id INTEGER PRIMARY KEY);
             CREATE TABLE c (pid INTEGER REFERENCES p (id));
             CREATE TABLE w (k TEXT PRIMARY KEY, pid INTEGER REFERENCES p (id)) WITHOUT ROWID;
             PRAGMA foreign_keys = OFF;
             INSERT INTO p VALUES (1);
             INSERT INTO c VALUES (1);",
        );
        ex.run_atomic(&[fk_check()], true).expect("no violations");

        let statements = [
            "INSERT INTO w VALUES ('a', 8), ('b', 8), ('c', 9)".to_string(),
            fk_check(),
        ];
        let err = ex
            .run_atomic(&statements, true)
            .expect_err("violations abort");
        assert_eq!(err.index, Some(1));
        let message = err.source.to_string();
        assert!(
            message.contains(&format!("{FK_VIOLATION_MESSAGE}3 in w")),
            "{message}"
        );
        assert_eq!(query_i64(&ex, "SELECT count(*) FROM w"), 0);

        // A row that breaks two keys is one row.
        let statements = [
            "CREATE TABLE two (a INTEGER REFERENCES p (id), b INTEGER REFERENCES p (id))"
                .to_string(),
            "INSERT INTO two VALUES (5, 6)".to_string(),
            fk_check(),
        ];
        let message = ex
            .run_atomic(&statements, true)
            .expect_err("violations abort")
            .source
            .to_string();
        assert!(
            message.contains(&format!("{FK_VIOLATION_MESSAGE}1 in two")),
            "{message}"
        );

        // With violations in two tables, the count is the first table's own.
        let statements = [
            "INSERT INTO w VALUES ('a', 8), ('b', 8), ('c', 9)".to_string(),
            "INSERT INTO c VALUES (5)".to_string(),
            fk_check(),
        ];
        let message = ex
            .run_atomic(&statements, true)
            .expect_err("violations abort")
            .source
            .to_string();
        assert!(
            message.contains(&format!("{FK_VIOLATION_MESSAGE}1 in c")),
            "{message}"
        );
        assert_eq!(query_i64(&ex, "SELECT count(*) FROM sqlite_temp_master"), 0);
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
            .chain(Dependents::of(&schema, "t").all().map(|o| o.name.as_str()))
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
