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

    /// smugglr is missing or has no exec.
    #[error("{0}")]
    NoSmugglr(String),

    /// The place has tables and no ledger, and its shape is not what the migrations make.
    #[error("{0}")]
    NotAdoptable(String),

    /// The place's fingerprint is not the one the ledger recorded.
    #[error("{0}")]
    ShapeChanged(String),

    /// The ledger holds no rewind point, so there is nothing to ship from.
    #[error("{0}")]
    NoRewindPoint(String),

    /// A --smuggl run failed and put the local database back at the rewind point.
    #[error("{0}")]
    ShipFailed(String),

    /// smugglr ran and refused or failed, in its own words.
    #[error("{0}")]
    Smugglr(String),

    #[error("{}: {source}", path.display())]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
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
                MigrateError::NoSuchForeignKey { .. } => "no_such_foreign_key",
                MigrateError::ColumnInUse { .. } => "column_in_use",
                MigrateError::RebuildNotFirst { .. } => "rebuild_not_first",
                MigrateError::SnapshotFailed { .. } => "snapshot_failed",
                MigrateError::RestoreNotConfirmed { .. } => "restore_not_confirmed",
                MigrateError::TriggerMismatch { .. } => "trigger_mismatch",
                MigrateError::UnknownObject { .. } => "unknown_object",
                MigrateError::Executor(_) => "database",
            },
            CliError::Database(_) => "database",
            CliError::Usage(_) => "usage",
            CliError::NoSmugglr(_) => "smugglr_missing",
            CliError::NotAdoptable(_) => "not_adoptable",
            CliError::ShapeChanged(_) => "shape_changed",
            CliError::NoRewindPoint(_) => "no_rewind_point",
            CliError::ShipFailed(_) => "ship_failed",
            CliError::Smugglr(_) => "smugglr",
            CliError::Io { .. } => "io",
        }
    }
}
