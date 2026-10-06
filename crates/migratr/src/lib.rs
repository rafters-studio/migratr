//! Schema migrations for SQLite.

mod error;
mod executor;

pub use error::Error;
pub use executor::{ColumnInfo, Executor, ForeignKeyInfo, SchemaObject, SchemaSnapshot, TableInfo};

#[cfg(feature = "rusqlite")]
pub use executor::RusqliteExecutor;
