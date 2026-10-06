//! Builds migratr without its rusqlite feature next to sqlx's SQLite driver,
//! proving the two link into one binary.

use migratr::SchemaSnapshot;
use sqlx::sqlite::SqliteConnectOptions;

fn main() {
    let snapshot = SchemaSnapshot::default();
    let options = SqliteConnectOptions::new().filename(":memory:");
    println!("{} objects, {:?}", snapshot.objects.len(), options);
}
