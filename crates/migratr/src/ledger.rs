#[cfg(feature = "rusqlite")]
use sha2::{Digest, Sha256};

use crate::executor::{Executor, LedgerRow, SchemaSnapshot};
use crate::migration::{MigrateError, Migration};
use crate::sql_ddl::quote_literal;

pub(crate) const LEDGER: &str = "_migratr_migrations";

/// Creates the ledger when it is absent. Every migration's atomic run starts with it, so the
/// table appears together with the first applied migration and rolls back with it.
pub(crate) const CREATE_LEDGER: &str = "CREATE TABLE IF NOT EXISTS _migratr_migrations (\
    version INTEGER PRIMARY KEY, \
    name TEXT NOT NULL, \
    checksum TEXT NOT NULL, \
    applied_at TEXT NOT NULL, \
    up_hash TEXT, \
    down_hash TEXT)";

/// The hashes of the statements a migration runs up and the statements that revert it. `None`
/// when they are not known, as for a row `record_applied` writes or an irreversible down.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Hashes {
    pub(crate) up: Option<String>,
    pub(crate) down: Option<String>,
}

/// SHA-256 in lowercase hex of `statements` joined with a newline, with no trailing newline.
#[cfg(feature = "rusqlite")]
pub(crate) fn hash(statements: &[String]) -> String {
    format!("{:x}", Sha256::digest(statements.join("\n")))
}

/// The statements that give a ledger written before the hash columns those columns, keeping
/// its rows; none when `schema` has no ledger or its ledger has them.
pub(crate) fn upgrade(schema: &SchemaSnapshot) -> Vec<String> {
    let Some(table) = schema.tables.iter().find(|t| t.name == LEDGER) else {
        return Vec::new();
    };
    ["up_hash", "down_hash"]
        .into_iter()
        .filter(|column| !table.columns.iter().any(|c| c.name == *column))
        .map(|column| format!("ALTER TABLE {LEDGER} ADD COLUMN {column} TEXT"))
        .collect()
}

fn text_or_null(value: Option<&str>) -> String {
    value.map_or_else(|| "NULL".to_string(), quote_literal)
}

/// The statement that records `migration` as applied. SQLite stamps the time, so the ledger
/// row and the migration's statements share one transaction and one clock.
pub(crate) fn insert_row(migration: &Migration, hashes: &Hashes) -> String {
    format!(
        "INSERT INTO _migratr_migrations (version, name, checksum, applied_at, up_hash, down_hash) \
         VALUES ({}, {}, {}, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), {}, {})",
        migration.version,
        quote_literal(&migration.name),
        quote_literal(&migration.checksum),
        text_or_null(hashes.up.as_deref()),
        text_or_null(hashes.down.as_deref()),
    )
}

/// The statement that removes the ledger row of the reverted migration `version`.
pub(crate) fn delete_row(version: u64) -> String {
    format!("DELETE FROM _migratr_migrations WHERE version = {version}")
}

/// Records `migration` as applied without running its operations, creating the ledger when it
/// is absent. For a database that already has the migration's shape. The ledger and the row
/// land in one atomic run. A version the ledger already holds is refused with `Apply`. The row's
/// hashes are NULL, since nothing ran.
pub fn record_applied(exec: &mut impl Executor, migration: &Migration) -> Result<(), MigrateError> {
    let schema = exec
        .read_schema()
        .map_err(|e| MigrateError::Executor(Box::new(e)))?;
    let mut statements = vec![CREATE_LEDGER.to_string()];
    statements.extend(upgrade(&schema));
    statements.push(insert_row(migration, &Hashes::default()));
    exec.run_atomic(&statements, false)
        .map_err(|failure| MigrateError::Apply {
            version: migration.version,
            statement: failure.index.and_then(|i| statements.get(i)).cloned(),
            operation: None,
            source: Box::new(failure.source),
        })
}

/// A JSON row of the ledger that is not a migration. Each kind sits at a reserved version no
/// migration takes, since migration versions are timestamps, with its JSON in `checksum` and
/// both hashes NULL. Every reader of migrations leaves them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// The snapshot migratr dev took, as the way back.
    RewindPoint,
    /// The place's structural fingerprint, as smugglr exec's read form gives it.
    Fingerprint,
}

impl Note {
    /// Every kind of note, in version order.
    pub const ALL: [Note; 2] = [Note::RewindPoint, Note::Fingerprint];

    /// The reserved version this kind of note is kept at.
    #[must_use]
    pub const fn version(self) -> u64 {
        match self {
            Note::RewindPoint => 0,
            Note::Fingerprint => 1,
        }
    }

    /// The name this kind of note is kept under, which is how status lists it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Note::RewindPoint => "rewind_point",
            Note::Fingerprint => "fingerprint",
        }
    }
}

/// Writes `json` as the ledger's `note`, replacing any earlier one of that kind and creating the
/// ledger when it is absent, in one atomic run. `applied_at` is the time it was written.
pub fn record_note(exec: &mut impl Executor, note: Note, json: &str) -> Result<(), MigrateError> {
    let schema = exec
        .read_schema()
        .map_err(|e| MigrateError::Executor(Box::new(e)))?;
    let mut statements = vec![CREATE_LEDGER.to_string()];
    statements.extend(upgrade(&schema));
    statements.push(format!(
        "INSERT OR REPLACE INTO _migratr_migrations \
         (version, name, checksum, applied_at, up_hash, down_hash) \
         VALUES ({}, {}, {}, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NULL, NULL)",
        note.version(),
        quote_literal(note.name()),
        quote_literal(json),
    ));
    exec.run_atomic(&statements, false)
        .map_err(|failure| MigrateError::Executor(Box::new(failure.source)))
}

/// Runs `upgrade` on its own, for a run with no migration to carry it.
pub(crate) fn run_upgrade(
    exec: &mut impl Executor,
    upgrade: &[String],
) -> Result<(), MigrateError> {
    if upgrade.is_empty() {
        return Ok(());
    }
    exec.run_atomic(upgrade, false)
        .map_err(|failure| MigrateError::Executor(Box::new(failure.source)))
}

/// Checks every ledger row against the migration file of the same version.
pub(crate) fn verify(ledger: &[LedgerRow], migrations: &[Migration]) -> Result<(), MigrateError> {
    for row in ledger {
        let Some(file) = migrations.iter().find(|m| m.version == row.version) else {
            return Err(MigrateError::MissingFile {
                version: row.version,
                name: row.name.clone(),
            });
        };
        if file.checksum != row.checksum {
            return Err(MigrateError::ChecksumMismatch {
                version: row.version,
                name: row.name.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migration(version: u64, name: &str, checksum: &str) -> Migration {
        Migration {
            version,
            name: name.into(),
            up: Vec::new(),
            checksum: checksum.into(),
        }
    }

    fn row(version: u64, name: &str, checksum: &str) -> LedgerRow {
        LedgerRow {
            version,
            name: name.into(),
            checksum: checksum.into(),
            up_hash: None,
            down_hash: None,
        }
    }

    #[test]
    fn matching_rows_verify() {
        let files = [migration(1, "a", "x"), migration(2, "b", "y")];
        verify(&[row(1, "a", "x")], &files).expect("verify");
    }

    #[test]
    fn edited_file_is_a_mismatch_naming_the_migration() {
        let err = verify(&[row(1, "a", "x")], &[migration(1, "a", "z")]).expect_err("refused");
        assert!(matches!(
            err,
            MigrateError::ChecksumMismatch { version: 1, ref name } if name == "a"
        ));
    }

    #[test]
    fn absent_file_is_missing_naming_the_migration() {
        let err = verify(&[row(7, "gone", "x")], &[migration(1, "a", "x")]).expect_err("refused");
        assert!(matches!(
            err,
            MigrateError::MissingFile { version: 7, ref name } if name == "gone"
        ));
    }

    #[test]
    fn insert_quotes_text_values() {
        let hashes = Hashes {
            up: Some("u".into()),
            down: None,
        };
        let sql = insert_row(&migration(5, "it's", "ab"), &hashes);
        assert!(sql.contains("VALUES (5, 'it''s', 'ab', strftime("), "{sql}");
        assert!(sql.ends_with(", 'u', NULL)"), "{sql}");
    }

    #[cfg(feature = "rusqlite")]
    #[test]
    fn notes_are_kept_at_their_versions_and_left_out_of_the_migrations() {
        let mut exec =
            crate::RusqliteExecutor::new(rusqlite::Connection::open_in_memory().expect("open"));
        record_note(&mut exec, Note::RewindPoint, r#"{"snapshot":"a"}"#).expect("rewind");
        record_note(&mut exec, Note::Fingerprint, r#"{"fingerprint":"f"}"#).expect("fingerprint");
        record_note(&mut exec, Note::Fingerprint, r#"{"fingerprint":"g"}"#).expect("replaced");

        assert!(exec.read_ledger().expect("ledger").is_empty());
        let rows: Vec<(i64, String, String)> = exec
            .connection()
            .prepare("SELECT version, name, checksum FROM _migratr_migrations ORDER BY version")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            rows,
            vec![
                (0, "rewind_point".into(), r#"{"snapshot":"a"}"#.into()),
                (1, "fingerprint".into(), r#"{"fingerprint":"g"}"#.into()),
            ]
        );
    }

    #[cfg(feature = "rusqlite")]
    #[test]
    fn hash_joins_statements_with_a_newline_and_no_trailing_newline() {
        let joined = format!("{:x}", Sha256::digest("A;\nB;"));
        assert_eq!(hash(&["A;".into(), "B;".into()]), joined);
        assert_eq!(joined.len(), 64);
    }
}
