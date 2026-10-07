use std::fmt;

use crate::executor::Executor;
use crate::ledger;
use crate::migration::{IndexDef, MigrateError, Migration, Op};
use crate::sql_ddl::quote_ident as ident;
use crate::up::{apply, ident_list};

/// What a `down` run reverted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownReport {
    /// The versions reverted by this run, newest first.
    pub reverted: Vec<u64>,
}

impl fmt::Display for DownReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let versions: Vec<String> = self.reverted.iter().map(u64::to_string).collect();
        write!(
            f,
            "reverted {} migration(s): {}. Data in dropped objects is not restored.",
            self.reverted.len(),
            versions.join(", ")
        )
    }
}

/// Reverses the newest `steps` applied migrations, newest first, each with its ledger delete in
/// one atomic run. Refuses to run, naming the first irreversible operation, when any operation
/// in the range has no inverse. Reverses structure only.
pub fn down(
    exec: &mut impl Executor,
    migrations: &[Migration],
    steps: usize,
) -> Result<DownReport, MigrateError> {
    let ledger_rows = exec
        .read_ledger()
        .map_err(|e| MigrateError::Executor(Box::new(e)))?;
    ledger::verify(&ledger_rows, migrations)?;

    let mut versions: Vec<u64> = ledger_rows.iter().map(|r| r.version).collect();
    versions.sort_unstable_by(|a, b| b.cmp(a));
    versions.truncate(steps);

    // Every verified ledger row has a file, so the lookup cannot miss.
    let range: Vec<&Migration> = versions
        .iter()
        .filter_map(|v| migrations.iter().find(|m| m.version == *v))
        .collect();

    let plan = range
        .iter()
        .map(|m| Ok((m.version, inverse_ops(m)?)))
        .collect::<Result<Vec<_>, MigrateError>>()?;

    for (version, ops) in &plan {
        apply(exec, *version, ops, &[], ledger::delete_row(*version))?;
    }
    Ok(DownReport { reverted: versions })
}

/// The operations that undo `migration`, in the order to run them: the inverse of each
/// operation, last operation first.
fn inverse_ops(migration: &Migration) -> Result<Vec<Op>, MigrateError> {
    migration
        .up
        .iter()
        .enumerate()
        .rev()
        .map(|(op_index, op)| {
            inverse(op).ok_or(MigrateError::Irreversible {
                version: migration.version,
                op_index,
            })
        })
        .collect()
}

/// The operation that undoes `op`, or `None` when it has no inverse.
fn inverse(op: &Op) -> Option<Op> {
    Some(match op {
        Op::CreateTable { table, .. } => Op::RawSql {
            up: format!("DROP TABLE {}", ident(table)),
            down: None,
        },
        Op::DropTable { definition, .. } => Op::RawSql {
            up: definition.sql.clone(),
            down: None,
        },
        Op::RenameTable { from, to } => Op::RenameTable {
            from: to.clone(),
            to: from.clone(),
        },
        Op::AddColumn { table, column } => Op::DropColumn {
            table: table.clone(),
            column: column.clone(),
        },
        Op::DropColumn { table, column } => Op::AddColumn {
            table: table.clone(),
            column: column.clone(),
        },
        Op::RenameColumn { table, from, to } => Op::RenameColumn {
            table: table.clone(),
            from: to.clone(),
            to: from.clone(),
        },
        Op::CreateIndex {
            name,
            table,
            columns,
            unique,
        } => Op::DropIndex {
            definition: IndexDef {
                name: name.clone(),
                table: table.clone(),
                columns: columns.clone(),
                unique: *unique,
                where_clause: None,
            },
        },
        Op::DropIndex { definition } => Op::RawSql {
            up: create_index(definition),
            down: None,
        },
        Op::RawSql { down, .. } => Op::RawSql {
            up: down.clone()?,
            down: None,
        },
    })
}

/// The CREATE INDEX statement for `index`, partial clause included.
fn create_index(index: &IndexDef) -> String {
    let mut sql = format!(
        "CREATE {}INDEX {} ON {} ({})",
        if index.unique { "UNIQUE " } else { "" },
        ident(&index.name),
        ident(&index.table),
        ident_list(index.columns.iter().map(String::as_str)),
    );
    if let Some(clause) = &index.where_clause {
        sql.push_str(&format!(" WHERE {clause}"));
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(up: &str, down: Option<&str>) -> Op {
        Op::RawSql {
            up: up.into(),
            down: down.map(Into::into),
        }
    }

    #[test]
    fn renames_swap() {
        let op = Op::RenameColumn {
            table: "t".into(),
            from: "a".into(),
            to: "b".into(),
        };
        assert_eq!(
            inverse(&op),
            Some(Op::RenameColumn {
                table: "t".into(),
                from: "b".into(),
                to: "a".into(),
            })
        );
    }

    #[test]
    fn raw_sql_inverts_to_its_down_and_without_one_has_no_inverse() {
        assert_eq!(inverse(&raw("A", Some("B"))), Some(raw("B", None)),);
        assert_eq!(inverse(&raw("A", None)), None);
    }

    #[test]
    fn irreversible_names_the_last_operation_first() {
        let migration = Migration {
            version: 9,
            name: "m".into(),
            up: vec![raw("A", None), raw("B", Some("C")), raw("D", None)],
            checksum: String::new(),
        };
        let err = inverse_ops(&migration).expect_err("refused");
        assert!(matches!(
            err,
            MigrateError::Irreversible {
                version: 9,
                op_index: 2
            }
        ));
    }

    #[test]
    fn dropped_partial_index_recreates_with_its_where_clause() {
        let index = IndexDef {
            name: "i".into(),
            table: "t".into(),
            columns: vec!["a".into()],
            unique: true,
            where_clause: Some("a > 0".into()),
        };
        assert_eq!(
            create_index(&index),
            "CREATE UNIQUE INDEX \"i\" ON \"t\" (\"a\") WHERE a > 0"
        );
    }
}
