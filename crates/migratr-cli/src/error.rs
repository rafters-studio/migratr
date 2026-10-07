use migratr::MigrateError;
use thiserror::Error;

/// A failed command. Its [`code`](CliError::code) is part of the CLI's public contract.
#[derive(Debug, Error)]
pub enum CliError {
    #[error(transparent)]
    Migrate(#[from] MigrateError),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("{0}")]
    Usage(String),
}

impl CliError {
    /// The stable machine-readable name of this failure.
    pub fn code(&self) -> &'static str {
        match self {
            CliError::Migrate(e) => match e {
                MigrateError::Parse { .. } => "parse",
                MigrateError::DuplicateVersion { .. } => "duplicate_version",
                MigrateError::Io { .. } => "io",
                MigrateError::ChecksumMismatch { .. } => "checksum_mismatch",
                MigrateError::MissingFile { .. } => "missing_file",
                MigrateError::Apply { .. } => "apply_failed",
                MigrateError::Irreversible { .. } => "irreversible",
                MigrateError::UnparseableTable { .. } => "unparseable_table",
                MigrateError::ForeignKeyViolation { .. } => "foreign_key_violation",
                MigrateError::ColumnInUse { .. } => "column_in_use",
                MigrateError::RebuildNotFirst { .. } => "rebuild_not_first",
                MigrateError::SnapshotFailed { .. } => "snapshot_failed",
                MigrateError::RestoreNotConfirmed { .. } => "restore_not_confirmed",
                MigrateError::UnknownObject { .. } => "unknown_object",
                MigrateError::Executor(_) => "database",
            },
            CliError::Database(_) => "database",
            CliError::Usage(_) => "usage",
        }
    }
}
