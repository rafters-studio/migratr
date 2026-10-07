use std::fs;
use std::path::Path;

use migratr::{RusqliteExecutor, load_dir, up, write_schema};
use rusqlite::Connection;
use tempfile::TempDir;

fn executor() -> RusqliteExecutor {
    RusqliteExecutor::new(Connection::open_in_memory().expect("open"))
}

const FIXTURE: &str = "tests/fixtures/embedded";

fn schema_json(dir: &Path) -> String {
    fs::read_to_string(dir.join("schema.json")).expect("schema.json")
}

#[test]
fn one_call_brings_an_empty_database_to_the_latest_version() {
    let migrator = migratr::embed!("tests/fixtures/embedded");
    let mut ex = executor();
    let report = migrator.up(&mut ex).expect("up");
    assert_eq!(report.applied, vec![20260101000001, 20260101000002]);
    assert!(migrator.plan(&mut ex).expect("plan").is_empty());
}

#[test]
fn the_embedded_schema_matches_the_directory_run_through_up() {
    let embedded = TempDir::new().expect("tempdir");
    let mut ex = executor();
    migratr::embed!("tests/fixtures/embedded")
        .up(&mut ex)
        .expect("up");
    write_schema(&mut ex, embedded.path()).expect("write schema");

    let direct = TempDir::new().expect("tempdir");
    let mut ex = executor();
    let migrations = load_dir(Path::new(FIXTURE)).expect("load");
    up(&mut ex, &migrations, None).expect("up");
    write_schema(&mut ex, direct.path()).expect("write schema");

    assert_eq!(schema_json(embedded.path()), schema_json(direct.path()));
}
