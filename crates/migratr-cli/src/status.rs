use std::collections::HashMap;
use std::path::Path;

use migratr::{Executor, Migration, Note, RusqliteExecutor, latest_snapshot};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::error::CliError;
use crate::{Outcome, has_ledger, open_existing};

/// Every migration file as applied or pending, ledger rows whose file is gone, and the latest
/// snapshot. A database that does not exist yet has nothing applied.
pub fn status(db: &Path, migrations: &[Migration]) -> Result<Outcome, CliError> {
    let (ledger, times, notes) = match open_existing(db)? {
        Some(conn) => {
            let mut exec = RusqliteExecutor::new(conn);
            let ledger = exec.read_ledger()?;
            let notes = notes(exec.connection())?;
            let times = if ledger.is_empty() {
                HashMap::new()
            } else {
                applied_times(exec.connection())?
            };
            (ledger, times, notes)
        }
        None => (Vec::new(), HashMap::new(), Vec::new()),
    };

    let mut lines = Vec::new();
    let mut files = Vec::new();
    for m in migrations {
        let row = ledger.iter().find(|r| r.version == m.version);
        let applied_at = row.and_then(|_| times.get(&m.version));
        let mismatch = row.is_some_and(|r| r.checksum != m.checksum);
        let state = if row.is_some() { "applied" } else { "pending" };
        lines.push(format!(
            "{}_{} {state}{}{}",
            m.version,
            m.name,
            applied_at.map_or(String::new(), |t| format!(" {t}")),
            if mismatch { " CHECKSUM MISMATCH" } else { "" },
        ));
        files.push(json!({
            "version": m.version,
            "name": m.name,
            "state": state,
            "applied_at": applied_at,
            "checksum_mismatch": mismatch,
        }));
    }

    let missing: Vec<_> = ledger
        .iter()
        .filter(|r| !migrations.iter().any(|m| m.version == r.version))
        .collect();
    for row in &missing {
        lines.push(format!("{}_{} MISSING FILE", row.version, row.name));
    }

    for (kind, value) in &notes {
        lines.push(format!("{kind} {value}"));
    }

    let snapshot = latest_snapshot(db)?;
    lines.push(match &snapshot {
        Some(path) => format!("latest snapshot: {}", path.display()),
        None => "latest snapshot: none".to_string(),
    });

    Ok(Outcome {
        fields: json!({
            "migrations": files,
            "missing": missing
                .iter()
                .map(|r| json!({ "version": r.version, "name": r.name }))
                .collect::<Vec<_>>(),
            "notes": notes
                .iter()
                .map(|(kind, value)| json!({ "kind": kind, "value": value }))
                .collect::<Vec<_>>(),
            "latest_snapshot": snapshot,
        }),
        text: lines.join("\n"),
    })
}

/// The ledger's notes, each as its kind and its JSON, in version order. None when there is no
/// ledger.
fn notes(conn: &Connection) -> rusqlite::Result<Vec<(String, Value)>> {
    if !has_ledger(conn)? {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for note in Note::ALL {
        let row: Option<String> = conn
            .query_row(
                "SELECT checksum FROM _migratr_migrations WHERE version = ?1",
                [i64::try_from(note.version()).unwrap_or(i64::MAX)],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(text) = row {
            let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
            found.push((note.name().to_string(), value));
        }
    }
    Ok(found)
}

/// When each applied version was applied, as the ledger recorded it.
fn applied_times(conn: &Connection) -> rusqlite::Result<HashMap<u64, String>> {
    conn.prepare("SELECT version, applied_at FROM _migratr_migrations")?
        .query_map([], |row| {
            let version: i64 = row.get(0)?;
            Ok((
                u64::try_from(version)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, version))?,
                row.get(1)?,
            ))
        })?
        .collect()
}
