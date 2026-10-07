use std::collections::HashMap;
use std::path::Path;

use migratr::{Executor, Migration, RusqliteExecutor, latest_snapshot};
use rusqlite::Connection;
use serde_json::json;

use crate::error::CliError;
use crate::{Outcome, open_existing};

/// Every migration file as applied or pending, ledger rows whose file is gone, and the latest
/// snapshot. A database that does not exist yet has nothing applied.
pub fn status(db: &Path, migrations: &[Migration]) -> Result<Outcome, CliError> {
    let (ledger, times) = match open_existing(db)? {
        Some(conn) => {
            let mut exec = RusqliteExecutor::new(conn);
            let ledger = exec.read_ledger()?;
            let times = if ledger.is_empty() {
                HashMap::new()
            } else {
                applied_times(exec.connection())?
            };
            (ledger, times)
        }
        None => (Vec::new(), HashMap::new()),
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
            "latest_snapshot": snapshot,
        }),
        text: lines.join("\n"),
    })
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
