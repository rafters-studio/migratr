//! SQLite's table rebuild: the procedure for schema changes ALTER TABLE cannot make in place
//! (sqlite.org/lang_altertable.html, section 7).
//!
//! The new table's CREATE statement is the stored one with the change edited into its text,
//! so everything else the author declared (types or their absence, collations, CHECK and
//! foreign-key clauses, generated columns, AUTOINCREMENT, `WITHOUT ROWID`) carries over
//! unchanged. The statements run inside the migration's transaction with foreign_keys
//! suspended by the executor; the caller brackets the migration with [`FK_BASELINE`] and
//! [`fk_check`].

use crate::executor::{SchemaObject, SchemaSnapshot, TableInfo};
use crate::migration::MigrateError;
use crate::sql_ddl::{
    TableBody, create_table_name_span, has_keyword, leading_identifier, mentions_identifier,
    quote_ident, quote_literal, table_body,
};

/// A change to one table that the rebuild makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TableChange {
    DropColumn { column: String },
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
/// when the table or column is absent, so the in-place statement reports it.
pub(crate) fn needs_rebuild(
    schema: &SchemaSnapshot,
    table: &str,
    column: &str,
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
    let named_elsewhere = (0..parsed.body.items.len())
        .filter(|&i| i != index)
        .any(|i| mentions_identifier(parsed.item(i), column));
    let indexed = Dependents::of(schema, table)
        .indexes
        .iter()
        .any(|o| mentions(o, column));
    Ok(constrained || named_elsewhere || indexed)
}

/// `table`'s CREATE statement without `column`, or `None` when it does not parse or has no
/// such column.
pub(crate) fn sql_without_column(
    schema: &SchemaSnapshot,
    table: &str,
    column: &str,
) -> Option<String> {
    let parsed = parse(schema, table).ok()?;
    Some(parsed.without_column(parsed.column_index(column)?))
}

/// `table`'s CREATE statement with `definition` added after its last column, or `None` when
/// it does not parse.
pub(crate) fn sql_with_column(
    schema: &SchemaSnapshot,
    table: &str,
    definition: &str,
) -> Option<String> {
    let parsed = parse(schema, table).ok()?;
    let last = parsed.info.columns.len().checked_sub(1)?;
    let at = parsed.body.items[last].end;
    Some(format!(
        "{}, {definition}{}",
        &parsed.sql[..at],
        &parsed.sql[at..]
    ))
}

fn mentions(object: &SchemaObject, name: &str) -> bool {
    object
        .sql
        .as_deref()
        .is_some_and(|sql| mentions_identifier(sql, name))
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

/// The names of every object a rebuild of `table` drops or recreates, the table included.
pub(crate) fn touched_objects(schema: &SchemaSnapshot, table: &str) -> Vec<String> {
    std::iter::once(table.to_string())
        .chain(Dependents::of(schema, table).all().map(|o| o.name.clone()))
        .collect()
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
/// when a recreated index, view or trigger names the dropped column, since SQLite does not
/// check a view or trigger body when it is created and the object would break on first use.
pub(crate) fn rebuild_statements(
    schema: &SchemaSnapshot,
    table: &str,
    change: &TableChange,
) -> Result<Vec<String>, MigrateError> {
    let parsed = parse(schema, table)?;
    let TableChange::DropColumn { column } = change;
    let dropped = parsed
        .column_index(column)
        .ok_or_else(|| MigrateError::UnparseableTable {
            table: table.to_string(),
        })?;
    let table_name = &parsed.object.name;

    let dependents = Dependents::of(schema, table_name);
    if let Some(user) = dependents.all().find(|o| mentions(o, column)) {
        return Err(MigrateError::ColumnInUse {
            table: table_name.clone(),
            column: column.clone(),
            object: user.name.clone(),
        });
    }

    let name_span =
        create_table_name_span(parsed.sql).ok_or_else(|| MigrateError::UnparseableTable {
            table: table.to_string(),
        })?;
    let taken: Vec<&str> = schema.objects.iter().map(|o| o.name.as_str()).collect();
    let new_name = free_name(&taken, "_migratr_rebuild");
    let without = parsed.without_column(dropped);
    let new_create = format!(
        "{}{}{}",
        &without[..name_span.start],
        quote_ident(&new_name),
        &without[name_span.end..],
    );

    // Generated columns compute their own values and cannot be inserted into.
    let mut copied: Vec<String> = parsed
        .info
        .columns
        .iter()
        .enumerate()
        .filter(|(i, c)| *i != dropped && c.hidden == 0)
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

/// Records the foreign-key violations that exist before a migration's first statement, so
/// [`fk_check`] blames the migration only for violations it adds.
pub(crate) const FK_BASELINE: &str =
    "CREATE TEMP TABLE _migratr_fk_before AS SELECT * FROM pragma_foreign_key_check";

/// The message prefix of [`fk_check`]'s failure; the count of new violations follows it,
/// then ` in ` and the first table that has one.
pub(crate) const FK_VIOLATION_MESSAGE: &str = "migratr: foreign key violations: ";

/// One statement that aborts with [`FK_VIOLATION_MESSAGE`] when the database has
/// foreign-key violations that [`FK_BASELINE`] did not record. RAISE works only in a trigger,
/// so the count goes into a temporary table whose trigger raises. Every temporary object is
/// dropped again before the statement ends.
pub(crate) fn fk_check() -> String {
    let message = quote_literal(FK_VIOLATION_MESSAGE);
    format!(
        "CREATE TEMP TABLE _migratr_fk_new AS \
           SELECT * FROM pragma_foreign_key_check EXCEPT SELECT * FROM _migratr_fk_before; \
         CREATE TEMP TABLE _migratr_fk_raise (n INTEGER, tbl TEXT); \
         CREATE TEMP TRIGGER _migratr_fk_raise_trigger BEFORE INSERT ON _migratr_fk_raise \
           WHEN NEW.n > 0 BEGIN SELECT RAISE(ABORT, {message} || NEW.n || ' in ' || NEW.tbl); END; \
         INSERT INTO _migratr_fk_raise SELECT count(*), min(\"table\") FROM _migratr_fk_new; \
         DROP TABLE _migratr_fk_raise; \
         DROP TABLE _migratr_fk_new; \
         DROP TABLE _migratr_fk_before"
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
    fn the_foreign_key_check_blames_only_new_violations() {
        let mut ex = executor(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE p (id INTEGER PRIMARY KEY);
             CREATE TABLE c (pid INTEGER REFERENCES p (id));
             INSERT INTO c VALUES (8);",
        );
        let check = |extra: &str| vec![FK_BASELINE.to_string(), extra.to_string(), fk_check()];

        ex.run_atomic(&check("INSERT INTO p VALUES (1)"), true)
            .expect("the existing orphan is not blamed");

        let statements = check("INSERT INTO c VALUES (9), (10)");
        let err = ex
            .run_atomic(&statements, true)
            .expect_err("new orphans abort");
        assert_eq!(err.index, Some(2));
        let message = err.source.to_string();
        assert!(
            message.contains(&format!("{FK_VIOLATION_MESSAGE}2 in c")),
            "{message}"
        );
        assert_eq!(query_i64(&ex, "SELECT count(*) FROM c"), 1);
        assert_eq!(query_i64(&ex, "SELECT count(*) FROM sqlite_temp_master"), 0);
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
                needs_rebuild(&schema, "t", column).expect(column),
                expected,
                "{column}"
            );
        }
        assert!(!needs_rebuild(&schema, "absent", "x").expect("absent table"));
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
        let mut touched = touched_objects(&schema, "t");
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

    #[test]
    fn the_snapshot_text_edits_add_and_remove_columns() {
        let mut ex = executor("CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY (a))");
        let schema = ex.read_schema().expect("schema");
        assert_eq!(
            sql_with_column(&schema, "t", "\"c\" TEXT").as_deref(),
            Some("CREATE TABLE t (a INTEGER, b TEXT, \"c\" TEXT, PRIMARY KEY (a))")
        );
        assert_eq!(
            sql_without_column(&schema, "t", "b").as_deref(),
            Some("CREATE TABLE t (a INTEGER, PRIMARY KEY (a))")
        );
        assert_eq!(sql_without_column(&schema, "t", "nope"), None);
    }
}
