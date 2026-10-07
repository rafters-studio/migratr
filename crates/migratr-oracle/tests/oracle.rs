use std::fs;

use migratr::{RusqliteExecutor, load_dir, up};
use migratr_oracle::{HazardCase, cases};
use rusqlite::Connection;
use tempfile::TempDir;

/// A connection holding the case's schema and seed, with foreign keys enforced.
fn seeded(case: &HazardCase) -> Connection {
    let conn = Connection::open_in_memory().expect("open");
    conn.pragma_update(None, "foreign_keys", true)
        .expect("foreign keys on");
    conn.execute_batch(case.schema).expect("schema");
    (case.seed)(&conn);
    conn
}

/// The tables that carry the `code` column the rebuild drops.
fn coded_tables(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT name FROM sqlite_master m
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
           AND EXISTS (SELECT 1 FROM pragma_table_info(m.name) WHERE name = 'code')
         ORDER BY name",
    )
    .expect("prepare")
    .query_map([], |row| row.get(0))
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("tables")
}

/// Rebuilds every table of the case by dropping its UNIQUE `code` column through migratr.
fn rebuilt(case: &HazardCase) -> RusqliteExecutor {
    let conn = seeded(case);
    // A rebuild must be the first operation of its migration, so each table gets its own.
    let dir = TempDir::new().expect("tempdir");
    for (index, table) in coded_tables(&conn).into_iter().enumerate() {
        let migration = serde_json::json!({
            "up": [{
                "op": "drop_column",
                "table": table,
                "column": {"name": "code", "type": "TEXT"},
            }]
        });
        fs::write(
            dir.path()
                .join(format!("2026010100{index:04}_drop_code.json")),
            migration.to_string(),
        )
        .expect("write migration");
    }
    let migrations = load_dir(dir.path()).expect("load");
    let mut executor = RusqliteExecutor::new(conn);
    up(&mut executor, &migrations, None).expect("up");
    executor
}

fn case_named(name: &str) -> HazardCase {
    cases()
        .into_iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("no case named {name}"))
}

#[test]
fn the_rebuild_diverges_from_the_untouched_reference_nowhere() {
    for case in cases() {
        let reference = (case.probe)(&seeded(&case));
        assert_eq!(reference, Ok(()), "the reference table fails its own probe");

        let executor = rebuilt(&case);
        assert!(
            coded_tables(executor.connection()).is_empty(),
            "{}: the rebuild left a code column behind",
            case.name
        );
        let after_rebuild = (case.probe)(executor.connection());
        assert_eq!(after_rebuild, reference, "{} diverged", case.name);
    }
}

/// How a case's construct is taken away.
enum Fault {
    /// Replace text in the schema before seeding.
    Schema(String, String),
    /// Run statements on the seeded database.
    After(&'static str),
    /// Replace text in the schema before seeding, then run statements on the seeded database.
    SchemaThenAfter(String, String, &'static str),
}

struct Break {
    case: &'static str,
    fault: Fault,
}

fn schema_break(case: &'static str, from: &str, to: &str) -> Break {
    Break {
        case,
        fault: Fault::Schema(from.to_string(), to.to_string()),
    }
}

/// What a rebuild that re-created the trigger before copying rows would leave behind.
const TRIGGER_RECREATED_BEFORE_COPY: &str = "
    DROP TRIGGER evented_audit;
    CREATE TABLE evented_new (id INTEGER PRIMARY KEY, note TEXT);
    CREATE TRIGGER evented_audit AFTER INSERT ON evented_new
    BEGIN INSERT INTO audit (note) VALUES (NEW.note); END;
    INSERT INTO evented_new SELECT id, note FROM evented;
    DROP TABLE evented;
    ALTER TABLE evented_new RENAME TO evented;";

/// For each foreign-key (event, action), the action it is wrongly rebuilt as.
const FOREIGN_KEY_FAULTS: [(&str, &str, &str); 10] = [
    ("DELETE", "CASCADE", "NO ACTION"),
    ("UPDATE", "CASCADE", "NO ACTION"),
    ("DELETE", "SET NULL", "NO ACTION"),
    ("UPDATE", "SET NULL", "NO ACTION"),
    ("DELETE", "SET DEFAULT", "NO ACTION"),
    ("UPDATE", "SET DEFAULT", "NO ACTION"),
    ("DELETE", "RESTRICT", "NO ACTION"),
    ("UPDATE", "RESTRICT", "NO ACTION"),
    ("DELETE", "NO ACTION", "RESTRICT"),
    ("UPDATE", "NO ACTION", "RESTRICT"),
];

fn breaks() -> Vec<Break> {
    let mut all: Vec<Break> = FOREIGN_KEY_FAULTS
        .iter()
        .map(|(event, action, wrong)| {
            schema_break(
                "foreign-key actions",
                &format!("ON {event} {action}"),
                &format!("ON {event} {wrong}"),
            )
        })
        .collect();
    all.extend([
        schema_break(
            "virtual generated columns",
            "GENERATED ALWAYS AS (base * 2) VIRTUAL",
            "",
        ),
        schema_break(
            "stored generated columns",
            "GENERATED ALWAYS AS (base * 3) STORED",
            "",
        ),
        schema_break("column ON CONFLICT clauses", "ON CONFLICT REPLACE", ""),
        schema_break("column ON CONFLICT clauses", "ON CONFLICT IGNORE", ""),
        schema_break(
            "column ON CONFLICT clauses",
            "ON CONFLICT ABORT",
            "ON CONFLICT IGNORE",
        ),
        schema_break(
            "column ON CONFLICT clauses",
            "ON CONFLICT ROLLBACK",
            "ON CONFLICT ABORT",
        ),
        schema_break("expression DEFAULTs", "DEFAULT (datetime('now'))", ""),
        schema_break("expression DEFAULTs", "DEFAULT (2 + 3)", "DEFAULT '2 + 3'"),
        schema_break("typeless columns", ", v, ", ", v BLOB, "),
        Break {
            case: "trigger re-fire",
            fault: Fault::After(TRIGGER_RECREATED_BEFORE_COPY),
        },
        Break {
            case: "trigger re-fire",
            fault: Fault::After("DROP TRIGGER evented_audit;"),
        },
        schema_break(
            "INTEGER PRIMARY KEY DESC",
            "PRIMARY KEY DESC",
            "PRIMARY KEY",
        ),
        Break {
            case: "views",
            fault: Fault::After("DROP VIEW view_shout;"),
        },
        Break {
            case: "views",
            fault: Fault::After("DROP TRIGGER view_shout_insert;"),
        },
    ]);
    all
}

/// Builds the database a faulty rebuild would have produced and returns the probe's answer.
fn probe_after(fault: &Break) -> Result<(), migratr_oracle::ProbeError> {
    let case = case_named(fault.case);
    let conn = Connection::open_in_memory().expect("open");
    conn.pragma_update(None, "foreign_keys", true)
        .expect("foreign keys on");
    match &fault.fault {
        Fault::Schema(from, to) | Fault::SchemaThenAfter(from, to, _) => {
            assert!(
                case.schema.contains(from.as_str()),
                "{}: the schema has no `{from}` to remove",
                case.name
            );
            conn.execute_batch(&case.schema.replacen(from.as_str(), to, 1))
                .expect("broken schema");
            (case.seed)(&conn);
            if let Fault::SchemaThenAfter(_, _, sql) = &fault.fault {
                conn.execute_batch(sql).expect("fault");
            }
        }
        Fault::After(sql) => {
            conn.execute_batch(case.schema).expect("schema");
            (case.seed)(&conn);
            conn.execute_batch(sql).expect("fault");
        }
    }
    (case.probe)(&conn)
}

#[test]
fn removing_a_construct_makes_its_probe_fail() {
    let all = breaks();
    for fault in &all {
        let error = probe_after(fault).expect_err(&format!(
            "{}: the probe passed with a construct removed",
            fault.case
        ));
        assert_eq!(error.case, fault.case);
    }
    for case in cases() {
        assert!(
            all.iter().any(|fault| fault.case == case.name),
            "{} has no mutation",
            case.name
        );
    }
}

/// A rebuild that copies a generated column's computed value into an ordinary column reads
/// correctly until the base moves, so only the second read catches it.
#[test]
fn a_generated_value_frozen_in_an_ordinary_column_fails_after_its_base_moves() {
    let frozen = [
        (
            "virtual generated columns",
            "GENERATED ALWAYS AS (base * 2) VIRTUAL",
            "UPDATE generated_virtual SET doubled = base * 2;",
        ),
        (
            "stored generated columns",
            "GENERATED ALWAYS AS (base * 3) STORED",
            "UPDATE generated_stored SET tripled = base * 3;",
        ),
    ];
    for (case, clause, freeze) in frozen {
        let fault = Break {
            case,
            fault: Fault::SchemaThenAfter(clause.to_string(), String::new(), freeze),
        };
        let error = probe_after(&fault).expect_err(case);
        assert!(
            error.differed.contains("after its base moved"),
            "{case}: failed at the wrong check: {error}"
        );
    }
}

#[test]
fn every_probe_fails_on_an_unseeded_database() {
    for case in cases() {
        let conn = Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys on");
        conn.execute_batch(case.schema).expect("schema");
        assert!(
            (case.probe)(&conn).is_err(),
            "{} passes on an empty database",
            case.name
        );
    }
}

#[test]
fn the_known_smugglr_rebuild_defects_are_caught() {
    let regressions = [
        (
            "smugglr#336",
            Break {
                case: "trigger re-fire",
                fault: Fault::After(TRIGGER_RECREATED_BEFORE_COPY),
            },
        ),
        (
            "smugglr#341",
            schema_break(
                "foreign-key actions",
                "ON DELETE CASCADE",
                "ON DELETE NO ACTION",
            ),
        ),
        (
            "smugglr#344",
            schema_break("typeless columns", ", v, ", ", v BLOB, "),
        ),
    ];
    for (label, fault) in &regressions {
        let error = probe_after(fault).expect_err(&format!("{label} went unnoticed"));
        assert_eq!(error.case, fault.case, "{label}");
    }
}
