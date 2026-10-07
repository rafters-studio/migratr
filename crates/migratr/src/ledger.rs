use crate::executor::LedgerRow;
use crate::migration::{MigrateError, Migration};

/// Creates the ledger when it is absent. Every migration's atomic run starts with it, so the
/// table appears together with the first applied migration and rolls back with it.
pub(crate) const CREATE_LEDGER: &str = "CREATE TABLE IF NOT EXISTS _migratr_migrations (\
    version INTEGER PRIMARY KEY, \
    name TEXT NOT NULL, \
    checksum TEXT NOT NULL, \
    applied_at TEXT NOT NULL)";

/// The statement that records `migration` as applied. SQLite stamps the time, so the ledger
/// row and the migration's statements share one transaction and one clock.
pub(crate) fn insert_row(migration: &Migration) -> String {
    format!(
        "INSERT INTO _migratr_migrations (version, name, checksum, applied_at) \
         VALUES ({}, {}, {}, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        migration.version,
        quote_literal(&migration.name),
        quote_literal(&migration.checksum),
    )
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

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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
        let sql = insert_row(&migration(5, "it's", "ab"));
        assert!(sql.contains("VALUES (5, 'it''s', 'ab', strftime("), "{sql}");
    }
}
