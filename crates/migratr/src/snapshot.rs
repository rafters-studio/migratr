use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::executor::Executor;
use crate::migration::{MigrateError, Op};

/// How many snapshots are kept when no setting says otherwise.
const DEFAULT_KEEP: usize = 1;

/// Which way a step runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Up,
    Down,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
        }
    }
}

/// What a restore replaces and what it discards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// When the snapshot was taken, as its file name's `YYYYMMDDHHMMSS` UTC timestamp.
    pub snapshot_time: String,
    /// The newest version in the snapshot's ledger; `None` when the ledger is empty.
    pub snapshot_version: Option<u64>,
    /// Every version in the database's ledger that the snapshot's ledger lacks, newest first.
    pub removed: Vec<u64>,
    /// Whether the database was replaced.
    pub restored: bool,
}

impl RestoreReport {
    pub(crate) fn discards(&self) -> String {
        let versions: Vec<String> = self.removed.iter().map(u64::to_string).collect();
        let version = self
            .snapshot_version
            .map_or_else(|| "none".to_string(), |v| v.to_string());
        format!(
            "everything written since the snapshot of {} (ledger version {version}); ledger versions removed: {}",
            self.snapshot_time,
            if versions.is_empty() {
                "none".to_string()
            } else {
                versions.join(", ")
            }
        )
    }
}

impl fmt::Display for RestoreReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = if self.restored {
            "restored"
        } else {
            "would restore"
        };
        write!(f, "{verb}; discards {}", self.discards())
    }
}

/// Whether running `ops` loses data: any drop, or raw SQL migratr cannot see into.
pub(crate) fn is_destructive(ops: &[Op]) -> bool {
    ops.iter().any(|op| {
        matches!(
            op,
            Op::DropTable { .. }
                | Op::DropColumn { .. }
                | Op::DropIndex { .. }
                | Op::DropTrigger { .. }
                | Op::RawSql { .. }
        )
    })
}

/// Before a destructive step, has the executor write a snapshot named for the step, then
/// deletes the oldest snapshots beyond the retention count. The step must not run when this
/// fails. A step that is not destructive, or a database that needs no snapshot, passes.
pub(crate) fn snapshot_before(
    exec: &mut impl Executor,
    ops: &[Op],
    version: u64,
    direction: Direction,
) -> Result<(), MigrateError> {
    if !is_destructive(ops) {
        return Ok(());
    }
    let file_name = format!(
        "{}_{version}_{}.db",
        timestamp(SystemTime::now()),
        direction.as_str()
    );
    let failed = |path: &Path, source: Box<dyn std::error::Error + Send + Sync>| {
        MigrateError::SnapshotFailed {
            path: path.to_path_buf(),
            source,
        }
    };
    let written = exec
        .snapshot(&file_name)
        .map_err(|e| failed(Path::new(&file_name), Box::new(e)))?;
    let Some(path) = written else {
        return Ok(());
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    prune(dir, &path, DEFAULT_KEEP)
}

/// Replaces the database at `db_path` with a snapshot, the newest by default. The caller must
/// have closed its connections to the database. Without `confirmed` nothing changes and the
/// error carries the report of what the restore would discard.
#[cfg(feature = "rusqlite")]
pub fn restore(
    db_path: &Path,
    snapshot: Option<&Path>,
    confirmed: bool,
) -> Result<RestoreReport, MigrateError> {
    use crate::executor::RusqliteExecutor;

    let io_err = |path: &Path, source: io::Error| MigrateError::Io {
        path: path.to_path_buf(),
        source,
    };
    let ledger_versions = |path: &Path| -> Result<Vec<u64>, MigrateError> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| MigrateError::Executor(Box::new(e)))?;
        let rows = RusqliteExecutor::new(conn)
            .read_ledger()
            .map_err(|e| MigrateError::Executor(Box::new(e)))?;
        Ok(rows.into_iter().map(|r| r.version).collect())
    };

    let source = match snapshot {
        Some(path) => path.to_path_buf(),
        None => {
            let dir = snapshot_dir(db_path);
            list_snapshots(&dir)
                .map_err(|e| io_err(&dir, e))?
                .pop()
                .ok_or_else(|| {
                    io_err(
                        &dir,
                        io::Error::new(io::ErrorKind::NotFound, "no snapshots"),
                    )
                })?
        }
    };

    let snapshot_time = source
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split('_').next())
        .unwrap_or_default()
        .to_string();
    let kept = ledger_versions(&source)?;
    let mut removed: Vec<u64> = if db_path.exists() {
        ledger_versions(db_path)?
            .into_iter()
            .filter(|v| !kept.contains(v))
            .collect()
    } else {
        Vec::new()
    };
    removed.sort_unstable_by(|a, b| b.cmp(a));

    let mut report = RestoreReport {
        snapshot_time,
        snapshot_version: kept.iter().max().copied(),
        removed,
        restored: false,
    };
    if !confirmed {
        return Err(MigrateError::RestoreNotConfirmed {
            report: Box::new(report),
        });
    }

    // Copy beside the database, then rename over it, so a failed copy leaves the database alone.
    let staged = db_path.with_extension("migratr-restore");
    fs::copy(&source, &staged).map_err(|e| io_err(&staged, e))?;
    fs::rename(&staged, db_path).map_err(|e| io_err(db_path, e))?;
    // Journal files of the replaced database do not belong to the restored one.
    for suffix in ["-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_owned();
        name.push(suffix);
        match fs::remove_file(PathBuf::from(&name)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(Path::new(&name), e)),
        }
    }
    report.restored = true;
    Ok(report)
}

/// The newest snapshot beside the database at `db_path`, or `None` when there is none.
#[cfg(feature = "rusqlite")]
pub fn latest_snapshot(db_path: &Path) -> Result<Option<PathBuf>, MigrateError> {
    let dir = snapshot_dir(db_path);
    match list_snapshots(&dir) {
        Ok(mut found) => Ok(found.pop()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(MigrateError::Io { path: dir, source }),
    }
}

#[cfg(feature = "rusqlite")]
fn snapshot_dir(db_path: &Path) -> PathBuf {
    let parent = match db_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    parent.join(".migratr").join("snapshots")
}

/// Why migratr dev keeps a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevSnapshot {
    /// The local database dev is about to replace.
    Replaced,
    /// The local database dev made, recorded as the rewind point.
    RewindPoint,
}

impl DevSnapshot {
    fn as_str(self) -> &'static str {
        match self {
            DevSnapshot::Replaced => "kept",
            DevSnapshot::RewindPoint => "dev",
        }
    }
}

/// Copies the database at `db_path`, which no connection may hold open, among its snapshots,
/// named for the newest migration its ledger holds, and returns the copy's path. A rewind point
/// sorts after any other snapshot of the same second, and pruning never removes either kind.
#[cfg(feature = "rusqlite")]
pub fn snapshot_for_dev(db_path: &Path, kind: DevSnapshot) -> Result<PathBuf, MigrateError> {
    use crate::executor::RusqliteExecutor;

    let executor = |e: rusqlite::Error| MigrateError::Executor(Box::new(e));
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(executor)?;
    let version = RusqliteExecutor::new(conn)
        .read_ledger()
        .map_err(executor)?
        .iter()
        .map(|r| r.version)
        .max()
        .unwrap_or(0);
    let dir = snapshot_dir(db_path);
    let path = dir.join(format!(
        "{}_{version}_{}.db",
        timestamp(SystemTime::now()),
        kind.as_str()
    ));
    let failed = |source: io::Error| MigrateError::SnapshotFailed {
        path: path.clone(),
        source: Box::new(source),
    };
    fs::create_dir_all(&dir).map_err(failed)?;
    fs::copy(db_path, &path).map_err(failed)?;
    Ok(path)
}

/// The step word of a snapshot file name: up, down, kept or dev.
fn direction_of(path: &Path) -> &str {
    path.file_stem()
        .and_then(|n| n.to_str())
        .and_then(|n| n.rsplit('_').next())
        .unwrap_or_default()
}

/// Whether `name` is `<14 digits>_<digits>_<up|down|kept|dev>.db`, the names `snapshot_before`
/// and `snapshot_for_dev` give.
fn is_snapshot_name(name: &str) -> bool {
    let all_digits = |s: &str, len: Option<usize>| {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && len.is_none_or(|n| s.len() == n)
    };
    let Some(stem) = name.strip_suffix(".db") else {
        return false;
    };
    let mut parts = stem.split('_');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(time), Some(version), Some(direction), None) => {
            all_digits(time, Some(14))
                && all_digits(version, None)
                && matches!(direction, "up" | "down" | "kept" | "dev")
        }
        _ => false,
    }
}

/// The snapshot files of a snapshot directory, oldest first, a rewind point after any other of
/// the same second. Other files are not migratr's.
fn list_snapshots(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_snapshot_name)
        {
            found.push(path);
        }
    }
    found.sort_by(|a, b| {
        let key = |p: &PathBuf| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            (
                name.split('_').next().unwrap_or_default().to_string(),
                direction_of(p) == "dev",
                name.to_string(),
            )
        };
        key(a).cmp(&key(b))
    });
    Ok(found)
}

/// Deletes the oldest snapshots of steps so that `keep` remain, `newest` among them. The
/// snapshots migratr dev keeps are not a step's and are never deleted here.
fn prune(dir: &Path, newest: &Path, keep: usize) -> Result<(), MigrateError> {
    let failed = |path: &Path, source: io::Error| MigrateError::SnapshotFailed {
        path: path.to_path_buf(),
        source: Box::new(source),
    };
    let others: Vec<PathBuf> = list_snapshots(dir)
        .map_err(|e| failed(dir, e))?
        .into_iter()
        .filter(|p| p != newest && matches!(direction_of(p), "up" | "down"))
        .collect();
    let excess = others.len().saturating_sub(keep.saturating_sub(1));
    for old in &others[..excess] {
        fs::remove_file(old).map_err(|e| failed(old, e))?;
    }
    Ok(())
}

/// `YYYYMMDDHHMMSS` in UTC.
fn timestamp(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}{:02}{:02}{:02}",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamp_formats_known_instants() {
        assert_eq!(timestamp(UNIX_EPOCH), "19700101000000");
        // 2024-02-29 12:34:56 UTC
        let t = UNIX_EPOCH + Duration::from_secs(1_709_210_096);
        assert_eq!(timestamp(t), "20240229123456");
    }

    #[test]
    fn pruning_deletes_only_files_named_like_snapshots() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let old = dir.path().join("20260101000000_1_up.db");
        let newest = dir.path().join("20260101000001_2_up.db");
        let live = dir.path().join("app.db");
        for file in [&old, &newest, &live] {
            fs::write(file, "x").expect("write");
        }

        prune(dir.path(), &newest, 1).expect("prune");

        assert!(!old.exists());
        assert!(newest.exists());
        assert!(live.exists());
    }

    #[test]
    fn a_rewind_point_sorts_after_a_snapshot_of_the_same_second_and_is_never_pruned() {
        let dir = tempfile::TempDir::new().expect("tmp");
        let kept = dir.path().join("20260101000000_9_kept.db");
        let rewind = dir.path().join("20260101000000_1_dev.db");
        let old = dir.path().join("20260101000001_2_up.db");
        let newest = dir.path().join("20260101000002_3_down.db");
        for file in [&kept, &rewind, &old, &newest] {
            fs::write(file, "x").expect("write");
        }

        let listed = list_snapshots(dir.path()).expect("list");
        assert_eq!(
            listed,
            vec![kept.clone(), rewind.clone(), old.clone(), newest.clone()]
        );

        prune(dir.path(), &newest, 1).expect("prune");
        assert!(kept.exists() && rewind.exists() && newest.exists());
        assert!(!old.exists());
    }

    #[test]
    fn drops_and_raw_sql_are_destructive_and_additions_are_not() {
        let add = Op::CreateIndex {
            name: "i".into(),
            table: "t".into(),
            columns: vec![crate::migration::IndexColumn::Name("a".into())],
            unique: false,
            where_clause: None,
        };
        let raw = Op::RawSql {
            up: "SELECT 1".into(),
            down: None,
        };
        assert!(!is_destructive(std::slice::from_ref(&add)));
        assert!(is_destructive(&[add, raw]));
    }
}
