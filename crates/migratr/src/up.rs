use crate::executor::Executor;
use crate::ledger;
use crate::migration::{Column, MigrateError, Migration, Op};

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
        let op_statements: Vec<String> = migration.up.iter().map(render).collect();
        let mut statements = Vec::with_capacity(op_statements.len() + 2);
        statements.push(ledger::CREATE_LEDGER.to_string());
        statements.extend(op_statements.iter().cloned());
        statements.push(ledger::insert_row(migration));

        exec.run_atomic(&statements, false)
            .map_err(|source| MigrateError::Apply {
                version: migration.version,
                statement: op_statements.join("; "),
                source: Box::new(source),
            })?;
        applied.push(migration.version);
    }
    Ok(UpReport { applied })
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

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn ident_list<'a>(names: impl Iterator<Item = &'a str>) -> String {
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

    #[test]
    fn identifiers_escape_quotes() {
        assert_eq!(ident("we\"ird"), "\"we\"\"ird\"");
    }
}
