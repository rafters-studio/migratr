//! `migratr dev`: the local database made from the place `smuggl` names, through the smugglr
//! binary, and recorded as the rewind point.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use migratr::{DevSnapshot, Note, RusqliteExecutor, record_note, snapshot_for_dev};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::CliError;
use crate::settings::{FILE_NAME, Settings};
use crate::{Outcome, has_ledger, open_existing};

/// The first smugglr release with exec.
const SMUGGLR_WITH_EXEC: &str = "0.6.3";

/// The usage line of exec's read form, which only a smugglr with exec prints.
const EXEC_USAGE: &str = "smugglr exec <database-or-place> --read";

/// D1's own table, which is not the place's.
const D1_TABLE: &str = "_cf_KV";

/// Copies the place's tables, then its rows through `smugglr sync`, then its indexes, triggers
/// and views to a new database, and puts it in place of the local database `db`, keeping the
/// old one as a snapshot. The result is snapshotted, checked against the local database with
/// `smugglr sync --dryrun`, and recorded in the ledger as the rewind point, beside the place's
/// fingerprint. Nothing changes until smugglr has been found with exec and the place read.
pub fn dev(db: &Path, settings: &Settings) -> Result<Outcome, CliError> {
    let place = settings.smuggl.as_deref().ok_or_else(|| {
        CliError::Usage(format!(
            "migratr dev needs a place: set \"smuggl\" in {FILE_NAME} to a file path, an \
             https:// address or d1://<name>; nothing changed"
        ))
    })?;
    let smugglr = Smugglr::find();
    // The read form on the local database is the check for exec; a local database not made yet
    // is checked by the read of the place, which dev needs anyway.
    if db.exists() {
        smugglr.read(db.as_os_str())?;
    }
    let read = smugglr.read(OsStr::new(place))?;
    let objects = objects(&read)?;
    let fingerprint = read["fingerprint"].as_str().unwrap_or_default().to_string();

    let work = work_dir(db);
    let fresh = |e: io::Error| CliError::Io {
        path: work.clone(),
        source: e,
    };
    if work.exists() {
        // Left by a run that stopped; nothing in it is kept.
        fs::remove_dir_all(&work).map_err(fresh)?;
    }
    fs::create_dir_all(&work).map_err(fresh)?;
    let ledger = read["ledger"].as_array().cloned().unwrap_or_default();
    let made = make(db, place, &smugglr, &work, &objects, &ledger, &fingerprint);
    let removed = fs::remove_dir_all(&work).map_err(fresh);
    let outcome = made?;
    removed?;
    Ok(outcome)
}

/// Whether the ledger of `db` holds the rewind point dev records. A database that is not named,
/// does not exist or has no ledger holds none.
pub fn has_rewind_point(db: Option<&Path>) -> Result<bool, CliError> {
    let Some(conn) = db.map(open_existing).transpose()?.flatten() else {
        return Ok(false);
    };
    if !has_ledger(&conn)? {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT count(*) > 0 FROM _migratr_migrations WHERE version = ?1",
        [i64::try_from(Note::RewindPoint.version()).unwrap_or(i64::MAX)],
        |row| row.get(0),
    )?)
}

fn make(
    db: &Path,
    place: &str,
    smugglr: &Smugglr,
    work: &Path,
    objects: &[Object],
    ledger: &[Value],
    fingerprint: &str,
) -> Result<Outcome, CliError> {
    copy_config(work)?;
    let staged = work.join("dev.db");
    // Tables first, rows second, then indexes, triggers and views, so no trigger fires on a
    // copied row.
    replay(&Connection::open(&staged)?, objects, |kind| kind == "table")?;
    smugglr.sync(OsStr::new(place), staged.as_os_str(), work, false)?;
    {
        let conn = Connection::open(&staged)?;
        copy_ledger(&conn, ledger)?;
        replay(&conn, objects, |kind| {
            matches!(kind, "index" | "trigger" | "view")
        })?;
        let mut exec = RusqliteExecutor::new(conn);
        record_note(
            &mut exec,
            Note::Fingerprint,
            &json!({ "fingerprint": fingerprint }).to_string(),
        )?;
    }

    let kept = if db.exists() {
        Some(snapshot_for_dev(db, DevSnapshot::Replaced)?)
    } else {
        None
    };
    // Journal files of the replaced database do not belong to the new one.
    for suffix in ["-wal", "-shm"] {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        let path = PathBuf::from(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(CliError::Io { path, source }),
        }
    }
    fs::rename(&staged, db).map_err(|source| CliError::Io {
        path: db.to_path_buf(),
        source,
    })?;

    let rewind = snapshot_for_dev(db, DevSnapshot::RewindPoint)?;
    let bytes = fs::read(&rewind).map_err(|source| CliError::Io {
        path: rewind.clone(),
        source,
    })?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    smugglr.sync(rewind.as_os_str(), db.as_os_str(), work, true)?;
    let point = json!({ "snapshot": rewind, "sha256": sha256 });
    let mut exec = RusqliteExecutor::new(Connection::open(db)?);
    record_note(&mut exec, Note::RewindPoint, &point.to_string())?;

    let mut lines = Vec::new();
    if let Some(path) = &kept {
        lines.push(format!("kept the old local database as {}", path.display()));
    }
    lines.push(format!("rewind point: {}", rewind.display()));
    lines.push("Dev ready".to_string());
    Ok(Outcome {
        fields: json!({
            "kept": kept,
            "rewind_point": point,
            "fingerprint": fingerprint,
        }),
        text: lines.join("\n"),
    })
}

/// Writes the place's migration rows, every column as the read form gives it, into the staged
/// ledger, which the replay made from the place's own: sync leaves the ledger out. The place's
/// notes are its own and are not copied.
fn copy_ledger(conn: &Connection, ledger: &[Value]) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for row in ledger {
        let Some(columns) = row.as_object() else {
            continue;
        };
        let note = columns
            .get("version")
            .and_then(Value::as_u64)
            .is_some_and(|v| Note::ALL.iter().any(|n| n.version() == v));
        if note {
            continue;
        }
        let names: Vec<String> = columns
            .keys()
            .map(|k| format!("\"{}\"", k.replace('"', "\"\"")))
            .collect();
        let values: Vec<rusqlite::types::Value> = columns.values().map(sql_value).collect();
        let marks = vec!["?"; values.len()].join(", ");
        tx.execute(
            &format!(
                "INSERT INTO _migratr_migrations ({}) VALUES ({marks})",
                names.join(", ")
            ),
            rusqlite::params_from_iter(values),
        )?;
    }
    tx.commit()
}

fn sql_value(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match value {
        Value::Null => Sql::Null,
        Value::Number(n) => n
            .as_i64()
            .map(Sql::Integer)
            .or_else(|| n.as_f64().map(Sql::Real))
            .unwrap_or(Sql::Null),
        Value::String(s) => Sql::Text(s.clone()),
        Value::Bool(b) => Sql::Integer(i64::from(*b)),
        other => Sql::Text(other.to_string()),
    }
}

/// Where dev builds the new database: beside migratr's snapshots, so it is renamed into place
/// on the same filesystem.
fn work_dir(db: &Path) -> PathBuf {
    let parent = match db.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    parent.join(".migratr").join("dev")
}

/// One row of the place's `sqlite_master`, as the read form gives it.
struct Object {
    kind: String,
    name: String,
    tbl_name: String,
    sql: Option<String>,
}

fn objects(read: &Value) -> Result<Vec<Object>, CliError> {
    let malformed = || CliError::Smugglr("smugglr exec --read gave no schema".to_string());
    let text = |row: &Value, key: &str| row[key].as_str().map(str::to_string);
    read["schema"]
        .as_array()
        .ok_or_else(malformed)?
        .iter()
        .map(|row| {
            Ok(Object {
                kind: text(row, "type").ok_or_else(malformed)?,
                name: text(row, "name").ok_or_else(malformed)?,
                tbl_name: text(row, "tbl_name").ok_or_else(malformed)?,
                sql: text(row, "sql"),
            })
        })
        .collect()
}

/// Runs, in one transaction and in the place's order, the place's own statement of each object
/// whose type `wanted` takes. An object with no statement, such as a `sqlite_autoindex`, and
/// D1's own table are not the place's and are skipped.
fn replay(
    conn: &Connection,
    objects: &[Object],
    wanted: impl Fn(&str) -> bool,
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for object in objects {
        if !wanted(&object.kind) || object.name == D1_TABLE || object.tbl_name == D1_TABLE {
            continue;
        }
        if let Some(sql) = &object.sql {
            tx.execute_batch(sql)?;
        }
    }
    tx.commit()
}

/// Copies smugglr's config file, which holds its ignore list, into `work`, where dev's syncs
/// read it. Their memory then stays in `work` and goes with it: a remembered pair would take a
/// new, empty database for rows deleted and delete them from the place.
fn copy_config(work: &Path) -> Result<(), CliError> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let Some(source) = base.map(|b| b.join("smugglr").join("config.toml")) else {
        return Ok(());
    };
    if !source.is_file() {
        return Ok(());
    }
    let dir = work.join("smugglr");
    let target = dir.join("config.toml");
    fs::create_dir_all(&dir)
        .and_then(|()| fs::copy(&source, &target))
        .map(|_| ())
        .map_err(|e| CliError::Io {
            path: target,
            source: e,
        })
}

/// The smugglr binary: the one SMUGGLR names, else `smugglr` on PATH.
struct Smugglr {
    bin: OsString,
}

impl Smugglr {
    fn find() -> Self {
        Self {
            bin: std::env::var_os("SMUGGLR")
                .filter(|bin| !bin.is_empty())
                .unwrap_or_else(|| OsString::from("smugglr")),
        }
    }

    fn missing(&self, why: &str) -> CliError {
        CliError::NoSmugglr(format!(
            "migratr dev needs smugglr with exec, and {} {why}; install smugglr \
             {SMUGGLR_WITH_EXEC} or later, or name its binary in SMUGGLR; nothing changed",
            Path::new(&self.bin).display()
        ))
    }

    /// The read form of exec on `target`: its schema, ledger and fingerprint.
    fn read(&self, target: &OsStr) -> Result<Value, CliError> {
        let output = Command::new(&self.bin)
            .arg("exec")
            .arg(target)
            .arg("--read")
            .output()
            .map_err(|e| self.missing(&format!("could not be run: {e}")))?;
        if output.status.success()
            && let Ok(value) = serde_json::from_slice::<Value>(&output.stdout)
            && value["fingerprint"].is_string()
        {
            return Ok(value);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains(EXEC_USAGE) {
            let said: Vec<&str> = stderr
                .lines()
                .take_while(|l| !l.starts_with("usage:"))
                .collect();
            return Err(CliError::Smugglr(format!(
                "smugglr exec {} --read: {}",
                Path::new(target).display(),
                said.join("; ")
            )));
        }
        Err(self.missing("has no exec"))
    }

    /// `smugglr sync source target`, reading its config and keeping its memory in `work`. A
    /// dry run must move nothing.
    fn sync(
        &self,
        source: &OsStr,
        target: &OsStr,
        work: &Path,
        dryrun: bool,
    ) -> Result<(), CliError> {
        let mut command = Command::new(&self.bin);
        command.arg("sync").arg(source).arg(target).arg("--json");
        if dryrun {
            command.arg("--dryrun");
        }
        let output = command
            .env("XDG_CONFIG_HOME", work)
            .output()
            .map_err(|e| self.missing(&format!("could not be run: {e}")))?;
        if output.status.success() && (!dryrun || moves_nothing(&output)) {
            return Ok(());
        }
        let what = if dryrun {
            format!(
                "the rewind point {} does not equal the local database {}",
                Path::new(source).display(),
                Path::new(target).display()
            )
        } else {
            format!("smugglr sync from {}", Path::new(source).display())
        };
        Err(CliError::Smugglr(format!(
            "{what}: {}{}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

/// Whether a dry run's record has every table in step.
fn moves_nothing(output: &Output) -> bool {
    serde_json::from_slice::<Value>(&output.stdout).is_ok_and(|record| {
        record["tables"]
            .as_object()
            .is_some_and(|tables| tables.values().all(|t| t["status"] == "in step"))
    })
}
