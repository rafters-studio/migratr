//! `Migrator`: the migrations `embed!` read at compile time, run through the same code as the
//! CLI.

use crate::down::{DownReport, down};
use crate::executor::Executor;
use crate::ledger;
use crate::migration::{MigrateError, Migration, load_sources};
use crate::up::{UpReport, up};

/// One migration file and whether the database has applied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStatus {
    pub version: u64,
    pub name: String,
    pub applied: bool,
}

/// A migrations directory embedded in the program.
///
/// A file that does not parse stops the build:
///
/// ```compile_fail
/// let _migrator = migratr::embed!("tests/fixtures/malformed");
/// ```
#[derive(Debug, Clone)]
pub struct Migrator {
    migrations: Vec<Migration>,
}

impl Migrator {
    /// Builds a migrator from the (file name, contents) pairs `embed!` expands to. `embed!`
    /// has already parsed every pair with this parser, so the panic is unreachable from it.
    #[doc(hidden)]
    pub fn from_validated(sources: &[(&str, &str)]) -> Self {
        match load_sources(sources) {
            Ok(migrations) => Self { migrations },
            Err(e) => panic!("embedded migrations do not parse: {e}"),
        }
    }

    /// The embedded migrations, in version order.
    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    /// Applies every pending migration.
    pub fn up(&self, exec: &mut impl Executor) -> Result<UpReport, MigrateError> {
        up(exec, &self.migrations, None)
    }

    /// Reverses the newest `steps` applied migrations.
    pub fn down(&self, exec: &mut impl Executor, steps: usize) -> Result<DownReport, MigrateError> {
        down(exec, &self.migrations, steps)
    }

    /// Every embedded migration with whether the database has applied it. Refuses, as `up`
    /// does, when a ledger row has no file or its file's checksum differs.
    pub fn status(&self, exec: &mut impl Executor) -> Result<Vec<MigrationStatus>, MigrateError> {
        let applied = self.applied_versions(exec)?;
        Ok(self
            .migrations
            .iter()
            .map(|m| MigrationStatus {
                version: m.version,
                name: m.name.clone(),
                applied: applied.contains(&m.version),
            })
            .collect())
    }

    /// The migrations `up` would apply, in order.
    pub fn plan(&self, exec: &mut impl Executor) -> Result<Vec<&Migration>, MigrateError> {
        let applied = self.applied_versions(exec)?;
        Ok(self
            .migrations
            .iter()
            .filter(|m| !applied.contains(&m.version))
            .collect())
    }

    fn applied_versions(&self, exec: &mut impl Executor) -> Result<Vec<u64>, MigrateError> {
        let rows = exec
            .read_ledger()
            .map_err(|e| MigrateError::Executor(Box::new(e)))?;
        ledger::verify(&rows, &self.migrations)?;
        Ok(rows.iter().map(|r| r.version).collect())
    }
}

#[cfg(all(test, feature = "rusqlite"))]
mod tests {
    use super::*;
    use crate::executor::RusqliteExecutor;
    use rusqlite::Connection;

    const ONE: &str = r#"{"up": [{"op": "create_table", "table": "one", "columns": [
        {"name": "id", "type": "INTEGER", "primary_key": 1}]}]}"#;
    const TWO: &str = r#"{"up": [{"op": "create_table", "table": "two", "columns": [
        {"name": "id", "type": "INTEGER", "primary_key": 1}]}]}"#;

    fn migrator() -> Migrator {
        Migrator::from_validated(&[
            ("20260101000002_two.json", TWO),
            ("20260101000001_one.json", ONE),
        ])
    }

    fn executor() -> RusqliteExecutor {
        RusqliteExecutor::new(Connection::open_in_memory().expect("open"))
    }

    #[test]
    fn up_applies_in_version_order_and_status_and_plan_follow() {
        let m = migrator();
        let mut ex = executor();
        assert_eq!(m.plan(&mut ex).expect("plan").len(), 2);

        let report = m.up(&mut ex).expect("up");
        assert_eq!(report.applied, vec![20260101000001, 20260101000002]);
        assert!(m.plan(&mut ex).expect("plan").is_empty());
        assert!(m.status(&mut ex).expect("status").iter().all(|s| s.applied));

        m.down(&mut ex, 1).expect("down");
        let status = m.status(&mut ex).expect("status");
        assert_eq!(
            status.iter().map(|s| s.applied).collect::<Vec<_>>(),
            vec![true, false]
        );
        let plan = m.plan(&mut ex).expect("plan");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].name, "two");
    }

    #[test]
    fn status_refuses_an_edited_migration_like_up() {
        let mut ex = executor();
        migrator().up(&mut ex).expect("up");
        let edited = Migrator::from_validated(&[
            ("20260101000001_one.json", r#"{"up": []}"#),
            ("20260101000002_two.json", TWO),
        ]);
        assert!(matches!(
            edited.status(&mut ex),
            Err(MigrateError::ChecksumMismatch { .. })
        ));
    }
}
