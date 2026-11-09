mod dev;
mod error;
mod plan;
mod settings;
mod status;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use migratr::{
    Executor, Migration, RusqliteExecutor, down, latest_snapshot, load_dir, restore, scaffold, up,
    write_schema,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};

use crate::error::CliError;
use crate::settings::{Loud, QUIET_NOTE, Settings};

const COMMANDS: [&str; 7] = ["new", "up", "down", "status", "plan", "restore", "dev"];

#[derive(Parser)]
#[command(name = "migratr", version, about = "Schema migrations for SQLite")]
struct Cli {
    /// The SQLite database file.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// The migrations directory.
    #[arg(long, global = true, default_value = "migrations")]
    dir: PathBuf,

    /// Write one JSON document describing the outcome to stdout.
    #[arg(long, global = true)]
    json: bool,

    /// The settings file to read instead of the nearest migratr.jsonc.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold a migration file from a Rails-style name and column specs.
    New {
        name: String,
        /// Column specs: name[:type][:modifier...].
        specs: Vec<String>,
    },
    /// Apply pending migrations.
    Up {
        /// Stop after this version.
        #[arg(long)]
        to: Option<u64>,
    },
    /// Reverse the newest applied migrations.
    Down {
        #[arg(long, default_value_t = 1)]
        steps: usize,
    },
    /// List every migration as applied or pending.
    Status,
    /// Show the SQL a run would execute, without running it.
    #[command(args_conflicts_with_subcommands = true)]
    Plan {
        /// Plan an up run that stops after this version.
        #[arg(long)]
        to: Option<u64>,
        #[command(subcommand)]
        direction: Option<PlanDirection>,
    },
    /// Replace the database with a snapshot.
    Restore {
        /// The snapshot to restore; the newest by default.
        #[arg(long)]
        snapshot: Option<PathBuf>,
        /// Confirm the restore.
        #[arg(long)]
        yes: bool,
    },
    /// Make the local database from the place smuggl names and record the rewind point.
    Dev,
}

#[derive(Subcommand, Clone)]
enum PlanDirection {
    Up {
        #[arg(long)]
        to: Option<u64>,
    },
    Down {
        #[arg(long, default_value_t = 1)]
        steps: usize,
    },
}

/// Which way a plan runs.
#[derive(Clone, Copy)]
pub enum Direction {
    Up { to: Option<u64> },
    Down { steps: usize },
}

impl Direction {
    pub fn name(self) -> &'static str {
        match self {
            Direction::Up { .. } => "up",
            Direction::Down { .. } => "down",
        }
    }
}

impl From<PlanDirection> for Direction {
    fn from(direction: PlanDirection) -> Self {
        match direction {
            PlanDirection::Up { to } => Direction::Up { to },
            PlanDirection::Down { steps } => Direction::Down { steps },
        }
    }
}

/// A successful command: the fields its JSON document carries and the text printed without
/// `--json`.
struct Outcome {
    fields: Value,
    text: String,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if e.use_stderr() => {
            let args: Vec<String> = std::env::args().collect();
            if args.iter().any(|a| a == "--json") {
                let command = args.iter().find(|a| COMMANDS.contains(&a.as_str()));
                let error = CliError::Usage(e.render().to_string());
                println!("{}", error_document(command.map(String::as_str), &error));
                return ExitCode::from(2);
            }
            e.exit()
        }
        Err(e) => e.exit(),
    };

    let name = command_name(&cli.command);
    // Settings are read before anything else, so a refused file changes nothing.
    let settings = match settings::load(cli.config.as_deref()) {
        Ok(settings) => settings,
        Err(error) => return fail(&cli, name, &error),
    };
    if settings.loud {
        for line in settings.describe() {
            eprintln!("{line}");
        }
    }
    let code = match run(&cli, &settings) {
        Ok(outcome) => {
            if cli.json {
                println!("{}", ok_document(name, outcome.fields));
            } else {
                println!("{}", outcome.text);
            }
            ExitCode::SUCCESS
        }
        Err(error) => fail(&cli, name, &error),
    };
    if settings.loud {
        eprintln!("{QUIET_NOTE}");
    }
    code
}

/// Reports `error` and returns its exit code.
fn fail(cli: &Cli, name: &str, error: &CliError) -> ExitCode {
    if cli.json {
        println!("{}", error_document(Some(name), error));
    } else {
        eprintln!("error [{}]: {error}", error.code());
    }
    match error {
        CliError::Usage(_) => ExitCode::from(2),
        _ => ExitCode::FAILURE,
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::New { .. } => "new",
        Command::Up { .. } => "up",
        Command::Down { .. } => "down",
        Command::Status => "status",
        Command::Plan { .. } => "plan",
        Command::Restore { .. } => "restore",
        Command::Dev => "dev",
    }
}

fn ok_document(command: &str, fields: Value) -> Value {
    let mut document = Map::new();
    document.insert("command".into(), json!(command));
    document.insert("status".into(), json!("ok"));
    if let Value::Object(fields) = fields {
        document.extend(fields);
    }
    Value::Object(document)
}

fn error_document(command: Option<&str>, error: &CliError) -> Value {
    json!({
        "command": command,
        "status": "error",
        "code": error.code(),
        "message": error.to_string(),
    })
}

fn run(cli: &Cli, settings: &Settings) -> Result<Outcome, CliError> {
    match &cli.command {
        Command::New { name, specs } => {
            if settings.safe
                && settings.smuggl.is_some()
                && !dev::has_rewind_point(cli.db.as_deref())?
            {
                return Err(CliError::NoRewindPoint);
            }
            let path = scaffold(&cli.dir, name, specs)?;
            Ok(Outcome {
                fields: json!({ "path": path }),
                text: format!("created {}", path.display()),
            })
        }
        Command::Up { to } => {
            let db = db_path(cli)?;
            let migrations = load_dir(&cli.dir)?;
            let mut exec = loud(settings, Connection::open(db)?);
            if settings.loud {
                let ledger = exec.read_ledger()?;
                let mut pending: Vec<&Migration> = migrations
                    .iter()
                    .filter(|m| !ledger.iter().any(|r| r.version == m.version))
                    .filter(|m| to.is_none_or(|limit| m.version <= limit))
                    .collect();
                pending.sort_by_key(|m| m.version);
                exec.upcoming = pending.into_iter().map(label).collect();
            }
            let report = up(&mut exec, &migrations, *to)?;
            write_schema(&mut exec, &cli.dir)?;
            Ok(Outcome {
                text: format!(
                    "applied {} migration(s){}",
                    report.applied.len(),
                    version_list(&report.applied)
                ),
                fields: json!({ "applied": report.applied }),
            })
        }
        Command::Down { steps } => {
            let db = db_path(cli)?;
            let migrations = load_dir(&cli.dir)?;
            let mut exec = loud(settings, Connection::open(db)?);
            if settings.loud {
                let mut versions: Vec<u64> =
                    exec.read_ledger()?.iter().map(|r| r.version).collect();
                versions.sort_unstable_by(|a, b| b.cmp(a));
                versions.truncate(*steps);
                exec.upcoming = versions
                    .iter()
                    .filter_map(|v| migrations.iter().find(|m| m.version == *v))
                    .map(label)
                    .collect();
            }
            let report = down(&mut exec, &migrations, *steps)?;
            write_schema(&mut exec, &cli.dir)?;
            let snapshot = latest_snapshot(db)?;
            let pointer = snapshot.as_ref().map_or_else(
                || "No snapshot exists.".to_string(),
                |path| format!("Latest snapshot: {}", path.display()),
            );
            Ok(Outcome {
                text: format!("{report} {pointer}"),
                fields: json!({
                    "reverted": report.reverted,
                    "data_restored": false,
                    "snapshot": snapshot,
                }),
            })
        }
        Command::Status => {
            let db = db_path(cli)?;
            let migrations = load_dir(&cli.dir)?;
            status::status(db, &migrations)
        }
        Command::Plan { to, direction } => {
            let db = db_path(cli)?;
            let migrations = load_dir(&cli.dir)?;
            let direction = match direction {
                None => Direction::Up { to: *to },
                Some(direction) => Direction::from(direction.clone()),
            };
            let (conn, bytes) = match open_existing(db)? {
                Some(conn) => (conn, std::fs::metadata(db).map_or(0, |m| m.len())),
                None => (Connection::open_in_memory()?, 0),
            };
            let fields = plan::plan(&conn, bytes, &migrations, direction)?;
            Ok(Outcome {
                text: plan::render(&fields),
                fields,
            })
        }
        Command::Dev => dev::dev(db_path(cli)?, settings),
        Command::Restore { snapshot, yes } => {
            let db = db_path(cli)?;
            let report = restore(db, snapshot.as_deref(), *yes)?;
            Ok(Outcome {
                text: report.to_string(),
                fields: json!({
                    "restored": report.restored,
                    "snapshot_time": report.snapshot_time,
                    "snapshot_version": report.snapshot_version,
                    "removed": report.removed,
                }),
            })
        }
    }
}

/// An executor over `conn` that prints each migration and statement when loud is on.
fn loud(settings: &Settings, conn: Connection) -> Loud<RusqliteExecutor> {
    Loud {
        inner: RusqliteExecutor::new(conn),
        loud: settings.loud,
        upcoming: VecDeque::new(),
    }
}

fn label(migration: &Migration) -> String {
    format!("{}_{}", migration.version, migration.name)
}

fn db_path(cli: &Cli) -> Result<&Path, CliError> {
    cli.db
        .as_deref()
        .ok_or_else(|| CliError::Usage("--db PATH is required".to_string()))
}

/// Opens `db` read-only, or returns `None` when the file does not exist.
fn open_existing(db: &Path) -> Result<Option<Connection>, CliError> {
    if !db.exists() {
        return Ok(None);
    }
    Ok(Some(Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?))
}

/// Whether `conn` holds migratr's ledger table.
fn has_ledger(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = '_migratr_migrations'",
        [],
        |row| row.get(0),
    )
}

fn version_list(versions: &[u64]) -> String {
    if versions.is_empty() {
        return String::new();
    }
    let list: Vec<String> = versions.iter().map(u64::to_string).collect();
    format!(": {}", list.join(", "))
}
