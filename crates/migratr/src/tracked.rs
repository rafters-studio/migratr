//! The earlier operations of one migration, as a table rebuild sees them.
//!
//! A rebuild works from the schema read before the migration. When an earlier operation of
//! the same migration changed the table or an object the rebuild recreates, that schema is
//! stale and no rebuild is made.

use crate::executor::SchemaSnapshot;
use crate::migration::{Column, MigrateError, Op};
use crate::rebuild::{TableChange, needs_rebuild, rebuild_statements, touched_objects};
use crate::sql_ddl::mentions_identifier;

/// What an earlier operation involved: the names a structured operation changed, or the text
/// of a raw SQL operation.
enum Earlier {
    Names { names: Vec<String>, rewrites: bool },
    Sql(String),
}

/// How to run one operation.
pub(crate) enum Plan {
    /// The operation's own statement.
    Plain,
    /// A column drop that SQLite cannot make in place: the table rebuild's statements.
    Rebuild(Vec<String>),
    /// A column drop on a table or objects an earlier operation changed, so the schema read
    /// before the migration cannot say whether it needs a rebuild. It runs in place, and
    /// SQLite's refusal means the rebuild is refused, naming the earlier operation.
    AfterChange { operation: usize },
}

pub(crate) struct Tracked {
    schema: SchemaSnapshot,
    /// The earlier operations, by operation index.
    earlier: Vec<(usize, Earlier)>,
}

impl Tracked {
    pub(crate) fn new(schema: SchemaSnapshot) -> Self {
        Self {
            schema,
            earlier: Vec::new(),
        }
    }

    /// Records operation number `at` and says how to run it.
    pub(crate) fn step(&mut self, at: usize, op: &Op) -> Result<Plan, MigrateError> {
        let mut plan = Plan::Plain;
        if let Op::DropColumn { table, column } = op {
            let rebuild = needs_rebuild(&self.schema, table, &column.name)?;
            plan = match (self.changed_since_read(table), rebuild) {
                (Some(operation), _) => Plan::AfterChange { operation },
                (None, true) => {
                    let change = TableChange::DropColumn {
                        column: column.name.clone(),
                    };
                    Plan::Rebuild(rebuild_statements(&self.schema, table, &change)?)
                }
                (None, false) => Plan::Plain,
            };
        }
        self.earlier.push((at, earlier(op)));
        Ok(plan)
    }

    /// The first earlier operation that involved an object a rebuild of `table` drops or
    /// recreates, the table included. A rename or drop also counts when it involved an
    /// object that those objects' SQL names, since SQLite rewrites that SQL. Raw SQL cannot be
    /// told apart, so it counts when it names any of them.
    fn changed_since_read(&self, table: &str) -> Option<usize> {
        let touched = touched_objects(&self.schema, table);
        let neighbours: Vec<&str> = self
            .schema
            .objects
            .iter()
            .filter(|o| touched.iter().any(|t| same(t, &o.name)))
            .filter_map(|o| o.sql.as_deref())
            .flat_map(|sql| {
                self.schema
                    .objects
                    .iter()
                    .filter(move |o| mentions_identifier(sql, &o.name))
                    .map(|o| o.name.as_str())
            })
            .collect();
        self.earlier
            .iter()
            .find(|(_, earlier)| match earlier {
                Earlier::Sql(sql) => touched
                    .iter()
                    .map(String::as_str)
                    .chain(neighbours.iter().copied())
                    .any(|name| mentions_identifier(sql, name)),
                Earlier::Names { names, rewrites } => names.iter().any(|n| {
                    touched.iter().any(|t| same(t, n))
                        || (*rewrites && neighbours.iter().any(|t| same(t, n)))
                }),
            })
            .map(|(at, _)| *at)
    }
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn earlier(op: &Op) -> Earlier {
    let referenced =
        |c: &Column| -> Vec<String> { c.references.iter().map(|fk| fk.table.clone()).collect() };
    let names = |names: Vec<String>, rewrites: bool| Earlier::Names { names, rewrites };
    match op {
        Op::CreateTable { table, columns, .. } => names(
            std::iter::once(table.clone())
                .chain(columns.iter().flat_map(referenced))
                .collect(),
            false,
        ),
        Op::AddColumn { table, column } => names(
            std::iter::once(table.clone())
                .chain(referenced(column))
                .collect(),
            false,
        ),
        Op::DropTable { table, .. } => names(vec![table.clone()], true),
        Op::RenameTable { from, to } => names(vec![from.clone(), to.clone()], true),
        Op::RenameColumn { table, .. } => names(vec![table.clone()], true),
        Op::DropColumn { table, .. } => names(vec![table.clone()], false),
        Op::CreateIndex { name, table, .. } => names(vec![name.clone(), table.clone()], false),
        Op::DropIndex { definition } => names(
            vec![definition.name.clone(), definition.table.clone()],
            false,
        ),
        Op::RawSql { up, .. } => Earlier::Sql(up.clone()),
    }
}
