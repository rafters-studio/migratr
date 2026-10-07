mod error;
mod plan;
mod status;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use migratr::{
    RusqliteExecutor, down, latest_snapshot, load_dir, restore, scaffold, up, write_schema,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};

use crate::error::CliError;

const COMMANDS: [&str; 6] = ["new", "up", "down", "status", "plan", "restore"];

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
    Plan {
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

impl From<Option<PlanDirection>> for Direction {
    fn from(direction: Option<PlanDirection>) -> Self {
        match direction {
            None => Direction::Up { to: None },
            Some(PlanDirection::Up { to }) => Direction::Up { to },
            Some(PlanDirection::Down { steps }) => Direction::Down { steps },
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
    match run(&cli) {
        Ok(outcome) => {
            if cli.json {
                println!("{}", ok_document(name, outcome.fields));
            } else {
                println!("{}", outcome.text);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            if cli.json {
                println!("{}", error_document(Some(name), &error));
            } else {
                eprintln!("error [{}]: {error}", error.code());
            }
            ExitCode::FAILURE
        }
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

fn run(cli: &Cli) -> Result<Outcome, CliError> {
    match &cli.command {
        Command::New { name, specs } => {
            let path = scaffold(&cli.dir, name, specs)?;
            Ok(Outcome {
                fields: json!({ "path": path }),
                text: format!("created {}", path.display()),
            })
        }
        Command::Up { to } => {
            let db = db_path(cli)?;
            let migrations = load_dir(&cli.dir)?;
            let mut exec = RusqliteExecutor::new(Connection::open(db)?);
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
            let mut exec = RusqliteExecutor::new(Connection::open(db)?);
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
        Command::Plan { direction } => {
            let db = db_path(cli)?;
            let migrations = load_dir(&cli.dir)?;
            let direction = Direction::from(direction.clone());
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

fn version_list(versions: &[u64]) -> String {
    if versions.is_empty() {
        return String::new();
    }
    let list: Vec<String> = versions.iter().map(u64::to_string).collect();
    format!(": {}", list.join(", "))
}
