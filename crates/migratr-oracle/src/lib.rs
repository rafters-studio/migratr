//! Behavioural cases for the SQLite table-rebuild hazards.
//!
//! A case is a schema, a seed, and a probe. The seed fills the schema before a rebuild; the
//! probe then exercises the construct the case is about and reports what differed. A rebuild
//! is correct when the probe gives the same answer on the rebuilt database as on an untouched
//! copy of the same schema and seed.
//!
//! Every table in every schema carries a `code TEXT UNIQUE` column. SQLite's own
//! `ALTER TABLE ... DROP COLUMN` refuses a UNIQUE column, so dropping it forces the rebuild.

use std::sync::LazyLock;

use rusqlite::Connection;
use thiserror::Error;

/// A probe found the database behaving differently from what its schema promises.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{case}: probe `{probe}` failed: {differed}")]
pub struct ProbeError {
    pub case: &'static str,
    pub probe: &'static str,
    pub differed: String,
}

/// One rebuild hazard: a schema carrying the construct, a seed, and a probe of its behaviour.
pub struct HazardCase {
    pub name: &'static str,
    pub schema: &'static str,
    /// Fills the schema with fixed rows. Panics when the rows do not fit the schema, which
    /// is a defect in the case itself.
    pub seed: fn(&Connection),
    /// Exercises the construct. Writes to the database, so run it once per connection.
    pub probe: fn(&Connection) -> Result<(), ProbeError>,
}

/// Every hazard case, in a fixed order.
pub fn cases() -> Vec<HazardCase> {
    vec![
        HazardCase {
            name: FOREIGN_KEYS,
            schema: FOREIGN_KEY_SCHEMA.as_str(),
            seed: |conn| seed(conn, &FOREIGN_KEY_SEED),
            probe: |conn| run(FOREIGN_KEYS, conn, foreign_keys),
        },
        HazardCase {
            name: GENERATED_VIRTUAL,
            schema: "CREATE TABLE generated_virtual (
                id INTEGER PRIMARY KEY,
                base INTEGER,
                doubled INTEGER GENERATED ALWAYS AS (base * 2) VIRTUAL,
                code TEXT UNIQUE
            );",
            seed: |conn| {
                seed(
                    conn,
                    "INSERT INTO generated_virtual (id, base) VALUES (1, 21);",
                )
            },
            probe: |conn| run(GENERATED_VIRTUAL, conn, generated_virtual),
        },
        HazardCase {
            name: GENERATED_STORED,
            schema: "CREATE TABLE generated_stored (
                id INTEGER PRIMARY KEY,
                base INTEGER,
                tripled INTEGER GENERATED ALWAYS AS (base * 3) STORED,
                code TEXT UNIQUE
            );",
            seed: |conn| {
                seed(
                    conn,
                    "INSERT INTO generated_stored (id, base) VALUES (1, 5);",
                )
            },
            probe: |conn| run(GENERATED_STORED, conn, generated_stored),
        },
        HazardCase {
            name: COLUMN_ON_CONFLICT,
            schema: "CREATE TABLE conflict_replace (
                id INTEGER PRIMARY KEY,
                k TEXT UNIQUE ON CONFLICT REPLACE,
                label TEXT,
                code TEXT UNIQUE
            );
            CREATE TABLE conflict_ignore (
                id INTEGER PRIMARY KEY,
                v TEXT NOT NULL ON CONFLICT IGNORE,
                code TEXT UNIQUE
            );
            CREATE TABLE conflict_abort (
                id INTEGER PRIMARY KEY,
                v TEXT NOT NULL ON CONFLICT ABORT,
                code TEXT UNIQUE
            );
            CREATE TABLE conflict_rollback (
                id INTEGER PRIMARY KEY,
                v TEXT NOT NULL ON CONFLICT ROLLBACK,
                code TEXT UNIQUE
            );",
            seed: |conn| {
                seed(
                    conn,
                    "INSERT INTO conflict_replace (id, k, label) VALUES (1, 'taken', 'seeded');
                     INSERT INTO conflict_ignore (id, v) VALUES (1, 'seeded');
                     INSERT INTO conflict_abort (id, v) VALUES (1, 'seeded');
                     INSERT INTO conflict_rollback (id, v) VALUES (1, 'seeded');",
                )
            },
            probe: |conn| run(COLUMN_ON_CONFLICT, conn, column_on_conflict),
        },
        HazardCase {
            name: EXPRESSION_DEFAULT,
            schema: "CREATE TABLE expression_default (
                id INTEGER PRIMARY KEY,
                made_at TEXT DEFAULT (datetime('now')),
                computed INTEGER DEFAULT (2 + 3),
                code TEXT UNIQUE
            );",
            seed: |conn| seed(conn, "INSERT INTO expression_default (id) VALUES (1);"),
            probe: |conn| run(EXPRESSION_DEFAULT, conn, expression_default),
        },
        HazardCase {
            name: TYPELESS_COLUMN,
            schema: "CREATE TABLE typeless_value (id INTEGER PRIMARY KEY, v, code TEXT UNIQUE);",
            seed: |conn| {
                seed(
                    conn,
                    "INSERT INTO typeless_value (id, v) VALUES (1, 'a string'), (2, 42);",
                )
            },
            probe: |conn| run(TYPELESS_COLUMN, conn, typeless_column),
        },
        HazardCase {
            name: TRIGGER_REFIRE,
            schema: "CREATE TABLE evented (
                id INTEGER PRIMARY KEY,
                note TEXT,
                code TEXT UNIQUE
            );
            CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT);
            CREATE TRIGGER evented_audit AFTER INSERT ON evented
            BEGIN
                INSERT INTO audit (note) VALUES (NEW.note);
            END;",
            seed: |conn| seed(conn, "INSERT INTO evented (id, note) VALUES (1, 'before');"),
            probe: |conn| run(TRIGGER_REFIRE, conn, trigger_refire),
        },
        HazardCase {
            name: DESCENDING_KEY,
            schema: "CREATE TABLE descending_key (
                id INTEGER PRIMARY KEY DESC,
                label TEXT,
                code TEXT UNIQUE
            );",
            seed: |conn| {
                seed(
                    conn,
                    "INSERT INTO descending_key (id, label) VALUES (100, 'seeded'), (200, 'seeded');",
                )
            },
            probe: |conn| run(DESCENDING_KEY, conn, descending_key),
        },
        HazardCase {
            name: VIEWS,
            schema: "CREATE TABLE view_base (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                code TEXT UNIQUE
            );
            CREATE VIEW view_shout AS SELECT id, upper(name) AS shout FROM view_base;
            CREATE TRIGGER view_shout_insert INSTEAD OF INSERT ON view_shout
            BEGIN
                INSERT INTO view_base (id, name) VALUES (NEW.id, lower(NEW.shout));
            END;",
            seed: |conn| {
                seed(
                    conn,
                    "INSERT INTO view_base (id, name, code) VALUES (1, 'ann', 'a1');",
                )
            },
            probe: |conn| run(VIEWS, conn, views),
        },
    ]
}

const FOREIGN_KEYS: &str = "foreign-key actions";
const GENERATED_VIRTUAL: &str = "virtual generated columns";
const GENERATED_STORED: &str = "stored generated columns";
const COLUMN_ON_CONFLICT: &str = "column ON CONFLICT clauses";
const EXPRESSION_DEFAULT: &str = "expression DEFAULTs";
const TYPELESS_COLUMN: &str = "typeless columns";
const TRIGGER_REFIRE: &str = "trigger re-fire";
const DESCENDING_KEY: &str = "INTEGER PRIMARY KEY DESC";
const VIEWS: &str = "views";

/// What a check found, before the case name is attached.
struct Miss {
    probe: &'static str,
    differed: String,
}

impl From<rusqlite::Error> for Miss {
    fn from(error: rusqlite::Error) -> Self {
        miss("sql", error.to_string())
    }
}

fn miss(probe: &'static str, differed: impl Into<String>) -> Miss {
    Miss {
        probe,
        differed: differed.into(),
    }
}

fn run(
    case: &'static str,
    conn: &Connection,
    check: fn(&Connection) -> Result<(), Miss>,
) -> Result<(), ProbeError> {
    check(conn).map_err(|found| ProbeError {
        case,
        probe: found.probe,
        differed: found.differed,
    })
}

fn seed(conn: &Connection, sql: &str) {
    conn.execute_batch(sql)
        .expect("a hazard seed is fixed SQL that fits its schema");
}

fn count(conn: &Connection, sql: &str) -> Result<i64, Miss> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

fn text(conn: &Connection, sql: &str) -> Result<Option<String>, Miss> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

/// Fails when a seeded row is absent, so a probe never passes over an empty database.
fn require_rows(
    conn: &Connection,
    probe: &'static str,
    sql: &str,
    expected: i64,
) -> Result<(), Miss> {
    let found = count(conn, sql)?;
    if found == expected {
        Ok(())
    } else {
        Err(miss(
            probe,
            format!("`{sql}` returned {found}, and the seed should have left {expected}"),
        ))
    }
}

// Foreign-key actions: one parent and one child table per (action, event).

#[derive(Clone, Copy)]
enum Event {
    Delete,
    Update,
}

impl Event {
    fn sql(self) -> &'static str {
        match self {
            Event::Delete => "DELETE",
            Event::Update => "UPDATE",
        }
    }
}

/// What the child row looks like after the parent changes.
#[derive(Clone, Copy)]
enum Expect {
    /// The child row is removed.
    ChildDeleted,
    /// The child row stays and its key is this value, or NULL.
    ChildKey(Option<i64>),
    /// The statement itself is refused.
    RefusedAtStatement,
    /// The statement runs and the commit is refused.
    RefusedAtCommit,
}

struct ForeignKeyRow {
    probe: &'static str,
    slug: &'static str,
    action: &'static str,
    event: Event,
    expect: Expect,
}

impl ForeignKeyRow {
    fn parent(&self) -> String {
        format!(
            "fk_{}_{}_parent",
            self.slug,
            self.event.sql().to_lowercase()
        )
    }

    fn child(&self) -> String {
        format!("fk_{}_{}_child", self.slug, self.event.sql().to_lowercase())
    }
}

const FOREIGN_KEY_ROWS: [ForeignKeyRow; 10] = [
    ForeignKeyRow {
        probe: "ON DELETE CASCADE",
        slug: "cascade",
        action: "CASCADE",
        event: Event::Delete,
        expect: Expect::ChildDeleted,
    },
    ForeignKeyRow {
        probe: "ON UPDATE CASCADE",
        slug: "cascade",
        action: "CASCADE",
        event: Event::Update,
        expect: Expect::ChildKey(Some(MOVED_TO)),
    },
    ForeignKeyRow {
        probe: "ON DELETE SET NULL",
        slug: "set_null",
        action: "SET NULL",
        event: Event::Delete,
        expect: Expect::ChildKey(None),
    },
    ForeignKeyRow {
        probe: "ON UPDATE SET NULL",
        slug: "set_null",
        action: "SET NULL",
        event: Event::Update,
        expect: Expect::ChildKey(None),
    },
    ForeignKeyRow {
        probe: "ON DELETE SET DEFAULT",
        slug: "set_default",
        action: "SET DEFAULT",
        event: Event::Delete,
        expect: Expect::ChildKey(Some(FALLBACK)),
    },
    ForeignKeyRow {
        probe: "ON UPDATE SET DEFAULT",
        slug: "set_default",
        action: "SET DEFAULT",
        event: Event::Update,
        expect: Expect::ChildKey(Some(FALLBACK)),
    },
    ForeignKeyRow {
        probe: "ON DELETE RESTRICT",
        slug: "restrict",
        action: "RESTRICT",
        event: Event::Delete,
        expect: Expect::RefusedAtStatement,
    },
    ForeignKeyRow {
        probe: "ON UPDATE RESTRICT",
        slug: "restrict",
        action: "RESTRICT",
        event: Event::Update,
        expect: Expect::RefusedAtStatement,
    },
    ForeignKeyRow {
        probe: "ON DELETE NO ACTION",
        slug: "no_action",
        action: "NO ACTION",
        event: Event::Delete,
        expect: Expect::RefusedAtCommit,
    },
    ForeignKeyRow {
        probe: "ON UPDATE NO ACTION",
        slug: "no_action",
        action: "NO ACTION",
        event: Event::Update,
        expect: Expect::RefusedAtCommit,
    },
];

/// The parent key every child starts out referencing.
const PARENT: i64 = 1;
/// Where an `ON UPDATE` probe moves the parent key.
const MOVED_TO: i64 = 2;
/// The child's column DEFAULT, and a parent row that exists.
const FALLBACK: i64 = 99;
const CHILD: i64 = 10;

/// Each child is DEFERRABLE INITIALLY DEFERRED so that RESTRICT (refuses at the statement)
/// and NO ACTION (refuses at commit) behave differently.
static FOREIGN_KEY_SCHEMA: LazyLock<String> = LazyLock::new(|| {
    FOREIGN_KEY_ROWS
        .iter()
        .map(|row| {
            let (parent, child) = (row.parent(), row.child());
            format!(
                "CREATE TABLE {parent} (id INTEGER PRIMARY KEY, code TEXT UNIQUE);
                 CREATE TABLE {child} (
                     id INTEGER PRIMARY KEY,
                     parent_id INTEGER DEFAULT {FALLBACK}
                         REFERENCES {parent} (id)
                         ON {} {} DEFERRABLE INITIALLY DEFERRED,
                     code TEXT UNIQUE
                 );
",
                row.event.sql(),
                row.action,
            )
        })
        .collect()
});

static FOREIGN_KEY_SEED: LazyLock<String> = LazyLock::new(|| {
    FOREIGN_KEY_ROWS
        .iter()
        .map(|row| {
            let (parent, child) = (row.parent(), row.child());
            format!(
                "INSERT INTO {parent} (id) VALUES ({PARENT}), ({FALLBACK});
                 INSERT INTO {child} (id, parent_id) VALUES ({CHILD}, {PARENT});
"
            )
        })
        .collect()
});

fn foreign_keys(conn: &Connection) -> Result<(), Miss> {
    if count(conn, "PRAGMA foreign_keys")? != 1 {
        return Err(miss(
            "enforcement",
            "foreign key enforcement is off, so no action can show",
        ));
    }
    FOREIGN_KEY_ROWS
        .iter()
        .try_for_each(|row| foreign_key_row(conn, row))
}

fn foreign_key_row(conn: &Connection, row: &ForeignKeyRow) -> Result<(), Miss> {
    let (parent, child) = (row.parent(), row.child());
    require_rows(
        conn,
        row.probe,
        &format!("SELECT count(*) FROM {child} WHERE parent_id = {PARENT}"),
        1,
    )?;

    let change = match row.event {
        Event::Delete => format!("DELETE FROM {parent} WHERE id = {PARENT}"),
        Event::Update => format!("UPDATE {parent} SET id = {MOVED_TO} WHERE id = {PARENT}"),
    };
    conn.execute_batch("BEGIN")?;
    let ran = conn.execute(&change, []).map(|_| ());
    let committed = match &ran {
        Ok(()) => conn.execute_batch("COMMIT"),
        Err(_) => Ok(()),
    };
    if !conn.is_autocommit() {
        conn.execute_batch("ROLLBACK")?;
    }

    let refused = |error: &rusqlite::Error| {
        format!(
            "ON {} {} refused the change ({error}); the action was dropped or became another one",
            row.event.sql(),
            row.action
        )
    };
    match row.expect {
        Expect::RefusedAtStatement => {
            if ran.is_ok() {
                return Err(miss(
                    row.probe,
                    format!("`{change}` ran; RESTRICT refuses at the statement"),
                ));
            }
        }
        Expect::RefusedAtCommit => {
            if let Err(error) = &ran {
                return Err(miss(
                    row.probe,
                    format!(
                        "`{change}` was refused at the statement ({error}); NO ACTION waits for the commit"
                    ),
                ));
            }
            if committed.is_ok() {
                return Err(miss(
                    row.probe,
                    format!("`{change}` committed with a child still referencing the parent"),
                ));
            }
        }
        Expect::ChildDeleted | Expect::ChildKey(_) => {
            if let Err(error) = &ran {
                return Err(miss(row.probe, refused(error)));
            }
            if let Err(error) = &committed {
                return Err(miss(row.probe, refused(error)));
            }
        }
    }

    match row.expect {
        Expect::ChildDeleted => {
            require_rows(conn, row.probe, &format!("SELECT count(*) FROM {child}"), 0)
        }
        Expect::ChildKey(key) => {
            let predicate = match key {
                Some(value) => format!("parent_id = {value}"),
                None => "parent_id IS NULL".to_string(),
            };
            require_rows(
                conn,
                row.probe,
                &format!("SELECT count(*) FROM {child} WHERE id = {CHILD} AND {predicate}"),
                1,
            )
        }
        Expect::RefusedAtStatement | Expect::RefusedAtCommit => require_rows(
            conn,
            row.probe,
            &format!("SELECT count(*) FROM {child} WHERE parent_id = {PARENT}"),
            1,
        ),
    }
}

// Generated columns.

/// Reads the generated column, moves its base, and reads it again. One read cannot tell a
/// generated column from an ordinary one holding the number a rebuild copied into it.
fn generated(
    conn: &Connection,
    table: &str,
    column: &str,
    (base, computed): (i64, i64),
    (moved_base, moved_computed): (i64, i64),
) -> Result<(), Miss> {
    const PROBE: &str = "generated value";
    require_rows(
        conn,
        PROBE,
        &format!("SELECT count(*) FROM {table} WHERE id = 1 AND base = {base}"),
        1,
    )?;
    let read = || count(conn, &format!("SELECT {column} FROM {table} WHERE id = 1"));
    let first = read()?;
    if first != computed {
        return Err(miss(
            PROBE,
            format!("{table}.{column} reads {first} for a base of {base}, not {computed}"),
        ));
    }
    conn.execute(
        &format!("UPDATE {table} SET base = {moved_base} WHERE id = 1"),
        [],
    )?;
    let second = read()?;
    if second != moved_computed {
        return Err(miss(
            PROBE,
            format!(
                "{table}.{column} reads {second} after its base moved to {moved_base}, not \
                 {moved_computed}; the value was copied but the computation was not"
            ),
        ));
    }
    Ok(())
}

fn generated_virtual(conn: &Connection) -> Result<(), Miss> {
    generated(conn, "generated_virtual", "doubled", (21, 42), (9, 18))
}

fn generated_stored(conn: &Connection) -> Result<(), Miss> {
    generated(conn, "generated_stored", "tripled", (5, 15), (7, 21))
}

// Column ON CONFLICT clauses.

fn column_on_conflict(conn: &Connection) -> Result<(), Miss> {
    for table in [
        "conflict_replace",
        "conflict_ignore",
        "conflict_abort",
        "conflict_rollback",
    ] {
        require_rows(conn, "seed", &format!("SELECT count(*) FROM {table}"), 1)?;
    }
    conflict_replace(conn)?;
    conflict_ignore(conn)?;
    conflict_abort(conn)?;
    conflict_rollback(conn)
}

fn conflict_replace(conn: &Connection) -> Result<(), Miss> {
    const PROBE: &str = "ON CONFLICT REPLACE";
    if let Err(error) = conn.execute(
        "INSERT INTO conflict_replace (id, k, label) VALUES (2, 'taken', 'probed')",
        [],
    ) {
        return Err(miss(
            PROBE,
            format!("the conflicting insert was thrown ({error}); REPLACE swaps the old row out"),
        ));
    }
    let rows = count(conn, "SELECT count(*) FROM conflict_replace")?;
    let label = text(conn, "SELECT label FROM conflict_replace")?;
    if rows != 1 || label.as_deref() != Some("probed") {
        return Err(miss(
            PROBE,
            format!("{rows} row(s) remain, labelled {label:?}; REPLACE leaves the one probed row"),
        ));
    }
    Ok(())
}

fn conflict_ignore(conn: &Connection) -> Result<(), Miss> {
    const PROBE: &str = "ON CONFLICT IGNORE";
    if let Err(error) = conn.execute("INSERT INTO conflict_ignore (id, v) VALUES (2, NULL)", []) {
        return Err(miss(
            PROBE,
            format!("inserting NULL was thrown ({error}); IGNORE skips the row"),
        ));
    }
    let rows = count(conn, "SELECT count(*) FROM conflict_ignore")?;
    if rows != 1 {
        return Err(miss(
            PROBE,
            format!("{rows} rows after inserting NULL; IGNORE skips the row"),
        ));
    }
    Ok(())
}

fn conflict_abort(conn: &Connection) -> Result<(), Miss> {
    const PROBE: &str = "ON CONFLICT ABORT";
    if conn
        .execute("INSERT INTO conflict_abort (id, v) VALUES (2, NULL)", [])
        .is_ok()
    {
        return Err(miss(PROBE, "inserting NULL was absorbed; ABORT throws it"));
    }
    let rows = count(conn, "SELECT count(*) FROM conflict_abort")?;
    if rows != 1 {
        return Err(miss(
            PROBE,
            format!("{rows} rows after a thrown insert; ABORT undoes the statement"),
        ));
    }
    Ok(())
}

/// ABORT and ROLLBACK both throw. They differ in reach: ROLLBACK ends the enclosing
/// transaction and takes earlier work in it along.
fn conflict_rollback(conn: &Connection) -> Result<(), Miss> {
    const PROBE: &str = "ON CONFLICT ROLLBACK";
    conn.execute_batch("BEGIN")?;
    let observed = (|| -> Result<(bool, bool, i64), Miss> {
        conn.execute(
            "INSERT INTO conflict_rollback (id, v) VALUES (2, 'in the transaction')",
            [],
        )?;
        let thrown = conn
            .execute("INSERT INTO conflict_rollback (id, v) VALUES (3, NULL)", [])
            .is_err();
        let ended = conn.is_autocommit();
        let rows = count(conn, "SELECT count(*) FROM conflict_rollback")?;
        Ok((thrown, ended, rows))
    })();
    if !conn.is_autocommit() {
        conn.execute_batch("ROLLBACK")?;
    }
    let (thrown, ended, rows) = observed?;
    if !thrown {
        return Err(miss(
            PROBE,
            "inserting NULL was absorbed; ROLLBACK throws it",
        ));
    }
    if !ended {
        return Err(miss(
            PROBE,
            "the transaction was still open after the thrown insert; ROLLBACK ends it",
        ));
    }
    if rows != 1 {
        return Err(miss(
            PROBE,
            format!(
                "{rows} rows remain; the row inserted earlier in the transaction should be gone"
            ),
        ));
    }
    Ok(())
}

// Expression DEFAULTs.

/// The shape `datetime('now')` renders, as a GLOB.
const TIMESTAMP_SHAPE: &str =
    "[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9]";

fn expression_default(conn: &Connection) -> Result<(), Miss> {
    const PROBE: &str = "evaluated default";
    require_rows(
        conn,
        PROBE,
        "SELECT count(*) FROM expression_default WHERE id = 1",
        1,
    )?;
    conn.execute("INSERT INTO expression_default (id) VALUES (2)", [])?;

    let stamped = count(
        conn,
        &format!("SELECT count(*) FROM expression_default WHERE made_at GLOB '{TIMESTAMP_SHAPE}'"),
    )?;
    if stamped != 2 {
        let seen = text(
            conn,
            "SELECT made_at FROM expression_default ORDER BY id DESC",
        )?;
        return Err(miss(
            PROBE,
            format!("{stamped} of 2 rows hold a timestamp in made_at (latest {seen:?})"),
        ));
    }
    let summed = count(
        conn,
        "SELECT count(*) FROM expression_default \
         WHERE typeof(computed) = 'integer' AND computed = 5",
    )?;
    if summed != 2 {
        return Err(miss(
            PROBE,
            format!("{summed} of 2 rows hold the integer 5 in computed"),
        ));
    }
    Ok(())
}

// Typeless columns.

fn typeless_column(conn: &Connection) -> Result<(), Miss> {
    require_rows(
        conn,
        "stored type",
        "SELECT count(*) FROM typeless_value WHERE id IN (1, 2)",
        2,
    )?;
    let kind = |id: i64| {
        text(
            conn,
            &format!("SELECT typeof(v) FROM typeless_value WHERE id = {id}"),
        )
    };
    let (string, integer) = (kind(1)?, kind(2)?);
    if string.as_deref() != Some("text") || integer.as_deref() != Some("integer") {
        return Err(miss(
            "stored type",
            format!(
                "typeof() reads {string:?} for a string and {integer:?} for an integer; a \
                 typeless column keeps each value as it is"
            ),
        ));
    }
    // A column promoted to BLOB keeps typeless storage behaviour, so only the declared type
    // shows the promotion.
    let declared = text(
        conn,
        "SELECT type FROM pragma_table_info('typeless_value') WHERE name = 'v'",
    )?;
    if declared.as_deref() != Some("") {
        return Err(miss(
            "declared type",
            format!("v declares the type {declared:?}, and it was declared with none"),
        ));
    }
    Ok(())
}

// Trigger re-fire.

/// The seed inserted `before` with the trigger live, so the audit holds one row. One more
/// insert must add exactly one; a rebuild that re-created the trigger before copying rows
/// audits `before` a second time.
fn trigger_refire(conn: &Connection) -> Result<(), Miss> {
    const PROBE: &str = "audit rows";
    require_rows(conn, PROBE, "SELECT count(*) FROM evented WHERE id = 1", 1)?;
    conn.execute("INSERT INTO evented (id, note) VALUES (2, 'after')", [])?;
    let audited: Vec<String> = conn
        .prepare("SELECT note FROM audit ORDER BY id")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    if audited != ["before", "after"] {
        return Err(miss(
            PROBE,
            format!(
                "audit holds {audited:?}, and should hold [\"before\", \"after\"]; fewer means \
                 the trigger never fired, more means it fired over rows the rebuild copied"
            ),
        ));
    }
    Ok(())
}

// INTEGER PRIMARY KEY DESC.

/// An INTEGER PRIMARY KEY is the rowid; with DESC it is an ordinary key and the table keeps
/// a rowid of its own, so nothing allocates a key.
fn descending_key(conn: &Connection) -> Result<(), Miss> {
    const PROBE: &str = "key is not the rowid";
    require_rows(
        conn,
        PROBE,
        "SELECT count(*) FROM descending_key WHERE id IN (100, 200)",
        2,
    )?;
    let aliased = count(conn, "SELECT count(*) FROM descending_key WHERE rowid = id")?;
    if aliased != 0 {
        return Err(miss(
            PROBE,
            format!(
                "{aliased} row(s) have a key equal to their rowid; DESC makes the key an ordinary column"
            ),
        ));
    }
    conn.execute(
        "INSERT INTO descending_key (label) VALUES ('no key given')",
        [],
    )?;
    let assigned = text(
        conn,
        "SELECT typeof(id) FROM descending_key WHERE label = 'no key given'",
    )?;
    if assigned.as_deref() != Some("null") {
        return Err(miss(
            PROBE,
            format!(
                "an insert without a key left {assigned:?} in id; only a rowid alias allocates one"
            ),
        ));
    }
    Ok(())
}

// Views.

fn views(conn: &Connection) -> Result<(), Miss> {
    require_rows(
        conn,
        "view read",
        "SELECT count(*) FROM view_base WHERE id = 1",
        1,
    )?;
    let shout = text(conn, "SELECT shout FROM view_shout WHERE id = 1")?;
    if shout.as_deref() != Some("ANN") {
        return Err(miss(
            "view read",
            format!("view_shout reads {shout:?} for id 1, not \"ANN\""),
        ));
    }
    if let Err(error) = conn.execute("INSERT INTO view_shout (id, shout) VALUES (2, 'BOB')", []) {
        return Err(miss(
            "instead-of trigger",
            format!("inserting through the view failed ({error})"),
        ));
    }
    let written = text(conn, "SELECT name FROM view_base WHERE id = 2")?;
    if written.as_deref() != Some("bob") {
        return Err(miss(
            "instead-of trigger",
            format!("the insert through the view wrote {written:?} to view_base, not \"bob\""),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_names_are_distinct_and_cover_the_nine_hazards() {
        let names: Vec<&str> = cases().iter().map(|case| case.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names.len(), 9);
        assert_eq!(sorted.len(), 9);
    }

    #[test]
    fn a_probe_error_names_the_case_the_probe_and_what_differed() {
        let error = ProbeError {
            case: "views",
            probe: "view read",
            differed: "reads NULL".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "views: probe `view read` failed: reads NULL"
        );
    }

    #[test]
    fn every_foreign_key_row_has_its_own_table_pair() {
        let mut tables: Vec<String> = FOREIGN_KEY_ROWS
            .iter()
            .flat_map(|row| [row.parent(), row.child()])
            .collect();
        tables.sort();
        tables.dedup();
        assert_eq!(tables.len(), FOREIGN_KEY_ROWS.len() * 2);
    }
}
