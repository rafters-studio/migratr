//! `migratr up --smuggl` and `migratr down --smuggl`: the migrations the run applies or reverts
//! on the local database, shipped to the place `smuggl` names through the rewind point migratr
//! dev recorded and smugglr exec.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use migratr::{
    DevSnapshot, Executor, Migration, Note, Op, RusqliteExecutor, down, load_dir, record_note,
    restore, snapshot_for_dev, up, write_schema,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::dev::{self, Smugglr};
use crate::error::CliError;
use crate::settings::{Loud, Settings};
use crate::{Direction, Outcome, loud, plan};

/// migratr's ledger table.
const LEDGER_TABLE: &str = "_migratr_migrations";

/// The two notes migratr dev wrote, as their JSON.
struct Notes {
    rewind: String,
    fingerprint: String,
}

/// The rewind point a note names.
struct RewindPoint {
    snapshot: PathBuf,
    sha256: String,
}

/// What a ship sent and where it left the place.
struct Shipped {
    versions: Vec<u64>,
    fingerprint: String,
}

/// Ships the migrations the run applies (up, to `to`) or reverts (down, `steps` of them) to the
/// place. It first compares the place's fingerprint with the one the ledger recorded and stops,
/// changing nothing, when they differ. It then snapshots the working database, puts the rewind
/// point back at the local database's own path with dev's two notes, syncs it with the place so
/// the place's changes since dev arrive, runs the migrations on it one at a time, and sends each
/// one's statements, as `migratr plan` gives them, through smugglr exec. The migrations a run
/// that failed partway already took to the place are run locally before the sync and not sent
/// again, and the stop expects the place's fingerprint after them. The place must end
/// with the local database's fingerprint, which the ledger then records. On success with
/// autoclean on, the rewind point, its ledger row and the working database's snapshot are
/// removed. On any failure the local database is put back at the rewind point with both notes
/// and nothing is removed, so the same run can be tried again.
pub fn smuggl(
    db: &Path,
    dir: &Path,
    settings: &Settings,
    direction: Direction,
) -> Result<Outcome, CliError> {
    let command = direction.name();
    let notes = notes(db, command)?;
    let place = dev::place(settings, &format!("{command} --smuggl"))?;
    let point = rewind_point(&notes.rewind)?;
    if !point_is_recorded(&point)? {
        return Err(CliError::ShipFailed(format!(
            "the rewind point {} is not the file migratr dev recorded: its SHA-256 differs; run \
             migratr dev again; nothing changed",
            point.snapshot.display()
        )));
    }
    let migrations = load_dir(dir)?;
    let smugglr = Smugglr::find();
    let there = smugglr.read(OsStr::new(place))?;
    let found = fingerprint(&there);
    let expected = expected(db, &point, &notes, &migrations, direction, &there, &smugglr)?;
    if found != expected.fingerprint {
        return Err(CliError::ShapeChanged(format!(
            "the place's shape changed outside migratr: its fingerprint is {found}, and the \
             ledger recorded {}; run migratr {command} --smuggl --discover to see why; nothing \
             changed",
            expected.fingerprint
        )));
    }
    let catch_up = expected.catch_up;
    drop(expected);

    // The run syncs with a copy of the pair's memory, which is never kept: every run starts by
    // putting the rewind point back, so the memory must stay the rewind point's.
    let pair = dev::pair_dir(db);
    let work = dev::migratr_dir(db).join("smuggl");
    let at = |path: &Path| {
        let path = path.to_path_buf();
        move |source: io::Error| CliError::Io { path, source }
    };
    if work.exists() {
        // Left by a run that stopped; nothing in it is kept.
        fs::remove_dir_all(&work).map_err(at(&work))?;
    }
    copy_tree(&pair, &work)?;
    dev::copy_config(&work)?;

    let kept = snapshot_for_dev(db, DevSnapshot::Replaced)?;
    let mut lines = vec![format!("kept the working database as {}", kept.display())];
    let shipped = match ship(
        db,
        dir,
        place,
        &smugglr,
        &work,
        &migrations,
        direction,
        catch_up,
        &notes,
        &point,
        settings,
    ) {
        Ok(shipped) => shipped,
        Err(error) => {
            let back = put_back(db, &point, &notes);
            // Best effort: the memory copy is the run's and is not kept.
            let _ = fs::remove_dir_all(&work);
            let back = match back {
                Ok(()) => format!(
                    "the local database is back at the rewind point {}, with its rewind-point \
                     and fingerprint rows",
                    point.snapshot.display()
                ),
                Err(e) => format!(
                    "putting the rewind point {} back at the local database also failed: {e}",
                    point.snapshot.display()
                ),
            };
            return Err(CliError::ShipFailed(format!(
                "{error}; {back}; nothing was removed: the rewind point and the working \
                 database's snapshot {} remain; run migratr {command} --smuggl again",
                kept.display()
            )));
        }
    };
    fs::remove_dir_all(&work).map_err(at(&work))?;

    lines.push(format!(
        "restored the rewind point {} at {} and synced it with the place",
        point.snapshot.display(),
        db.display()
    ));
    lines.push(format!(
        "shipped {} migration(s){}",
        shipped.versions.len(),
        crate::version_list(&shipped.versions)
    ));
    lines.push(format!("the place's fingerprint: {}", shipped.fingerprint));
    let mut removed = Vec::new();
    if settings.autoclean {
        fs::remove_file(&point.snapshot).map_err(at(&point.snapshot))?;
        lines.push(format!(
            "removed the rewind point {}",
            point.snapshot.display()
        ));
        Connection::open(db)?.execute(
            &format!("DELETE FROM {LEDGER_TABLE} WHERE version = ?1"),
            [Note::RewindPoint.version().to_string()],
        )?;
        lines.push("removed the rewind point's ledger row".to_string());
        fs::remove_file(&kept).map_err(at(&kept))?;
        lines.push(format!(
            "removed the working database's snapshot {}",
            kept.display()
        ));
        removed.push(point.snapshot.clone());
        removed.push(kept.clone());
    } else {
        lines.push(format!(
            "autoclean is off: kept the rewind point {} and the working database's snapshot {}",
            point.snapshot.display(),
            kept.display()
        ));
    }
    lines.push("done".to_string());
    Ok(Outcome {
        fields: json!({
            "shipped": shipped.versions,
            "fingerprint": shipped.fingerprint,
            "kept": kept,
            "rewind_point": point.snapshot,
            "removed": removed,
        }),
        text: lines.join("\n"),
    })
}

/// `migratr up --smuggl --discover` and `migratr down --smuggl --discover`: smugglr's --discover
/// report of the rewind point and the place, the migrations the run would send with their
/// statements, and the tables and rows each one that drops a table or a column would destroy on
/// the place. Nothing changes anywhere. `would_stop` says whether the run would stop before
/// changing anything, which exits 3.
pub fn discover(
    db: &Path,
    dir: &Path,
    settings: &Settings,
    direction: Direction,
    json: bool,
) -> Result<Outcome, CliError> {
    let command = direction.name();
    let place = dev::place(settings, &format!("{command} --smuggl"))?;
    let notes = match notes(db, command) {
        Ok(notes) => notes,
        Err(CliError::NoRewindPoint(why)) => return Ok(would_stop(&[why])),
        Err(error) => return Err(error),
    };
    let point = match rewind_point(&notes.rewind) {
        Ok(point) => point,
        Err(CliError::NoRewindPoint(why)) => return Ok(would_stop(&[why])),
        Err(error) => return Err(error),
    };
    if !point.snapshot.is_file() || !point_is_recorded(&point)? {
        return Ok(would_stop(&[format!(
            "the rewind point {} is not the file migratr dev recorded; run migratr dev again",
            point.snapshot.display()
        )]));
    }
    let smugglr = Smugglr::find();
    let migrations = load_dir(dir)?;
    let mut stops = Vec::new();
    let there = smugglr.read(OsStr::new(place))?;
    let found = fingerprint(&there);
    let expected = expected(db, &point, &notes, &migrations, direction, &there, &smugglr)?;
    if found != expected.fingerprint {
        stops.push(format!(
            "the place's shape changed outside migratr: its fingerprint is {found}, and the \
             ledger recorded {}",
            expected.fingerprint
        ));
    }
    let (sync_stops, report) =
        smugglr.report(point.snapshot.as_os_str(), OsStr::new(place), true)?;
    let report: Value = serde_json::from_str(&report).unwrap_or(Value::Null);
    if sync_stops {
        stops.push("smugglr's --discover says a sync with the place would stop".to_string());
    }
    let text_report = if json {
        String::new()
    } else {
        smugglr
            .report(point.snapshot.as_os_str(), OsStr::new(place), false)?
            .1
    };

    // The run puts the rewind point back as the local database, with what a run that failed
    // partway took to the place caught up on it, so it plans against that.
    let planned = {
        let local = expected.copy.as_ref().map_or(&point.snapshot, |copy| &copy.db);
        let conn = Connection::open_with_flags(local, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let bytes = fs::metadata(local).map_or(0, |m| m.len());
        match plan::plan(&conn, bytes, &migrations, expected.catch_up.after(direction)) {
            Ok(planned) => planned,
            Err(error) => {
                stops.push(error.to_string());
                json!({ "direction": command, "steps": [] })
            }
        }
    };
    let mut planned = planned;
    // What exec is sent: each migration's own statements, as smugglr writes the ledger row.
    for step in planned["steps"].as_array_mut().into_iter().flatten() {
        let statements: Vec<String> = step["statements"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect();
        step["statements"] = json!(operations(&statements));
    }
    let rows = |table: &str| {
        report["target"]["tables"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["name"] == table)
            .and_then(|t| t["rows"].as_u64())
    };
    let mut destroys = Vec::new();
    for step in planned["steps"].as_array().into_iter().flatten() {
        let Some(migration) = migrations
            .iter()
            .find(|m| step["version"].as_u64() == Some(m.version))
        else {
            continue;
        };
        for (table, column) in drops(migration, direction) {
            destroys.push(json!({
                "version": migration.version,
                "table": table,
                "column": column,
                "rows": rows(&table),
            }));
        }
    }

    let mut lines = vec![text_report];
    lines.push(format!("migratr {command} --smuggl would send:"));
    lines.extend(
        plan::render(&planned)
            .lines()
            .skip(1)
            .map(|l| format!("  {l}")),
    );
    for destroy in &destroys {
        let rows = destroy["rows"]
            .as_u64()
            .map_or_else(|| "rows unknown".to_string(), |n| format!("{n} rows"));
        let table = destroy["table"].as_str().unwrap_or_default();
        let what = match destroy["column"].as_str() {
            Some(column) => format!("column {column} of table {table}"),
            None => format!("table {table}"),
        };
        lines.push(format!(
            "{} destroys {what} on the place: {rows}",
            destroy["version"]
        ));
    }
    let mut outcome = would_stop(&stops);
    lines.push(outcome.text);
    outcome.text = lines.join("\n").trim_start().to_string();
    outcome.fields["discover"] = report;
    outcome.fields["steps"] = planned["steps"].clone();
    outcome.fields["destroys"] = json!(destroys);
    Ok(outcome)
}

/// A --discover outcome that says the run would stop for each of `stops`, or that it could go
/// ahead when there are none.
fn would_stop(stops: &[String]) -> Outcome {
    let text = if stops.is_empty() {
        "the run could go ahead".to_string()
    } else {
        format!("the run would stop: {}", stops.join("; "))
    };
    Outcome {
        fields: json!({ "would_stop": !stops.is_empty(), "stops": stops }),
        text,
    }
}

/// The tables, and columns with their table, that running `migration` in `direction` drops.
fn drops(migration: &Migration, direction: Direction) -> Vec<(String, Option<String>)> {
    migration
        .up
        .iter()
        .filter_map(|op| match (direction, op) {
            (Direction::Up { .. }, Op::DropTable { table, .. })
            | (Direction::Down { .. }, Op::CreateTable { table, .. }) => {
                Some((table.clone(), None))
            }
            (Direction::Up { .. }, Op::DropColumn { table, column })
            | (Direction::Down { .. }, Op::AddColumn { table, column }) => {
                Some((table.clone(), Some(column.name.clone())))
            }
            _ => None,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn ship(
    db: &Path,
    dir: &Path,
    place: &str,
    smugglr: &Smugglr,
    work: &Path,
    migrations: &[Migration],
    direction: Direction,
    catch_up: CatchUp,
    notes: &Notes,
    point: &RewindPoint,
    settings: &Settings,
) -> Result<Shipped, CliError> {
    put_back(db, point, notes)?;
    let mut expect = recorded_fingerprint(notes);

    // A run that failed partway already took some of its migrations to the place: they run
    // locally first, so both have one shape for the sync, and are not sent again.
    if catch_up != CatchUp::Nothing {
        let mut exec = loud(settings, Connection::open(db)?);
        catch_up.run(&mut exec, migrations)?;
        write_schema(&mut exec, dir)?;
        drop(exec);
        expect = fingerprint(&smugglr.read(db.as_os_str())?);
    }

    let local = dev::address(db)?;
    smugglr.sync(local.as_os_str(), OsStr::new(place), work, false)?;

    let planned = {
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let bytes = fs::metadata(db).map_or(0, |m| m.len());
        plan::plan(&conn, bytes, migrations, catch_up.after(direction))?
    };
    let mut versions = Vec::new();
    for step in planned["steps"].as_array().into_iter().flatten() {
        let version = step["version"]
            .as_u64()
            .ok_or_else(|| CliError::Smugglr("migratr plan gave a step with no version".into()))?;
        let statements: Vec<String> = step["statements"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect();
        let batch = operations(&statements);

        let hash = format!("{:x}", Sha256::digest(batch.join("\n")));
        let mut exec = loud(settings, Connection::open(db)?);
        if settings.loud {
            exec.upcoming = migrations
                .iter()
                .filter(|m| m.version == version)
                .map(crate::label)
                .collect();
        }
        let row = |exec: &mut Loud<RusqliteExecutor>| {
            exec.read_ledger()
                .map(|rows| rows.into_iter().find(|row| row.version == version))
        };
        // An up's hash is recorded by the run itself; a down's was recorded when it went up, and
        // its row goes with the run, so the local ledger no longer holds the version for exec.
        let recorded = if matches!(direction, Direction::Down { .. }) {
            let recorded = row(&mut exec)?.and_then(|row| row.down_hash);
            let reverted = down(&mut exec, migrations, 1)?.reverted;
            if reverted != [version] {
                return Err(CliError::ShipFailed(format!(
                    "migratr reverted {reverted:?}, and migratr plan gave version {version} to \
                     revert"
                )));
            }
            recorded
        } else {
            up(&mut exec, migrations, Some(version))?;
            row(&mut exec)?.and_then(|row| row.up_hash)
        };
        if recorded.as_deref() != Some(hash.as_str()) {
            return Err(CliError::ShipFailed(format!(
                "the statements migratr ran for version {version} are not the ones migratr plan \
                 gave for it"
            )));
        }
        write_schema(&mut exec, dir)?;
        drop(exec);

        smugglr.exec(
            db.as_os_str(),
            OsStr::new(place),
            version,
            direction.name(),
            &expect,
            &batch,
        )?;
        versions.push(version);
        expect = fingerprint(&smugglr.read(db.as_os_str())?);
    }

    let there = smugglr.read(OsStr::new(place))?;
    let found = fingerprint(&there);
    if found != expect {
        return Err(CliError::ShipFailed(format!(
            "the place did not end with the local database's shape: its fingerprint is {found}, \
             and the local database's is {expect}"
        )));
    }
    let ledger = there["ledger"].as_array().cloned().unwrap_or_default();
    let mut exec = RusqliteExecutor::new(Connection::open(db)?);
    record_note(
        &mut exec,
        Note::Fingerprint,
        &json!({
            "fingerprint": found,
            "migrations": dev::migration_versions(&ledger),
        })
        .to_string(),
    )?;
    Ok(Shipped {
        versions,
        fingerprint: found,
    })
}

/// A migration's own statements in a plan step: its ledger statements, the CREATE and ALTERs
/// before them and the INSERT after, left out, as its up hash is worked out.
fn operations(statements: &[String]) -> Vec<String> {
    let ledger = |s: &str| {
        s.starts_with(&format!("CREATE TABLE IF NOT EXISTS {LEDGER_TABLE}"))
            || s.starts_with(&format!("ALTER TABLE {LEDGER_TABLE} "))
    };
    let start = statements.iter().take_while(|s| ledger(s)).count();
    let end = statements.len().saturating_sub(1).max(start);
    statements[start..end].to_vec()
}

/// The place's fingerprint dev's fingerprint note recorded.
fn recorded_fingerprint(notes: &Notes) -> String {
    let note: Value = serde_json::from_str(&notes.fingerprint).unwrap_or(Value::Null);
    note["fingerprint"].as_str().unwrap_or_default().to_string()
}

/// What a run that failed partway already took to the place: the migrations up to a version it
/// applied there, or the latest migrations it reverted there, by number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CatchUp {
    Nothing,
    UpTo(u64),
    Reverted(usize),
}

impl CatchUp {
    /// What the run took to the place before it failed, from the migration versions in the
    /// rewind point's ledger (`local`) and the place's (`held`). A down whose reverted
    /// migrations are not the latest of the run's is nothing: the shape stop then refuses it.
    fn of(local: &[u64], held: &[u64], migrations: &[Migration], direction: Direction) -> Self {
        match direction {
            Direction::Up { to } => migrations
                .iter()
                .map(|m| m.version)
                .filter(|v| {
                    held.contains(v) && !local.contains(v) && to.is_none_or(|to| *v <= to)
                })
                .max()
                .map_or(CatchUp::Nothing, CatchUp::UpTo),
            Direction::Down { steps } => {
                let mut run: Vec<u64> = local
                    .iter()
                    .copied()
                    .filter(|v| migrations.iter().any(|m| m.version == *v))
                    .collect();
                run.sort_unstable_by(|a, b| b.cmp(a));
                run.truncate(steps);
                let reverted = run.iter().take_while(|v| !held.contains(v)).count();
                if reverted == 0 || run[reverted..].iter().any(|v| !held.contains(v)) {
                    CatchUp::Nothing
                } else {
                    CatchUp::Reverted(reverted)
                }
            }
        }
    }

    /// Runs on `exec`, at the rewind point, what the run already took to the place.
    fn run(self, exec: &mut impl Executor, migrations: &[Migration]) -> Result<(), CliError> {
        match self {
            CatchUp::Nothing => {}
            CatchUp::UpTo(version) => {
                up(exec, migrations, Some(version))?;
            }
            CatchUp::Reverted(n) => {
                down(exec, migrations, n)?;
            }
        }
        Ok(())
    }

    /// The run's direction once this has been caught up.
    fn after(self, direction: Direction) -> Direction {
        match (self, direction) {
            (CatchUp::Reverted(n), Direction::Down { steps }) => Direction::Down {
                steps: steps.saturating_sub(n),
            },
            _ => direction,
        }
    }
}

/// A copy of the rewind point in its own directory under `.migratr`, removed when dropped.
struct Scratch {
    dir: PathBuf,
    db: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort: nothing in it is kept.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The fingerprint the place should have before a run, and what a run that failed partway
/// already took to it.
struct Expected {
    fingerprint: String,
    catch_up: CatchUp,
    /// The rewind point with the catch-up run on it, when there is one.
    copy: Option<Scratch>,
}

/// The fingerprint the place, whose read form is `there`, should have before the run: the one
/// dev recorded or, when a run that failed partway already took some of its migrations to the
/// place, the one smugglr's read form gives for a copy of the rewind point with them caught up.
fn expected(
    db: &Path,
    point: &RewindPoint,
    notes: &Notes,
    migrations: &[Migration],
    direction: Direction,
    there: &Value,
    smugglr: &Smugglr,
) -> Result<Expected, CliError> {
    let held = dev::migration_versions(there["ledger"].as_array().map_or(&[][..], Vec::as_slice));
    let local: Vec<u64> = RusqliteExecutor::new(Connection::open_with_flags(
        &point.snapshot,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
    .read_ledger()?
    .into_iter()
    .map(|row| row.version)
    .collect();
    let catch_up = CatchUp::of(&local, &held, migrations, direction);
    if catch_up == CatchUp::Nothing {
        return Ok(Expected {
            fingerprint: recorded_fingerprint(notes),
            catch_up,
            copy: None,
        });
    }
    let dir = dev::migratr_dir(db).join("caught-up");
    let at = |source: io::Error| CliError::Io {
        path: dir.clone(),
        source,
    };
    if dir.exists() {
        // Left by a run that stopped; nothing in it is kept.
        fs::remove_dir_all(&dir).map_err(at)?;
    }
    fs::create_dir_all(&dir).map_err(at)?;
    let copy = Scratch {
        db: dir.join(db.file_name().unwrap_or(OsStr::new("local.db"))),
        dir,
    };
    put_back(&copy.db, point, notes)?;
    catch_up.run(
        &mut RusqliteExecutor::new(Connection::open(&copy.db)?),
        migrations,
    )?;
    Ok(Expected {
        fingerprint: fingerprint(&smugglr.read(copy.db.as_os_str())?),
        catch_up,
        copy: Some(copy),
    })
}

/// Whether the rewind point's file has the SHA-256 dev recorded.
fn point_is_recorded(point: &RewindPoint) -> Result<bool, CliError> {
    let bytes = fs::read(&point.snapshot).map_err(|source| CliError::Io {
        path: point.snapshot.clone(),
        source,
    })?;
    Ok(format!("{:x}", Sha256::digest(&bytes)) == point.sha256)
}

fn fingerprint(read: &Value) -> String {
    read["fingerprint"].as_str().unwrap_or_default().to_string()
}

/// Puts the rewind point back at the local database's own path and writes dev's two notes into
/// its ledger: the snapshot was taken before dev wrote them.
fn put_back(db: &Path, point: &RewindPoint, notes: &Notes) -> Result<(), CliError> {
    restore(db, Some(&point.snapshot), true)?;
    let mut exec = RusqliteExecutor::new(Connection::open(db)?);
    record_note(&mut exec, Note::RewindPoint, &notes.rewind)?;
    record_note(&mut exec, Note::Fingerprint, &notes.fingerprint)?;
    Ok(())
}

/// Dev's two notes in the ledger of `db`; with no rewind point, an error saying to run migratr
/// dev, before anything changes.
fn notes(db: &Path, command: &str) -> Result<Notes, CliError> {
    let note = |conn: &Connection, note: Note| -> Result<Option<String>, CliError> {
        let has_ledger: bool = conn.query_row(
            "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [LEDGER_TABLE],
            |row| row.get(0),
        )?;
        if !has_ledger {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                &format!("SELECT checksum FROM {LEDGER_TABLE} WHERE version = ?1"),
                [note.version().to_string()],
                |row| row.get(0),
            )
            .optional()?)
    };
    let (rewind, fingerprint) = if db.exists() {
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        (
            note(&conn, Note::RewindPoint)?,
            note(&conn, Note::Fingerprint)?,
        )
    } else {
        (None, None)
    };
    match (rewind, fingerprint) {
        (Some(rewind), Some(fingerprint)) => Ok(Notes {
            rewind,
            fingerprint,
        }),
        _ => Err(CliError::NoRewindPoint(format!(
            "migratr {command} --smuggl needs the rewind point migratr dev records, and the \
             ledger holds none; run migratr dev first; nothing changed"
        ))),
    }
}

fn rewind_point(json: &str) -> Result<RewindPoint, CliError> {
    let note: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    match (note["snapshot"].as_str(), note["sha256"].as_str()) {
        (Some(snapshot), Some(sha256)) => Ok(RewindPoint {
            snapshot: PathBuf::from(snapshot),
            sha256: sha256.to_string(),
        }),
        _ => Err(CliError::NoRewindPoint(
            "the ledger's rewind-point row names no snapshot; run migratr dev first; nothing \
             changed"
                .to_string(),
        )),
    }
}

/// Copies the directory `from`, when there is one, to `to`, every file in it.
fn copy_tree(from: &Path, to: &Path) -> Result<(), CliError> {
    let at = |path: &Path| {
        let path = path.to_path_buf();
        move |source: io::Error| CliError::Io { path, source }
    };
    fs::create_dir_all(to).map_err(at(to))?;
    if !from.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(from).map_err(at(from))? {
        let entry = entry.map_err(at(from))?;
        let target = to.join(entry.file_name());
        if entry.file_type().map_err(at(&entry.path()))?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(at(&target))?;
        }
    }
    Ok(())
}
