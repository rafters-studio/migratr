//! The schema as it stands partway through one migration.
//!
//! Each structured operation is applied to an in-memory copy of the schema read before the
//! migration, so a later table rebuild works from the text the earlier operations produced.
//! Changes that cannot be followed exactly are recorded, and a rebuild that would recreate an
//! object they touched is refused.

use crate::executor::{ColumnInfo, SchemaObject, SchemaSnapshot, TableInfo};
use crate::migration::{Column, MigrateError, Op};
use crate::rebuild::{TableChange, needs_rebuild, rebuild_statements, touched_objects};
use crate::sql_ddl::{
    create_table_name_span, mentions_identifier, quote_ident, rename_after_keyword,
    rename_column_in_index, rename_column_in_table,
};
use crate::up::{column_def, render};

pub(crate) struct Tracked {
    schema: SchemaSnapshot,
    /// The raw SQL operations so far, by operation index.
    raw_sql: Vec<(usize, String)>,
    /// Objects whose stored SQL SQLite rewrote in a way this copy does not follow, with the
    /// operation that caused it.
    untracked: Vec<(usize, String)>,
}

impl Tracked {
    pub(crate) fn new(schema: SchemaSnapshot) -> Self {
        Self {
            schema,
            raw_sql: Vec::new(),
            untracked: Vec::new(),
        }
    }

    /// Applies `op`, operation number `at` of migration `version`, to the schema copy. For a
    /// column drop SQLite cannot make in place, returns the rebuild's statements.
    ///
    /// Refused with `UntrackedChange` when a rebuild would recreate an object that an earlier
    /// raw SQL operation names or that an earlier rename left stale.
    pub(crate) fn step(
        &mut self,
        version: u64,
        at: usize,
        op: &Op,
    ) -> Result<Option<Vec<String>>, MigrateError> {
        let mut rebuild = None;
        match op {
            Op::CreateTable { table, columns, .. } => {
                self.schema
                    .objects
                    .push(object("table", table, table, Some(render(op))));
                self.schema.tables.push(TableInfo {
                    name: table.clone(),
                    columns: columns.iter().enumerate().map(column_info).collect(),
                    foreign_keys: Vec::new(),
                });
            }
            Op::DropTable { table, .. } => {
                self.schema.objects.retain(|o| {
                    let on_table = o.kind != "view" && o.tbl_name.eq_ignore_ascii_case(table);
                    !on_table
                });
                self.schema
                    .tables
                    .retain(|t| !t.name.eq_ignore_ascii_case(table));
            }
            Op::RenameTable { from, to } => self.rename_table(at, from, to),
            Op::AddColumn { table, column } => {
                if let Some(sql) =
                    crate::rebuild::sql_with_column(&self.schema, table, &column_def(column))
                {
                    let position = self
                        .schema
                        .tables
                        .iter()
                        .find(|t| same(&t.name, table))
                        .map(|t| t.columns.len());
                    if let Some(cid) = position {
                        self.set_table_sql(table, sql);
                        self.table_info(table, |t| t.columns.push(column_info((cid, column))));
                    }
                }
            }
            Op::DropColumn { table, column } => {
                let known = self.schema.tables.iter().any(|t| same(&t.name, table));
                if !known {
                    // The table came from an earlier raw SQL operation that this copy cannot read.
                    if let Some((operation, _)) = self
                        .raw_sql
                        .iter()
                        .find(|(_, sql)| mentions_identifier(sql, table))
                    {
                        return Err(untracked(version, table, *operation));
                    }
                } else if needs_rebuild(&self.schema, table, &column.name)? {
                    if let Some(operation) = self.untracked_change(table) {
                        return Err(untracked(version, table, operation));
                    }
                    let change = TableChange::DropColumn {
                        column: column.name.clone(),
                    };
                    rebuild = Some(rebuild_statements(&self.schema, table, &change)?);
                    self.move_rebuilt_to_end(table);
                }
                if let Some(sql) =
                    crate::rebuild::sql_without_column(&self.schema, table, &column.name)
                {
                    self.set_table_sql(table, sql);
                    self.table_info(table, |t| {
                        t.columns.retain(|c| !same(&c.name, &column.name));
                        for (cid, c) in t.columns.iter_mut().enumerate() {
                            c.cid = cid as i64;
                        }
                    });
                }
            }
            Op::RenameColumn { table, from, to } => self.rename_column(at, table, from, to),
            Op::CreateIndex { name, table, .. } => {
                self.schema
                    .objects
                    .push(object("index", name, table, Some(render(op))));
            }
            Op::DropIndex { definition } => {
                self.schema
                    .objects
                    .retain(|o| !(o.kind == "index" && same(&o.name, &definition.name)));
            }
            Op::RawSql { up, .. } => self.raw_sql.push((at, up.clone())),
        }
        Ok(rebuild)
    }

    /// The first earlier operation that changed, in a way this copy does not follow, an object
    /// a rebuild of `table` drops or recreates.
    fn untracked_change(&self, table: &str) -> Option<usize> {
        let touched = touched_objects(&self.schema, table);
        let by_raw_sql = self
            .raw_sql
            .iter()
            .filter(|(_, sql)| touched.iter().any(|name| mentions_identifier(sql, name)))
            .map(|(at, _)| *at);
        let by_rewrite = self
            .untracked
            .iter()
            .filter(|(_, name)| touched.iter().any(|t| same(t, name)))
            .map(|(at, _)| *at);
        by_raw_sql.chain(by_rewrite).min()
    }

    /// SQLite drops and recreates the rebuilt table and its dependents, which puts them last
    /// in sqlite_master in the order the rebuild creates them.
    fn move_rebuilt_to_end(&mut self, table: &str) {
        let order = touched_objects(&self.schema, table);
        let position = |o: &SchemaObject| order.iter().position(|name| same(name, &o.name));
        let (mut moved, mut kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.schema.objects)
            .into_iter()
            .partition(|o| position(o).is_some());
        moved.sort_by_key(position);
        kept.extend(moved);
        self.schema.objects = kept;
    }

    fn rename_table(&mut self, at: usize, from: &str, to: &str) {
        for o in &mut self.schema.objects {
            if o.kind == "table" {
                if same(&o.name, from) {
                    o.name = to.to_string();
                    o.tbl_name = to.to_string();
                    o.sql = o.sql.take().map(|sql| match create_table_name_span(&sql) {
                        Some(span) => format!(
                            "{}{}{}",
                            &sql[..span.start],
                            quote_ident(to),
                            &sql[span.end..]
                        ),
                        None => sql,
                    });
                }
                o.sql = o
                    .sql
                    .take()
                    .map(|sql| rename_after_keyword(&sql, "REFERENCES", from, to));
            } else if o.tbl_name.eq_ignore_ascii_case(from) {
                o.tbl_name = to.to_string();
                if o.kind == "index" {
                    o.sql = o
                        .sql
                        .take()
                        .map(|sql| rename_after_keyword(&sql, "ON", from, to));
                }
            }
            if matches!(o.kind.as_str(), "view" | "trigger")
                && o.sql
                    .as_deref()
                    .is_some_and(|sql| mentions_identifier(sql, from))
            {
                self.untracked.push((at, o.name.clone()));
            }
        }
        self.table_info(from, |t| t.name = to.to_string());
        let earlier: Vec<usize> = self
            .raw_sql
            .iter()
            .filter(|(_, sql)| mentions_identifier(sql, from))
            .map(|(operation, _)| *operation)
            .collect();
        self.untracked.extend(
            earlier
                .into_iter()
                .map(|operation| (operation, to.to_string())),
        );
    }

    fn rename_column(&mut self, at: usize, table: &str, from: &str, to: &str) {
        let mut rewritten = Vec::new();
        for o in &mut self.schema.objects {
            let Some(sql) = o.sql.as_deref() else {
                continue;
            };
            let on_table = o.tbl_name.eq_ignore_ascii_case(table);
            match o.kind.as_str() {
                "table" if on_table => {
                    let renamed = rename_column_in_table(sql, from, to);
                    // A self-reference's parent column list is left as written.
                    if mentions_identifier(&renamed, from) {
                        rewritten.push(o.name.clone());
                    }
                    o.sql = Some(renamed);
                }
                "table" => {
                    // A child table's REFERENCES list names this table's column.
                    if mentions_identifier(sql, table) && mentions_identifier(sql, from) {
                        rewritten.push(o.name.clone());
                    }
                }
                "index" if on_table => o.sql = Some(rename_column_in_index(sql, from, to)),
                "view" | "trigger" if mentions_identifier(sql, from) => {
                    rewritten.push(o.name.clone());
                }
                _ => {}
            }
        }
        self.untracked
            .extend(rewritten.into_iter().map(|name| (at, name)));
        self.table_info(table, |t| {
            for c in &mut t.columns {
                if same(&c.name, from) {
                    c.name = to.to_string();
                }
            }
        });
    }

    fn set_table_sql(&mut self, table: &str, sql: String) {
        if let Some(o) = self
            .schema
            .objects
            .iter_mut()
            .find(|o| o.kind == "table" && same(&o.name, table))
        {
            o.sql = Some(sql);
        }
    }

    fn table_info(&mut self, table: &str, change: impl FnOnce(&mut TableInfo)) {
        if let Some(t) = self.schema.tables.iter_mut().find(|t| same(&t.name, table)) {
            change(t);
        }
    }
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn untracked(version: u64, table: &str, operation: usize) -> MigrateError {
    MigrateError::UntrackedChange {
        version,
        table: table.to_string(),
        operation,
    }
}

fn object(kind: &str, name: &str, tbl_name: &str, sql: Option<String>) -> SchemaObject {
    SchemaObject {
        kind: kind.to_string(),
        name: name.to_string(),
        tbl_name: tbl_name.to_string(),
        sql,
    }
}

/// The pragma row for column number `cid`.
fn column_info((cid, column): (usize, &Column)) -> ColumnInfo {
    ColumnInfo {
        cid: cid as i64,
        name: column.name.clone(),
        decl_type: column.type_name.clone(),
        not_null: column.not_null,
        default_value: column.default.clone(),
        pk: column.primary_key.map_or(0, i64::from),
        hidden: column
            .generated
            .as_ref()
            .map_or(0, |g| if g.stored { 3 } else { 2 }),
    }
}
