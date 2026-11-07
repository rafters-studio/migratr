use crate::executor::Executor;
use crate::ledger;
use crate::migration::{Column, ForeignKeyDef, IndexColumn, MigrateError, Migration, Op};
use crate::rebuild::{TableChange, add_needs_rebuild, family, op_rebuild, rebuild_statements};
use crate::snapshot::{Direction, snapshot_before};
use crate::sql_ddl::quote_ident as ident;

/// What an `up` run applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpReport {
    /// The versions applied by this run, in order.
    pub applied: Vec<u64>,
}

/// Applies the pending migrations in version order, each with its ledger row in one atomic
/// run. Stops after `to` when given. Refuses to run when a ledger row has no matching file
/// or its file's checksum differs.
pub fn up(
    exec: &mut impl Executor,
    migrations: &[Migration],
    to: Option<u64>,
) -> Result<UpReport, MigrateError> {
    let ledger_rows = exec
        .read_ledger()
        .map_err(|e| MigrateError::Executor(Box::new(e)))?;
    ledger::verify(&ledger_rows, migrations)?;

    let mut pending: Vec<&Migration> = migrations
        .iter()
        .filter(|m| !ledger_rows.iter().any(|r| r.version == m.version))
        .filter(|m| to.is_none_or(|limit| m.version <= limit))
        .collect();
    pending.sort_by_key(|m| m.version);

    let mut applied = Vec::with_capacity(pending.len());
    for migration in pending {
        apply(
            exec,
            migration.version,
            Direction::Up,
            &migration.up,
            &[ledger::CREATE_LEDGER.to_string()],
            ledger::insert_row(migration),
        )?;
        applied.push(migration.version);
    }
    Ok(UpReport { applied })
}

/// Runs `ops` for migration `version` in one atomic run: the `prelude` statements, the
/// operations, then `record`, the ledger statement that marks the migration applied or reverted.
pub(crate) fn apply(
    exec: &mut impl Executor,
    version: u64,
    direction: Direction,
    ops: &[Op],
    prelude: &[String],
    record: String,
) -> Result<(), MigrateError> {
    let schema = if ops.iter().any(|op| match op {
        Op::DropColumn { .. } | Op::AddForeignKey(_) | Op::DropForeignKey { .. } => true,
        Op::AddColumn { column, .. } => add_needs_rebuild(column),
        _ => false,
    }) {
        Some(
            exec.read_schema()
                .map_err(|e| MigrateError::Executor(Box::new(e)))?,
        )
    } else {
        None
    };

    // Each operation's statements, with the operation they came from.
    let mut body: Vec<(String, usize)> = Vec::new();
    let mut rebuilt: Vec<String> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let rebuild = match &schema {
            Some(schema) => op_rebuild(schema, ops, i)?,
            None => None,
        };
        if let (Some(schema), Some((table, column)), Some(change)) =
            (&schema, rebuild, table_change(op))
        {
            // The rebuild works from the schema read before the migration, which is
            // current only for the first operation.
            if i > 0 {
                return Err(MigrateError::RebuildNotFirst {
                    version,
                    table: table.clone(),
                    column,
                    operation: i,
                });
            }
            rebuilt = family(schema, table);
            body.extend(
                rebuild_statements(schema, table, &change)?
                    .into_iter()
                    .map(|s| (s, i)),
            );
            continue;
        }
        body.push((render(op), i));
    }

    // A rebuild copies every row of its family with foreign keys as they are, so rows a
    // foreign key refuses are counted first, before any change.
    if !rebuilt.is_empty() {
        let tables: Vec<&str> = rebuilt.iter().map(String::as_str).collect();
        let violations = exec
            .foreign_key_violations(&tables)
            .map_err(|e| MigrateError::Executor(Box::new(e)))?;
        if let Some((table, rows)) = violations.and_then(|v| v.into_iter().next()) {
            return Err(MigrateError::ForeignKeyViolation { table, rows });
        }
    }
    // An added foreign key's own rows are its check: rows naming a missing parent are counted
    // before any change.
    for op in ops {
        if let Op::AddForeignKey(key) = op {
            let missing = exec
                .missing_parents(key)
                .map_err(|e| MigrateError::Executor(Box::new(e)))?;
            if let Some(rows) = missing.filter(|rows| *rows > 0) {
                return Err(MigrateError::ForeignKeyViolation {
                    table: key.table.clone(),
                    rows,
                });
            }
        }
    }

    let mut statements = prelude.to_vec();
    let mut origins: Vec<Option<usize>> = vec![None; prelude.len()];
    for (statement, operation) in body {
        statements.push(statement);
        origins.push(Some(operation));
    }
    statements.push(record);
    origins.push(None);

    snapshot_before(exec, ops, version, direction)?;
    exec.run_atomic(&statements, false)
        .map_err(|failure| MigrateError::Apply {
            version,
            statement: failure.index.and_then(|i| statements.get(i)).cloned(),
            operation: failure
                .index
                .and_then(|i| origins.get(i).copied().flatten()),
            source: Box::new(failure.source),
        })
}

/// The change a rebuild makes for `op`, for an operation a rebuild can make.
fn table_change(op: &Op) -> Option<TableChange> {
    Some(match op {
        Op::DropColumn { column, .. } => TableChange::DropColumn {
            column: column.name.clone(),
        },
        Op::AddColumn { column, .. } => TableChange::AddColumn {
            definition: rebuilt_column_def(column),
        },
        Op::AddForeignKey(key) => TableChange::AddConstraint {
            definition: foreign_key_item(key),
        },
        Op::DropForeignKey { definition } => TableChange::DropForeignKey {
            columns: definition.columns.clone(),
            parent: definition.references.table.clone(),
        },
        _ => return None,
    })
}

/// The table-level `FOREIGN KEY` item for `key`.
fn foreign_key_item(key: &ForeignKeyDef) -> String {
    let mut sql = format!(
        "FOREIGN KEY ({}) REFERENCES {} ({})",
        ident_list(key.columns.iter().map(String::as_str)),
        ident(&key.references.table),
        ident_list(key.references.columns.iter().map(String::as_str)),
    );
    if let Some(action) = &key.on_delete {
        sql.push_str(&format!(" ON DELETE {action}"));
    }
    if let Some(action) = &key.on_update {
        sql.push_str(&format!(" ON UPDATE {action}"));
    }
    sql
}

/// The CREATE INDEX statement for an index, partial clause included.
pub(crate) fn create_index(
    name: &str,
    table: &str,
    columns: &[IndexColumn],
    unique: bool,
    where_clause: Option<&str>,
) -> String {
    let keys: Vec<String> = columns
        .iter()
        .map(|column| {
            let (key, desc) = match column {
                IndexColumn::Name(name) => (ident(name), false),
                IndexColumn::Column { name, desc } => (ident(name), *desc),
                IndexColumn::Expr { expr, desc } => (expr.clone(), *desc),
            };
            if desc { format!("{key} DESC") } else { key }
        })
        .collect();
    let mut sql = format!(
        "CREATE {}INDEX {} ON {} ({})",
        if unique { "UNIQUE " } else { "" },
        ident(name),
        ident(table),
        keys.join(", "),
    );
    if let Some(clause) = where_clause {
        sql.push_str(&format!(" WHERE {clause}"));
    }
    sql
}

/// The SQL statement for one operation.
fn render(op: &Op) -> String {
    match op {
        Op::CreateTable {
            table,
            columns,
            without_rowid,
            checks,
            unique,
        } => create_table(table, columns, *without_rowid, checks, unique),
        Op::DropTable { table, .. } => format!("DROP TABLE {}", ident(table)),
        Op::RenameTable { from, to } => {
            format!("ALTER TABLE {} RENAME TO {}", ident(from), ident(to))
        }
        Op::AddColumn { table, column } => {
            format!(
                "ALTER TABLE {} ADD COLUMN {}",
                ident(table),
                column_def(column)
            )
        }
        Op::DropColumn { table, column } => format!(
            "ALTER TABLE {} DROP COLUMN {}",
            ident(table),
            ident(&column.name)
        ),
        Op::RenameColumn { table, from, to } => format!(
            "ALTER TABLE {} RENAME COLUMN {} TO {}",
            ident(table),
            ident(from),
            ident(to)
        ),
        Op::CreateIndex {
            name,
            table,
            columns,
            unique,
            where_clause,
        } => create_index(name, table, columns, *unique, where_clause.as_deref()),
        Op::DropIndex { definition } => format!("DROP INDEX {}", ident(&definition.name)),
        Op::AddForeignKey(_) | Op::DropForeignKey { .. } => {
            unreachable!("a foreign key is added or dropped only by the rebuild")
        }
        Op::CreateTrigger(trigger) => trigger.sql.clone(),
        Op::DropTrigger { definition } => format!("DROP TRIGGER {}", ident(&definition.name)),
        Op::RawSql { up, .. } => up.clone(),
    }
}

fn create_table(
    table: &str,
    columns: &[Column],
    without_rowid: bool,
    checks: &[String],
    unique: &[Vec<String>],
) -> String {
    let mut parts: Vec<String> = columns.iter().map(column_def).collect();

    let mut key: Vec<&Column> = columns.iter().filter(|c| c.primary_key.is_some()).collect();
    key.sort_by_key(|c| c.primary_key);
    if !key.is_empty() {
        parts.push(format!(
            "PRIMARY KEY ({})",
            ident_list(key.iter().map(|c| c.name.as_str()))
        ));
    }
    parts.extend(checks.iter().map(|check| format!("CHECK ({check})")));
    parts.extend(
        unique
            .iter()
            .map(|names| format!("UNIQUE ({})", ident_list(names.iter().map(String::as_str)))),
    );

    format!(
        "CREATE TABLE {} ({}){}",
        ident(table),
        parts.join(", "),
        if without_rowid { " WITHOUT ROWID" } else { "" }
    )
}

/// The column's definition as a rebuilt table's column item, where a PRIMARY KEY column can
/// say so inline.
fn rebuilt_column_def(column: &Column) -> String {
    let mut sql = column_def(column);
    if column.primary_key.is_some() {
        sql.push_str(" PRIMARY KEY");
    }
    sql
}

fn column_def(column: &Column) -> String {
    let mut sql = ident(&column.name);
    if !column.type_name.is_empty() {
        sql.push(' ');
        sql.push_str(&column.type_name);
    }
    if let Some(collation) = &column.collation {
        sql.push_str(&format!(" COLLATE {collation}"));
    }
    if column.not_null {
        sql.push_str(" NOT NULL");
    }
    if column.unique {
        sql.push_str(" UNIQUE");
    }
    if let Some(default) = &column.default {
        sql.push_str(&format!(" DEFAULT ({default})"));
    }
    if let Some(check) = &column.check {
        sql.push_str(&format!(" CHECK ({check})"));
    }
    if let Some(fk) = &column.references {
        sql.push_str(&format!(
            " REFERENCES {} ({})",
            ident(&fk.table),
            ident(&fk.column)
        ));
        if let Some(action) = &fk.on_update {
            sql.push_str(&format!(" ON UPDATE {action}"));
        }
        if let Some(action) = &fk.on_delete {
            sql.push_str(&format!(" ON DELETE {action}"));
        }
    }
    if let Some(generated) = &column.generated {
        sql.push_str(&format!(
            " GENERATED ALWAYS AS ({}) {}",
            generated.expr,
            if generated.stored {
                "STORED"
            } else {
                "VIRTUAL"
            }
        ));
    }
    sql
}

pub(crate) fn ident_list<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.map(ident).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::ForeignKey;

    fn col(name: &str, type_name: &str) -> Column {
        Column {
            name: name.into(),
            type_name: type_name.into(),
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

    #[test]
    fn create_table_renders_columns_and_composite_key_in_key_order() {
        let mut a = col("a", "INTEGER");
        a.primary_key = Some(2);
        let mut b = col("b", "TEXT");
        b.primary_key = Some(1);
        b.not_null = true;
        b.collation = Some("NOCASE".into());
        let sql = create_table("t", &[a, b], true, &[], &[]);
        assert_eq!(
            sql,
            "CREATE TABLE \"t\" (\"a\" INTEGER, \"b\" TEXT COLLATE NOCASE NOT NULL, \
             PRIMARY KEY (\"b\", \"a\")) WITHOUT ROWID"
        );
    }

    #[test]
    fn create_table_renders_checks_then_unique_each_in_order() {
        let mut id = col("id", "INTEGER");
        id.primary_key = Some(1);
        let sql = create_table(
            "t",
            &[id, col("a", "INTEGER"), col("b", "TEXT")],
            false,
            &["a > 0".into(), "length(b) < 9".into()],
            &[vec!["a".into(), "b".into()], vec!["b".into()]],
        );
        assert_eq!(
            sql,
            "CREATE TABLE \"t\" (\"id\" INTEGER, \"a\" INTEGER, \"b\" TEXT, PRIMARY KEY (\"id\"), \
             CHECK (a > 0), CHECK (length(b) < 9), UNIQUE (\"a\", \"b\"), UNIQUE (\"b\"))"
        );
    }

    #[test]
    fn column_def_renders_every_constraint() {
        let mut c = col("owner", "INTEGER");
        c.unique = true;
        c.default = Some("0".into());
        c.check = Some("owner >= 0".into());
        c.references = Some(ForeignKey {
            table: "users".into(),
            column: "id".into(),
            on_update: None,
            on_delete: Some("CASCADE".into()),
        });
        assert_eq!(
            column_def(&c),
            "\"owner\" INTEGER UNIQUE DEFAULT (0) CHECK (owner >= 0) \
             REFERENCES \"users\" (\"id\") ON DELETE CASCADE"
        );
    }
}
