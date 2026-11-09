//! `migratr up --smuggl`: the migrations the run applies on the local database, shipped to the
//! place `smuggl` names through the rewind point migratr dev recorded and smugglr exec.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use migratr::{
    DevSnapshot, Executor, Migration, Note, load_dir, record_note, restore, snapshot_for_dev, up,
    write_schema,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::dev::{self, Smugglr};
use crate::error::CliError;
use crate::settings::Settings;
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

/// Ships the migrations up to `to` to the place: snapshots the working database, puts the rewind
/// point back at the local database's own path with dev's two notes, syncs it with the place so
/// the place's changes since dev arrive, applies the migrations on it one at a time, and sends
/// each one's statements, as `migratr plan` gives them, through smugglr exec. The place must end
/// with the local database's fingerprint, which the ledger then records. On success with
/// autoclean on, the rewind point, its ledger row and the working database's snapshot are
/// removed. On any failure the local database is put back at the rewind point with both notes
/// and nothing is removed, so the same run can be tried again.
pub fn up_smuggl(
    db: &Path,
    dir: &Path,
    settings: &Settings,
    to: Option<u64>,
) -> Result<Outcome, CliError> {
    let notes = notes(db, "up")?;
    let place = dev::place(settings, "up --smuggl")?;
    let point = rewind_point(&notes.rewind)?;
    let bytes = fs::read(&point.snapshot).map_err(|source| CliError::Io {
        path: point.snapshot.clone(),
        source,
    })?;
    if format!("{:x}", Sha256::digest(&bytes)) != point.sha256 {
        return Err(CliError::ShipFailed(format!(
            "the rewind point {} is not the file migratr dev recorded: its SHA-256 differs; run \
             migratr dev again; nothing changed",
            point.snapshot.display()
        )));
    }
    let migrations = load_dir(dir)?;
    let smugglr = Smugglr::find();

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
        to,
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
                 database's snapshot {} remain; run migratr up --smuggl again",
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

/// `migratr down --smuggl`: with no rewind point it stops, saying to run migratr dev.
pub fn down_smuggl(db: &Path) -> Result<Outcome, CliError> {
    notes(db, "down")?;
    Err(CliError::Usage(
        "migratr down --smuggl cannot ship a revert yet; nothing changed".to_string(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn ship(
    db: &Path,
    dir: &Path,
    place: &str,
    smugglr: &Smugglr,
    work: &Path,
    migrations: &[Migration],
    to: Option<u64>,
    notes: &Notes,
    point: &RewindPoint,
    settings: &Settings,
) -> Result<Shipped, CliError> {
    put_back(db, point, notes)?;
    let local = dev::address(db)?;
    smugglr.sync(local.as_os_str(), OsStr::new(place), work, false)?;

    let planned = {
        let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let bytes = fs::metadata(db).map_or(0, |m| m.len());
        plan::plan(&conn, bytes, migrations, Direction::Up { to })?
    };
    let recorded: Value = serde_json::from_str(&notes.fingerprint).unwrap_or(Value::Null);
    let mut expect = recorded["fingerprint"]
        .as_str()
        .unwrap_or_default()
        .to_string();
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

        let mut exec = loud(settings, Connection::open(db)?);
        if settings.loud {
            exec.upcoming = migrations
                .iter()
                .filter(|m| m.version == version)
                .map(crate::label)
                .collect();
        }
        up(&mut exec, migrations, Some(version))?;
        let recorded = exec
            .read_ledger()?
            .into_iter()
            .find(|row| row.version == version)
            .and_then(|row| row.up_hash);
        let hash = format!("{:x}", Sha256::digest(batch.join("\n")));
        if recorded.as_deref() != Some(hash.as_str()) {
            return Err(CliError::ShipFailed(format!(
                "the statements migratr ran for version {version} are not the ones migratr plan \
                 gave for it"
            )));
        }
        write_schema(&mut exec, dir)?;
        drop(exec);

        smugglr.exec(db.as_os_str(), OsStr::new(place), version, &expect, &batch)?;
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
    let mut exec = migratr::RusqliteExecutor::new(Connection::open(db)?);
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

fn fingerprint(read: &Value) -> String {
    read["fingerprint"].as_str().unwrap_or_default().to_string()
}

/// Puts the rewind point back at the local database's own path and writes dev's two notes into
/// its ledger: the snapshot was taken before dev wrote them.
fn put_back(db: &Path, point: &RewindPoint, notes: &Notes) -> Result<(), CliError> {
    restore(db, Some(&point.snapshot), true)?;
    let mut exec = migratr::RusqliteExecutor::new(Connection::open(db)?);
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
