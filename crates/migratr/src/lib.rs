//! Schema migrations for SQLite.

mod error;
mod executor;
mod migration;

pub use error::Error;
pub use executor::{ColumnInfo, Executor, ForeignKeyInfo, SchemaObject, SchemaSnapshot, TableInfo};
pub use migration::{
    Column, ForeignKey, GeneratedColumn, IndexDef, MigrateError, Migration, Op, TableDef, load_dir,
};

#[cfg(feature = "rusqlite")]
pub use executor::RusqliteExecutor;
