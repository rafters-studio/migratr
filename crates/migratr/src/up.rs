use crate::executor::Executor;
use crate::ledger;
use crate::migration::{Column, MigrateError, Migration, Op};
use crate::rebuild::{
    FK_VIOLATION_MESSAGE, TableChange, add_needs_rebuild, fk_check, needs_rebuild,
    rebuild_statements,
};
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
        Op::DropColumn { .. } => true,
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
    let mut has_rebuild = false;
    for (i, op) in ops.iter().enumerate() {
        let rebuild = match (&schema, op) {
            (Some(schema), Op::DropColumn { table, column }) => {
                // An earlier DropIndex has removed its index by the time the column goes.
                let dropped_indexes: Vec<&str> = ops[..i]
                    .iter()
                    .filter_map(|o| match o {
                        Op::DropIndex { definition } => Some(definition.name.as_str()),
                        _ => None,
                    })
                    .collect();
                needs_rebuild(schema, table, &column.name, &dropped_indexes)?.then(|| {
                    (
                        table,
                        column,
                        TableChange::DropColumn {
                            column: column.name.clone(),
                        },
                    )
                })
            }
            (Some(_), Op::AddColumn { table, column }) if add_needs_rebuild(column) => Some((
                table,
                column,
                TableChange::AddColumn {
                    definition: rebuilt_column_def(column),
                },
            )),
            _ => None,
        };
        if let (Some(schema), Some((table, column, change))) = (&schema, rebuild) {
            // The rebuild works from the schema read before the migration, which is
            // current only for the first operation.
            if i > 0 {
                return Err(MigrateError::RebuildNotFirst {
                    version,
                    table: table.clone(),
                    column: column.name.clone(),
                    operation: i,
                });
            }
            has_rebuild = true;
            body.extend(
                rebuild_statements(schema, table, &change)?
                    .into_iter()
                    .map(|s| (s, i)),
            );
            continue;
        }
        body.push((render(op), i));
    }

    // A migration with a rebuild runs with foreign keys suspended, so the whole database is
    // checked before the ledger row, as SQLite's own procedure does.
    let mut statements = prelude.to_vec();
    let mut origins: Vec<Option<usize>> = vec![None; prelude.len()];
    for (statement, operation) in body {
        statements.push(statement);
        origins.push(Some(operation));
    }
    let fk_check_at = has_rebuild.then_some(statements.len());
    if has_rebuild {
        statements.push(fk_check());
        origins.push(None);
    }
    statements.push(record);
    origins.push(None);

    snapshot_before(exec, ops, version, direction)?;
    exec.run_atomic(&statements, has_rebuild)
        .map_err(|failure| {
            let violation = failure
                .index
                .filter(|&i| Some(i) == fk_check_at)
                .and_then(|_| foreign_key_violation(&failure.source.to_string()));
            violation.unwrap_or_else(|| MigrateError::Apply {
                version,
                statement: failure.index.and_then(|i| statements.get(i)).cloned(),
                operation: failure
                    .index
                    .and_then(|i| origins.get(i).copied().flatten()),
                source: Box::new(failure.source),
            })
        })
}

/// The `ForeignKeyViolation` that an executor's [`fk_check`] failure message reports.
fn foreign_key_violation(message: &str) -> Option<MigrateError> {
    let report = message.split(FK_VIOLATION_MESSAGE).nth(1)?;
    let (rows, table) = report.split_once(" in ")?;
    Some(MigrateError::ForeignKeyViolation {
        table: table.trim().to_string(),
        rows: rows.parse().ok()?,
    })
}

/// The SQL statement for one operation.
fn render(op: &Op) -> String {
    match op {
        Op::CreateTable {
            table,
            columns,
            without_rowid,
        } => create_table(table, columns, *without_rowid),
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
        } => format!(
            "CREATE {}INDEX {} ON {} ({})",
            if *unique { "UNIQUE " } else { "" },
            ident(name),
            ident(table),
            ident_list(columns.iter().map(String::as_str)),
        ),
        Op::DropIndex { definition } => format!("DROP INDEX {}", ident(&definition.name)),
        Op::RawSql { up, .. } => up.clone(),
    }
}

fn create_table(table: &str, columns: &[Column], without_rowid: bool) -> String {
    let mut parts: Vec<String> = columns.iter().map(column_def).collect();

    let mut key: Vec<&Column> = columns.iter().filter(|c| c.primary_key.is_some()).collect();
    key.sort_by_key(|c| c.primary_key);
    if !key.is_empty() {
        parts.push(format!(
            "PRIMARY KEY ({})",
            ident_list(key.iter().map(|c| c.name.as_str()))
        ));
    }

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
        let sql = create_table("t", &[a, b], true);
        assert_eq!(
            sql,
            "CREATE TABLE \"t\" (\"a\" INTEGER, \"b\" TEXT COLLATE NOCASE NOT NULL, \
             PRIMARY KEY (\"b\", \"a\")) WITHOUT ROWID"
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
