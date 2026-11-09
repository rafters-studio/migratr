//! `migratr dev`: the local database made from the place `smuggl` names, through the smugglr
//! binary, and recorded as the rewind point.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use migratr::{
    DevSnapshot, Note, RusqliteExecutor, has_keyword, load_dir, record_note, snapshot_for_dev, up,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::Outcome;
use crate::error::CliError;
use crate::settings::{FILE_NAME, Settings};

/// The first smugglr release with exec.
const SMUGGLR_WITH_EXEC: &str = "0.6.3";

/// The usage line of exec's read form, which only a smugglr with exec prints.
const EXEC_USAGE: &str = "smugglr exec <database-or-place> --read";

/// D1's own table, which is not the place's.
const D1_TABLE: &str = "_cf_KV";

/// migratr's ledger table.
const LEDGER_TABLE: &str = "_migratr_migrations";

/// Copies the place's tables, then its rows through `smugglr sync`, then its indexes, triggers
/// and views to a new database, and puts it in place of the local database `db`, keeping the
/// old one as a snapshot. The result is snapshotted, checked against the local database with
/// `smugglr sync --dryrun`, and recorded in the ledger as the rewind point, beside the place's
/// fingerprint. Nothing changes until smugglr has been found with exec and the place read, and
/// a place with an AUTOINCREMENT table is refused before anything changes. The rewind point is
/// recorded on the new database before it replaces the local one; only the pair's memory is made
/// after.
///
/// The first dev against a place with tables and no ledger adopts it when its shape, compared by
/// structure, is what the migrations in `dir` make, and records every migration; otherwise it
/// stops naming the difference. A later dev compares the place's fingerprint with the ledger's
/// and stops when they differ. Either stop changes nothing.
pub fn dev(db: &Path, dir: &Path, settings: &Settings) -> Result<Outcome, CliError> {
    let place = place(settings, "dev")?;
    let smugglr = Smugglr::find();
    // The read form on the local database is the check for exec; a local database not made yet
    // is checked by the read of the place, which dev needs anyway.
    let local = if db.exists() {
        Some(smugglr.read(db.as_os_str())?)
    } else {
        None
    };
    let read = smugglr.read(OsStr::new(place))?;
    let objects = objects(&read)?;
    refuse_autoincrement(&objects)?;
    let fingerprint = read["fingerprint"].as_str().unwrap_or_default().to_string();

    let local_ledger = local
        .as_ref()
        .and_then(|l| l["ledger"].as_array())
        .cloned()
        .unwrap_or_default();
    let recorded = recorded_fingerprint(&local_ledger);
    if let Some((recorded, _)) = &recorded
        && *recorded != fingerprint
    {
        return Err(CliError::ShapeChanged(format!(
            "the place's shape changed outside migratr: its fingerprint is {fingerprint}, and \
             the ledger recorded {recorded}; run migratr dev --discover to see why; nothing \
             changed"
        )));
    }
    let place_has_ledger = objects
        .iter()
        .any(|o| o.kind == "table" && o.name == LEDGER_TABLE);
    let (ledger, adopted) = if place_has_ledger {
        (read["ledger"].as_array().cloned().unwrap_or_default(), None)
    } else if let Some((_, versions)) = &recorded {
        // Discovered by an earlier dev: the migration rows the place held then are carried, not
        // adopted again. A migration applied locally since is not the place's and reads as
        // pending again.
        let carried = local_ledger
            .into_iter()
            .filter(|row| {
                row["version"]
                    .as_u64()
                    .is_some_and(|v| versions.contains(&v))
            })
            .collect();
        (carried, None)
    } else {
        match adopt(dir, &objects)? {
            Some(rows) => {
                let count = rows.len();
                (rows, Some(count))
            }
            None => (Vec::new(), None),
        }
    };

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
    let made = make(db, place, &smugglr, &work, &objects, &ledger, &fingerprint);
    // The local database has been replaced by now, so a work directory that cannot be removed
    // fails nothing: the next run clears it.
    let _ = fs::remove_dir_all(&work);
    let mut outcome = made?;
    outcome.fields["adopted"] = json!(adopted.is_some());
    if let Some(count) = adopted {
        outcome.text = format!(
            "adopted the place: its shape is what the migrations make; recorded {count} \
             migration(s)\n{}",
            outcome.text
        );
    }
    Ok(outcome)
}

/// `migratr dev --discover`: smugglr's --discover report of the local database and the place,
/// as JSON when `json`. Nothing changes on either side.
pub fn discover(db: &Path, settings: &Settings, json: bool) -> Result<Outcome, CliError> {
    let place = place(settings, "dev")?;
    Smugglr::find().discover(db.as_os_str(), OsStr::new(place), json)
}

/// Refuses a place with an AUTOINCREMENT table, naming each: migratr allows no auto columns.
fn refuse_autoincrement(objects: &[Object]) -> Result<(), CliError> {
    let tables = tables_with(objects, "AUTOINCREMENT");
    if tables.is_empty() {
        return Ok(());
    }
    Err(CliError::AutoIncrement(format!(
        "the place has AUTOINCREMENT in table {}, and migratr allows no auto columns; nothing \
         changed",
        tables.join(", table ")
    )))
}

/// The names of the tables whose statement has `keyword` as a bare word.
fn tables_with<'a>(objects: &'a [Object], keyword: &str) -> Vec<&'a str> {
    objects
        .iter()
        .filter(|o| {
            o.kind == "table"
                && o.sql
                    .as_deref()
                    .is_some_and(|sql| has_keyword(sql, keyword))
        })
        .map(|o| o.name.as_str())
        .collect()
}

pub(crate) fn place<'a>(settings: &'a Settings, command: &str) -> Result<&'a str, CliError> {
    settings.smuggl.as_deref().ok_or_else(|| {
        CliError::Usage(format!(
            "migratr {command} needs a place: set \"smuggl\" in {FILE_NAME} to a file path, an \
             https:// address or d1://<name>; nothing changed"
        ))
    })
}

/// The fingerprint an earlier dev recorded in the ledger `rows`, as the read form gives them,
/// with the versions of the migrations the place held when it was recorded.
fn recorded_fingerprint(rows: &[Value]) -> Option<(String, Vec<u64>)> {
    let row = rows
        .iter()
        .find(|r| r["version"].as_u64() == Some(Note::Fingerprint.version()))?;
    let note: Value = serde_json::from_str(row["checksum"].as_str()?).ok()?;
    let versions = note["migrations"]
        .as_array()
        .map(|vs| vs.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    note["fingerprint"]
        .as_str()
        .map(|f| (f.to_string(), versions))
}

/// The versions of the migration rows in `ledger`, leaving out notes.
pub(crate) fn migration_versions(ledger: &[Value]) -> Vec<u64> {
    ledger
        .iter()
        .filter_map(|row| row["version"].as_u64())
        .filter(|v| Note::ALL.iter().all(|n| n.version() != *v))
        .collect()
}

/// Adopts a place with tables and no ledger: the place's schema, replayed in memory, is compared
/// by structure with what the migrations in `dir` make. The same gives the ledger rows of every
/// migration; a difference stops, naming it. `None` when there is nothing to adopt: a place with
/// no tables, or no migrations to compare it with.
fn adopt(dir: &Path, objects: &[Object]) -> Result<Option<Vec<Value>>, CliError> {
    let has_tables = objects.iter().any(|o| {
        o.kind == "table"
            && o.name != D1_TABLE
            && !o.name.to_ascii_lowercase().starts_with("sqlite_")
    });
    if !has_tables || !dir.is_dir() {
        return Ok(None);
    }
    let migrations = load_dir(dir)?;
    if migrations.is_empty() {
        return Ok(None);
    }
    let mut made = RusqliteExecutor::new(Connection::open_in_memory()?);
    up(&mut made, &migrations, None)?;
    let place = Connection::open_in_memory()?;
    replay(&place, objects, |_| true)?;
    let differences = RusqliteExecutor::new(place).shape_differences(&made)?;
    if !differences.is_empty() {
        return Err(CliError::NotAdoptable(format!(
            "the place has tables and no migratr ledger, and its shape is not what the \
             migrations make: {}; nothing changed",
            differences.join("; ")
        )));
    }
    ledger_rows(made.connection()).map(Some)
}

/// The migration rows of `conn`'s ledger, each as an object of its columns.
fn ledger_rows(conn: &Connection) -> Result<Vec<Value>, CliError> {
    use rusqlite::types::ValueRef;
    let notes: Vec<String> = Note::ALL.iter().map(|n| n.version().to_string()).collect();
    let mut statement = conn.prepare(&format!(
        "SELECT * FROM {LEDGER_TABLE} WHERE version NOT IN ({}) ORDER BY version",
        notes.join(", ")
    ))?;
    let names: Vec<String> = statement.column_names().map(str::to_string).collect();
    let rows = statement.query_map([], |row| {
        let mut columns = serde_json::Map::new();
        for (at, name) in names.iter().enumerate() {
            let value = match row.get_ref(at)? {
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(n) => json!(n),
                ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                ValueRef::Null | ValueRef::Blob(_) => Value::Null,
            };
            columns.insert(name.clone(), value);
        }
        Ok(Value::Object(columns))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
    // A virtual table, such as a full-text table, has no primary key for smugglr to sync by: its
    // rows come through its shadow tables, and it is ignored.
    let virtual_tables = tables_with(objects, "VIRTUAL");
    copy_config(work, &virtual_tables)?;
    let staged = work.join("dev.db");
    // Tables first, rows second, then indexes, triggers and views, so no trigger fires on a
    // copied row.
    replay(&Connection::open(&staged)?, objects, |kind| kind == "table")?;
    smugglr.sync(OsStr::new(place), staged.as_os_str(), work, SyncRun::Bring)?;
    {
        // The note comes first: it makes the ledger when the place has none.
        let mut exec = RusqliteExecutor::new(Connection::open(&staged)?);
        record_note(
            &mut exec,
            Note::Fingerprint,
            &json!({
                "fingerprint": fingerprint,
                "migrations": migration_versions(ledger),
            })
            .to_string(),
        )?;
        copy_ledger(exec.connection(), ledger)?;
        replay(exec.connection(), objects, |kind| {
            matches!(kind, "index" | "trigger" | "view")
        })?;
    }

    // The rewind point is snapshotted, checked and recorded on the staged database, so only the
    // pair's memory, which needs the local database at its own path, is made after it is
    // replaced.
    let rewind = snapshot_for_dev(db, &staged, DevSnapshot::RewindPoint)?;
    let bytes = fs::read(&rewind).map_err(|source| CliError::Io {
        path: rewind.clone(),
        source,
    })?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    smugglr.sync(rewind.as_os_str(), staged.as_os_str(), work, SyncRun::Check)?;
    let point = json!({ "snapshot": rewind, "sha256": sha256 });
    {
        let mut exec = RusqliteExecutor::new(Connection::open(&staged)?);
        record_note(&mut exec, Note::RewindPoint, &point.to_string())?;
    }

    let kept = if db.exists() {
        Some(snapshot_for_dev(db, db, DevSnapshot::Replaced)?)
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
    remember(db, place, smugglr, &virtual_tables)?;

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

/// Syncs the local database `db`, just made from the place and equal to it, with the place once,
/// so the pair's memory, kept in [`pair_dir`], holds the rows the rewind point holds. up --smuggl
/// syncs the rewind point, put back at `db`'s own path, with that memory, so the place's changes
/// since dev arrive, deletes included. The memory starts empty.
fn remember(db: &Path, place: &str, smugglr: &Smugglr, ignored: &[&str]) -> Result<(), CliError> {
    let pair = pair_dir(db);
    let fresh = |e: io::Error| CliError::Io {
        path: pair.clone(),
        source: e,
    };
    if pair.exists() {
        fs::remove_dir_all(&pair).map_err(fresh)?;
    }
    fs::create_dir_all(&pair).map_err(fresh)?;
    copy_config(&pair, ignored)?;
    smugglr.sync(
        address(db)?.as_os_str(),
        OsStr::new(place),
        &pair,
        SyncRun::Sync,
    )
}

/// The local database's address as smugglr is given it whenever the pair's memory is used: the
/// memory is kept by the two addresses.
pub(crate) fn address(db: &Path) -> Result<PathBuf, CliError> {
    fs::canonicalize(db).map_err(|source| CliError::Io {
        path: db.to_path_buf(),
        source,
    })
}

/// Where the memory of the pair of the local database `db` and the place is kept, as smugglr's
/// config home: beside migratr's snapshots.
pub(crate) fn pair_dir(db: &Path) -> PathBuf {
    migratr_dir(db).join("pair")
}

/// Writes migration rows, every column as the read form gives it, into the staged ledger: sync
/// leaves the ledger out. Notes are not copied.
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
    migratr_dir(db).join("dev")
}

/// migratr's directory beside the database `db`.
pub(crate) fn migratr_dir(db: &Path) -> PathBuf {
    let parent = match db.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    parent.join(".migratr")
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
/// D1's own table are not the place's and are skipped, and so is one the replay has already
/// made, such as a full-text table's shadow tables.
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
        let made: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = ?1)",
            [&object.name],
            |row| row.get(0),
        )?;
        if let Some(sql) = object.sql.as_ref().filter(|_| !made) {
            tx.execute_batch(sql)?;
        }
    }
    tx.commit()
}

/// Copies smugglr's config file, which holds its ignore list, into `work`, where dev's syncs
/// read it, with `ignored` added to the list. Their memory then stays in `work` and goes with
/// it: a remembered pair would take a new, empty database for rows deleted and delete them from
/// the place.
pub(crate) fn copy_config(work: &Path, ignored: &[&str]) -> Result<(), CliError> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let source = base
        .map(|b| b.join("smugglr").join("config.toml"))
        .filter(|s| s.is_file());
    let text = match &source {
        Some(path) => fs::read_to_string(path).map_err(|e| CliError::Io {
            path: path.clone(),
            source: e,
        })?,
        None if ignored.is_empty() => return Ok(()),
        None => String::new(),
    };
    let dir = work.join("smugglr");
    let target = dir.join("config.toml");
    fs::create_dir_all(&dir)
        .and_then(|()| fs::write(&target, with_ignored(&text, ignored)))
        .map_err(|e| CliError::Io {
            path: target,
            source: e,
        })
}

/// The config `text` with `names` added to its top-level ignore list, which is made when it has
/// none.
fn with_ignored(text: &str, names: &[&str]) -> String {
    if names.is_empty() {
        return text.to_string();
    }
    let quoted: Vec<String> = names
        .iter()
        .map(|n| format!("\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    let quoted = quoted.join(", ");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    for line in &mut lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            // A table header: the top level, where ignore lives, has ended.
            break;
        }
        let is_ignore = trimmed
            .strip_prefix("ignore")
            .is_some_and(|rest| rest.trim_start().starts_with('='));
        if let Some(open) = line.find('[').filter(|_| is_ignore) {
            line.insert_str(open + 1, &format!("{quoted}, "));
            return lines.join("\n") + "\n";
        }
    }
    format!("ignore = [{quoted}]\n{text}")
}

/// The smugglr binary: the one SMUGGLR names, else `smugglr` on PATH.
pub(crate) struct Smugglr {
    bin: OsString,
}

impl Smugglr {
    pub(crate) fn find() -> Self {
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
    pub(crate) fn read(&self, target: &OsStr) -> Result<Value, CliError> {
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

    /// `smugglr exec local place --version v --up|--down --expect fingerprint`, as `way` is "up"
    /// or "down", the statements `batch` on stdin: smugglr runs them on the place only when their
    /// hash is the one recorded for `version` (an up's in the local ledger, a down's in the
    /// place's) and the place's fingerprint is `expect`. Returns the place's fingerprint after
    /// them.
    pub(crate) fn exec(
        &self,
        local: &OsStr,
        place: &OsStr,
        version: u64,
        way: &str,
        expect: &str,
        batch: &[String],
    ) -> Result<String, CliError> {
        use std::io::Write as _;
        use std::process::Stdio;

        let mut child = Command::new(&self.bin)
            .arg("exec")
            .arg(local)
            .arg(place)
            .args([
                "--version",
                &version.to_string(),
                &format!("--{way}"),
                "--expect",
                expect,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| self.missing(&format!("could not be run: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(json!(batch).to_string().as_bytes())
                .map_err(|e| CliError::Smugglr(format!("smugglr exec: {e}")))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|e| CliError::Smugglr(format!("smugglr exec: {e}")))?;
        let said: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        if output.status.success()
            && let Some(fingerprint) = said["fingerprint"].as_str()
        {
            return Ok(fingerprint.to_string());
        }
        let why = if let Some(detail) = said["detail"].as_str() {
            format!(
                "refused ({}): {detail}",
                said["refused"].as_str().unwrap_or_default()
            )
        } else if let Some(error) = said["error"].as_str() {
            format!(
                "failed at {:?}: {error}",
                said["failed"].as_str().unwrap_or_default()
            )
        } else {
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            )
        };
        Err(CliError::Smugglr(format!(
            "smugglr exec of version {version} on {}: {why}",
            Path::new(place).display()
        )))
    }

    /// `smugglr sync local place --discover`: its report, which changes nothing. Exit 3 is a
    /// report too: a sync would stop.
    fn discover(&self, local: &OsStr, place: &OsStr, json: bool) -> Result<Outcome, CliError> {
        let (_, report) = self.report(local, place, json)?;
        let value = if json {
            serde_json::from_str(&report).ok()
        } else {
            Some(Value::Null)
        };
        match value {
            Some(value) => Ok(Outcome {
                fields: json!({ "discover": value }),
                text: report,
            }),
            None => Err(CliError::Smugglr(format!(
                "smugglr sync {} {} --discover --json gave no JSON: {report}",
                Path::new(local).display(),
                Path::new(place).display(),
            ))),
        }
    }

    /// `smugglr sync local place --discover`, with `--json` when `json`: whether a sync would
    /// stop (its exit 3), and its report. Nothing changes on either side.
    pub(crate) fn report(
        &self,
        local: &OsStr,
        place: &OsStr,
        json: bool,
    ) -> Result<(bool, String), CliError> {
        let mut command = Command::new(&self.bin);
        command.arg("sync").arg(local).arg(place).arg("--discover");
        if json {
            command.arg("--json");
        }
        let output = command
            .output()
            .map_err(|e| self.missing(&format!("could not be run: {e}")))?;
        let report = String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string();
        match output.status.code() {
            Some(code @ (0 | 3)) => Ok((code == 3, report)),
            _ => Err(CliError::Smugglr(format!(
                "smugglr sync {} {} --discover: {}{}",
                Path::new(local).display(),
                Path::new(place).display(),
                report,
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    /// `smugglr sync source target`, reading its config and keeping its memory in `work`, run
    /// as `run` says.
    pub(crate) fn sync(
        &self,
        source: &OsStr,
        target: &OsStr,
        work: &Path,
        run: SyncRun,
    ) -> Result<(), CliError> {
        let dryrun = run == SyncRun::Check;
        let mut command = Command::new(&self.bin);
        command.arg("sync").arg(source).arg(target).arg("--json");
        match run {
            SyncRun::Check => {
                command.arg("--dryrun");
            }
            SyncRun::Bring => {
                command.args(["--sync-mode", "source"]);
            }
            SyncRun::Sync => {}
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

/// How a smugglr sync is run.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncRun {
    /// A dry run, which must move nothing.
    Check,
    /// The source's copy of a row changed in both places is kept: dev's first copy, whose
    /// target holds only the rows its replay made, such as a full-text table's shadow rows.
    Bring,
    /// smugglr's default: a row changed in both places stops the sync.
    Sync,
}

/// Whether a dry run's record has every table in step.
fn moves_nothing(output: &Output) -> bool {
    serde_json::from_slice::<Value>(&output.stdout).is_ok_and(|record| {
        record["tables"]
            .as_object()
            .is_some_and(|tables| tables.values().all(|t| t["status"] == "in step"))
    })
}

#[cfg(test)]
mod tests {
    use super::with_ignored;

    #[test]
    fn ignored_names_join_an_ignore_list_or_make_one() {
        assert_eq!(
            with_ignored("ignore = [\"b\"]\n[mesh]\nport = 1\n", &["docs"]),
            "ignore = [\"docs\", \"b\"]\n[mesh]\nport = 1\n"
        );
        assert_eq!(
            with_ignored("[mesh]\nignore = [\"x\"]\n", &["docs", "notes"]),
            "ignore = [\"docs\", \"notes\"]\n[mesh]\nignore = [\"x\"]\n"
        );
        assert_eq!(with_ignored("", &["docs"]), "ignore = [\"docs\"]\n");
        assert_eq!(
            with_ignored("steering = \"t\"\n", &[]),
            "steering = \"t\"\n"
        );
    }
}
