//! Schema migrations for SQLite.

mod down;
mod error;
mod executor;
mod ledger;
mod migration;
mod rebuild;
mod sql_ddl;
mod up;

pub use down::{DownReport, down};
pub use error::Error;
pub use executor::{
    AtomicError, ColumnInfo, Executor, ForeignKeyInfo, LedgerRow, SchemaObject, SchemaSnapshot,
    TableInfo,
};
pub use migration::{
    Column, ForeignKey, GeneratedColumn, IndexDef, MigrateError, Migration, Op, TableDef, load_dir,
};
pub use up::{UpReport, up};

#[cfg(feature = "rusqlite")]
pub use executor::RusqliteExecutor;
