use migratr::{AtomicError, Executor, LedgerRow, Migration, SchemaSnapshot, down, up};
use rusqlite::Connection;
use rusqlite::backup::Backup;
use serde_json::{Value, json};

use crate::Direction;
use crate::error::CliError;

/// One migration's atomic run as `up` or `down` issued it.
struct Step {
    statements: Vec<String>,
    /// The file name of the snapshot taken before the run, for a destructive step.
    snapshot: Option<String>,
}

/// An executor that forwards to `inner` and records each atomic run. A snapshot request is
/// recorded and not taken.
struct Recorder<E> {
    inner: E,
    pending_snapshot: Option<String>,
    steps: Vec<Step>,
}

impl<E: Executor> Executor for Recorder<E> {
    type Error = E::Error;

    fn read_schema(&mut self) -> Result<SchemaSnapshot, Self::Error> {
        self.inner.read_schema()
    }

    fn read_ledger(&mut self) -> Result<Vec<LedgerRow>, Self::Error> {
        self.inner.read_ledger()
    }

    fn run_atomic(
        &mut self,
        statements: &[String],
        suspend_foreign_keys: bool,
    ) -> Result<(), AtomicError<Self::Error>> {
        self.inner.run_atomic(statements, suspend_foreign_keys)?;
        self.steps.push(Step {
            statements: statements.to_vec(),
            snapshot: self.pending_snapshot.take(),
        });
        Ok(())
    }

    fn snapshot(&mut self, file_name: &str) -> Result<Option<std::path::PathBuf>, Self::Error> {
        self.pending_snapshot = Some(file_name.to_string());
        Ok(None)
    }
}

/// The plan as text: each step's version, its snapshot when destructive, and its statements.
pub fn render(plan: &Value) -> String {
    let mut lines = vec![format!(
        "plan {}; database is {} bytes",
        plan["direction"].as_str().unwrap_or_default(),
        plan["database_bytes"]
    )];
    for step in plan["steps"].as_array().into_iter().flatten() {
        let name = step["name"].as_str().unwrap_or_default();
        let mark = match step["snapshot"].as_str() {
            Some(snapshot) => format!(" DESTRUCTIVE, snapshot {snapshot}"),
            None => String::new(),
        };
        lines.push(format!("{}_{name}{mark}", step["version"]));
        for statement in step["statements"].as_array().into_iter().flatten() {
            lines.push(format!("  {}", statement.as_str().unwrap_or_default()));
        }
    }
    if lines.len() == 1 {
        lines.push("nothing to do".to_string());
    }
    lines.join("\n")
}

/// Runs `up` or `down` through a [`Recorder`] over an in-memory copy of `db`, so the statements
/// are exactly the ones a real run generates and the database itself is only read. A `db` that
/// does not exist plans against an empty database.
pub fn plan(
    db: &Connection,
    db_bytes: u64,
    migrations: &[Migration],
    direction: Direction,
) -> Result<Value, CliError> {
    let mut copy = Connection::open_in_memory()?;
    Backup::new(db, &mut copy)?.run_to_completion(256, std::time::Duration::ZERO, None)?;

    let mut recorder = Recorder {
        inner: migratr::RusqliteExecutor::new(copy),
        pending_snapshot: None,
        steps: Vec::new(),
    };
    let versions = match direction {
        Direction::Up { to } => up(&mut recorder, migrations, to)?.applied,
        Direction::Down { steps } => down(&mut recorder, migrations, steps)?.reverted,
    };

    let steps: Vec<Value> = versions
        .iter()
        .zip(recorder.steps)
        .map(|(version, step)| {
            let name = migrations
                .iter()
                .find(|m| m.version == *version)
                .map(|m| m.name.as_str());
            json!({
                "version": version,
                "name": name,
                "destructive": step.snapshot.is_some(),
                "snapshot": step.snapshot,
                "statements": step.statements,
            })
        })
        .collect();
    Ok(json!({
        "direction": direction.name(),
        "database_bytes": db_bytes,
        "steps": steps,
    }))
}
