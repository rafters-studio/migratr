//! SQLite's table rebuild: the procedure for schema changes ALTER TABLE cannot make in place
//! (sqlite.org/lang_altertable.html, section 7).
//!
//! The new table's CREATE statement is the stored one with the change edited into its text,
//! so everything else the author declared (types or their absence, collations, CHECK and
//! foreign-key clauses, generated columns, AUTOINCREMENT, `WITHOUT ROWID`) carries over
//! unchanged. The statements run inside the migration's transaction with foreign_keys
//! suspended by the executor.

use crate::executor::{SchemaObject, SchemaSnapshot};
use crate::migration::MigrateError;
use crate::sql_ddl::{
    create_table_name_span, has_keyword, leading_identifier, mentions_identifier, quote_ident,
    quote_literal, table_body,
};

/// A change to one table that the rebuild makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TableChange {
    DropColumn { column: String },
}

/// The message prefix of the foreign-key check's failure; the violating row count follows it.
pub(crate) const FK_VIOLATION_MESSAGE: &str = "migratr: foreign key violations: ";

/// The statements that rebuild `table` with `change` applied, in order:
///
/// 1. drop the views that name the table (with their INSTEAD OF triggers)
/// 2. create the new table under a free name and copy the rows, rowids included
/// 3. carry the table's sqlite_sequence value to the new table
/// 4. drop the old table, which drops its indexes and triggers, and rename the new one
/// 5. recreate the indexes, then the views, then the triggers
/// 6. fail with [`FK_VIOLATION_MESSAGE`] if the table or any table referencing it now has
///    foreign-key violations
///
/// Triggers are recreated after the copy, so they never fire on copied rows.
///
/// Refused with `UnparseableTable` when the table's stored CREATE statement cannot be
/// split into column definitions that match the table's columns, or when the table or the
/// changed column is absent from `schema`.
pub(crate) fn rebuild_statements(
    schema: &SchemaSnapshot,
    table: &str,
    change: &TableChange,
) -> Result<Vec<String>, MigrateError> {
    let unparseable = || MigrateError::UnparseableTable {
        table: table.to_string(),
    };
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

    let object = schema
        .objects
        .iter()
        .find(|o| o.kind == "table" && same(&o.name, table))
        .ok_or_else(unparseable)?;
    let create_sql = object.sql.as_deref().ok_or_else(unparseable)?;
    let info = schema
        .tables
        .iter()
        .find(|t| same(&t.name, table))
        .ok_or_else(unparseable)?;
    let body = table_body(create_sql).ok_or_else(unparseable)?;
    let name_span = create_table_name_span(create_sql).ok_or_else(unparseable)?;

    // Column definitions precede table constraints, one item per column in declaration order.
    let declared = info.columns.len();
    let columns_match = body.items.len() >= declared
        && info.columns.iter().zip(&body.items).all(|(column, item)| {
            leading_identifier(&create_sql[item.clone()]).is_some_and(|n| same(&n, &column.name))
        });
    if !columns_match {
        return Err(unparseable());
    }

    let TableChange::DropColumn { column } = change;
    let dropped = info
        .columns
        .iter()
        .position(|c| same(&c.name, column))
        .ok_or_else(unparseable)?;

    // The dropped item goes with the comma after it, or before it when it is the last item.
    let removed = match body.items.get(dropped + 1) {
        Some(next) => body.items[dropped].start..next.start,
        None if dropped > 0 => body.items[dropped - 1].end..body.items[dropped].end,
        None => body.items[dropped].clone(),
    };

    let taken: Vec<&str> = schema.objects.iter().map(|o| o.name.as_str()).collect();
    let new_name = free_name(&taken, "_migratr_rebuild");
    let new_create = format!(
        "{}{}{}{}",
        &create_sql[..name_span.start],
        quote_ident(&new_name),
        &create_sql[name_span.end..removed.start],
        &create_sql[removed.end..],
    );

    // Generated columns compute their own values and cannot be inserted into.
    let mut copied: Vec<String> = info
        .columns
        .iter()
        .enumerate()
        .filter(|(i, c)| *i != dropped && c.hidden == 0)
        .map(|(_, c)| quote_ident(&c.name))
        .collect();
    let without_rowid = has_keyword(&create_sql[body.close + 1..], "ROWID");
    let rowid = ["rowid", "oid", "_rowid_"]
        .into_iter()
        .find(|alias| !info.columns.iter().any(|c| same(&c.name, alias)));
    if let (false, Some(rowid)) = (without_rowid, rowid) {
        copied.insert(0, rowid.to_string());
    }
    let copied = copied.join(", ");

    let dependent_views: Vec<&SchemaObject> = schema
        .objects
        .iter()
        .filter(|o| {
            o.kind == "view"
                && o.sql
                    .as_deref()
                    .is_some_and(|sql| mentions_identifier(sql, table))
        })
        .collect();
    let on_table_or_view = |o: &&SchemaObject| {
        same(&o.tbl_name, table) || dependent_views.iter().any(|v| same(&o.tbl_name, &v.name))
    };
    let recreated_sql = |kind: &str| -> Vec<String> {
        schema
            .objects
            .iter()
            .filter(|o| o.kind == kind)
            .filter(on_table_or_view)
            .filter_map(|o| o.sql.clone())
            .collect()
    };

    let quoted_table = quote_ident(&object.name);
    let quoted_new = quote_ident(&new_name);
    let mut statements: Vec<String> = dependent_views
        .iter()
        .map(|v| format!("DROP VIEW {}", quote_ident(&v.name)))
        .collect();
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
            quote_literal(&object.name)
        ));
    }
    statements.push(format!("DROP TABLE {quoted_table}"));
    statements.push(format!("ALTER TABLE {quoted_new} RENAME TO {quoted_table}"));
    statements.extend(recreated_sql("index"));
    statements.extend(dependent_views.iter().filter_map(|v| v.sql.clone()));
    statements.extend(recreated_sql("trigger"));
    statements.push(foreign_key_check(schema, &object.name));
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

/// One statement that aborts with [`FK_VIOLATION_MESSAGE`] and the count when `table`, or a
/// table whose foreign keys reference it, has foreign-key violations. RAISE works only in a
/// trigger, so the check inserts the count into a temporary table whose trigger raises; both
/// are dropped again before the statement ends.
fn foreign_key_check(schema: &SchemaSnapshot, table: &str) -> String {
    let mut checked = vec![table];
    for t in &schema.tables {
        let references = t
            .foreign_keys
            .iter()
            .any(|fk| fk.table.eq_ignore_ascii_case(table));
        if references && !checked.iter().any(|c| c.eq_ignore_ascii_case(&t.name)) {
            checked.push(&t.name);
        }
    }
    let violations = checked
        .iter()
        .map(|t| {
            format!(
                "SELECT 1 FROM pragma_foreign_key_check({})",
                quote_literal(t)
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let message = quote_literal(FK_VIOLATION_MESSAGE);
    format!(
        "CREATE TEMP TABLE _migratr_fk_check (n INTEGER); \
         CREATE TEMP TRIGGER _migratr_fk_check_raise BEFORE INSERT ON _migratr_fk_check \
         WHEN NEW.n > 0 BEGIN SELECT RAISE(ABORT, {message} || NEW.n); END; \
         INSERT INTO _migratr_fk_check SELECT count(*) FROM ({violations}); \
         DROP TABLE _migratr_fk_check"
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
    fn the_foreign_key_check_raises_with_the_violation_count() {
        let mut ex = executor(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE p (id INTEGER PRIMARY KEY, gone UNIQUE);
             CREATE TABLE c (pid INTEGER REFERENCES p (id));
             INSERT INTO c VALUES (8), (9);",
        );
        let schema = ex.read_schema().expect("schema");
        let statements = rebuild_statements(&schema, "p", &drop_column("gone")).expect("stmts");
        let err = ex
            .run_atomic(&statements, true)
            .expect_err("violations abort");
        assert_eq!(err.index, Some(statements.len() - 1));
        assert!(
            err.source
                .to_string()
                .contains(&format!("{FK_VIOLATION_MESSAGE}2")),
            "{}",
            err.source
        );
        assert_eq!(
            table_sql(&mut ex, "p"),
            "CREATE TABLE p (id INTEGER PRIMARY KEY, gone UNIQUE)"
        );
    }
}
