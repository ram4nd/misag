//! misag's SQLite store (`misag.db` in the app data directory): the open tabs,
//! app state, and the Claude Code session each Claude tab is currently in.
//!
//! The app and the Claude session hook (`misag --claude-hook`, one process per
//! event) write concurrently, so the database runs in WAL mode with a busy timeout.
//! Only the app migrates the schema; the hook refuses to touch a database whose
//! schema it does not know exactly.

use crate::claude::SessionId;
use crate::flows::{self, Graph};
use crate::runs;
use crate::layout::{Layout, Tab, TabId, TabKind};
use crate::settings::{FinishSound, Settings};
use rusqlite::{params, Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const FILE: &str = "misag.db";
/// The database of raterm, misag's old name, which misag copies on its first start.
pub const RATERM_FILE: &str = "raterm.db";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Applied in order; `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[r#"
    CREATE TABLE tabs (
        position       INTEGER PRIMARY KEY,
        tab_id         TEXT NOT NULL UNIQUE,
        kind           TEXT NOT NULL CHECK (kind IN ('terminal', 'claude')),
        name           TEXT NOT NULL,
        renamed        INTEGER NOT NULL DEFAULT 0,
        cwd            TEXT,
        command        TEXT,
        claude_session TEXT
    );
    -- Key/value app state, such as `active_tab`.
    CREATE TABLE app_state (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    -- Written by the SessionStart hook whenever a tab's Claude session starts or changes.
    CREATE TABLE claude_sessions (
        tab_id          TEXT PRIMARY KEY,
        session_id      TEXT NOT NULL,
        source          TEXT,
        cwd             TEXT,
        transcript_path TEXT,
        updated_at      INTEGER NOT NULL
    );
"#, r#"
    -- The user's settings: one row, one column per setting. A new setting is a new
    -- migration adding a column with a default; like every schema change, it makes
    -- older misag versions refuse the database.
    CREATE TABLE settings (
        id               INTEGER PRIMARY KEY CHECK (id = 1),
        skip_permissions INTEGER NOT NULL DEFAULT 0 CHECK (skip_permissions IN (0, 1))
    );
    INSERT INTO settings (id) VALUES (1);
"#, r#"
    ALTER TABLE settings ADD COLUMN workspace TEXT;
"#, r#"
    -- The Claude process that reported, so `session_run_by` can tell whether a
    -- terminal tab still has that session open.
    ALTER TABLE claude_sessions ADD COLUMN claude_pid INTEGER;
"#, r#"
    -- Whether Claude tabs get `autoMemoryEnabled: false`; 0 leaves it to the user's config.
    ALTER TABLE settings ADD COLUMN disable_auto_memory INTEGER NOT NULL DEFAULT 0
        CHECK (disable_auto_memory IN (0, 1));
"#, r#"
    -- The user's flows of prompts, in the order the sidebar lists them. `graph` is
    -- the flow's steps as JSON. Runs of flows get their own tables with the runner.
    CREATE TABLE flows (
        id         INTEGER PRIMARY KEY,
        name       TEXT NOT NULL CHECK (trim(name) <> ''),
        position   INTEGER NOT NULL,
        archived   INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
        graph      TEXT NOT NULL DEFAULT '{"nodes":[],"edges":[]}',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
"#, r#"
    -- A flow being worked through in a tab (see runs.rs), with its own copy of the
    -- flow's steps, so editing the flow does not change a run. Deleting a flow
    -- deletes its runs, but never while one is active (see `delete_flow`).
    CREATE TABLE flow_runs (
        id         INTEGER PRIMARY KEY,
        flow_id    INTEGER NOT NULL REFERENCES flows (id) ON DELETE CASCADE,
        tab_id     TEXT NOT NULL,
        graph      TEXT NOT NULL,
        node_id    TEXT NOT NULL,
        status     TEXT NOT NULL CHECK (status IN ('running', 'manual', 'attention', 'done', 'stopped')),
        -- Steps taken since a person last stepped in; see runs::MAX_RUN_STEPS.
        steps      INTEGER NOT NULL DEFAULT 0,
        -- Why the run needs attention.
        note       TEXT,
        started_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    -- A tab has at most one active run.
    CREATE UNIQUE INDEX one_active_run_per_tab ON flow_runs (tab_id)
        WHERE status IN ('running', 'manual', 'attention');
    CREATE INDEX flow_runs_by_flow ON flow_runs (flow_id);
    -- The steps a run took: the step it left and the output it left by.
    CREATE TABLE flow_run_events (
        id      INTEGER PRIMARY KEY,
        run_id  INTEGER NOT NULL REFERENCES flow_runs (id) ON DELETE CASCADE,
        node_id TEXT NOT NULL,
        output  TEXT NOT NULL,
        at      INTEGER NOT NULL
    );
    CREATE INDEX flow_run_events_by_run ON flow_run_events (run_id);
"#, r#"
    -- The Claude sessions pinned to the top of the Session history box, which
    -- orders them by activity; `pinned_at` is only kept for the record.
    CREATE TABLE pinned_sessions (
        session_id TEXT PRIMARY KEY,
        pinned_at  INTEGER NOT NULL
    );
"#, r#"
    -- The terminals' font size in pixels (see settings::FONT_SIZES).
    ALTER TABLE settings ADD COLUMN font_size INTEGER NOT NULL DEFAULT 14
        CHECK (font_size BETWEEN 8 AND 32);
"#, r#"
    -- How Continue goes on with a run that needs attention (see runs::Resume);
    -- NULL when it cannot help, and the person picks an output.
    ALTER TABLE flow_runs ADD COLUMN resume TEXT CHECK (resume IN ('remind', 'prompt'));
"#, r#"
    -- The times a person had Claude go on with a step that needed attention
    -- (Continue), and why it waited. Apart from flow_run_events, as going on is
    -- not another run of the step (see runs::runs_of).
    CREATE TABLE flow_run_resumes (
        id      INTEGER PRIMARY KEY,
        run_id  INTEGER NOT NULL REFERENCES flow_runs (id) ON DELETE CASCADE,
        node_id TEXT NOT NULL,
        note    TEXT,
        at      INTEGER NOT NULL
    );
    CREATE INDEX flow_run_resumes_by_run ON flow_run_resumes (run_id);
"#, r#"
    -- For a run whose Start clears, while it waits at Start for Claude's /clear
    -- (see runs::Clear); NULL otherwise.
    ALTER TABLE flow_runs ADD COLUMN clear TEXT CHECK (clear IN ('pending', 'done'));
"#, r#"
    -- 1 for a flow a program in a tab added (`misag flow import` with MISAG_TAB set),
    -- until the person confirms it in the app; runs::start refuses it until then.
    ALTER TABLE flows ADD COLUMN unconfirmed INTEGER NOT NULL DEFAULT 0 CHECK (unconfirmed IN (0, 1));
"#, r#"
    -- 0 while Claude lacks Start's base info: from a run's start, and after each
    -- /clear in its tab, until a prompt carries it (see runs::go). Runs started
    -- before base info sent none, and owe none.
    ALTER TABLE flow_runs ADD COLUMN briefed INTEGER NOT NULL DEFAULT 1 CHECK (briefed IN (0, 1));
    -- Start's text was neither shown nor sent before base info, so a flow holding
    -- some may send Claude what no one has read: it asks before its next run.
    UPDATE flows SET unconfirmed = 1
    WHERE json_valid(graph) AND COALESCE(json_extract(graph, '$.version'), 1) < 4 AND EXISTS (
        SELECT 1 FROM json_each(graph, '$.nodes')
        WHERE json_extract(value, '$.kind') = 'start' AND trim(COALESCE(json_extract(value, '$.text'), '')) <> ''
    );
"#, r#"
    -- When (unix ms) the tab's session last ended a turn with background tasks in
    -- flight, which wake Claude again; NULL while it waits on none. Set and cleared
    -- by the Stop hook, cleared by SessionStart, SessionEnd and startup pruning
    -- (see claude::stopped).
    ALTER TABLE claude_sessions ADD COLUMN background_at INTEGER;
"#, r#"
    -- The sound played when Claude in a tab finishes its turn (see settings::FinishSound).
    ALTER TABLE settings ADD COLUMN finish_sound TEXT NOT NULL DEFAULT 'off'
        CHECK (finish_sound IN ('off', 'chime', 'bell', 'pop'));
"#, r#"
    -- 1 while a run waits at a manual prompt whose text the app has yet to put into
    -- Claude's input (see runs::prefill_text).
    ALTER TABLE flow_runs ADD COLUMN prefill INTEGER NOT NULL DEFAULT 0 CHECK (prefill IN (0, 1));
"#, r#"
    -- Names the paste a run owes while prefill is 1, so the record of an earlier
    -- paste does not cancel one owed since (see runs::prefilled).
    ALTER TABLE flow_runs ADD COLUMN prefill_token INTEGER;
"#, r#"
    -- 1 while a run waits at Start for Claude's reply to its briefing (see
    -- runs::on_stop): only that reply takes the first step.
    ALTER TABLE flow_runs ADD COLUMN briefing INTEGER NOT NULL DEFAULT 0 CHECK (briefing IN (0, 1));
"#, r#"
    -- 1 on a finished or stopped run whose Claude has yet to be told the flow is
    -- finished, which comes with the person's next prompt (see runs::on_prompt).
    ALTER TABLE flow_runs ADD COLUMN closing INTEGER NOT NULL DEFAULT 0 CHECK (closing IN (0, 1));
    -- Every prompt in every tab looks for a note owed there.
    CREATE INDEX flow_runs_closing ON flow_runs (tab_id) WHERE closing = 1;
"#, r#"
    -- When the run sent its briefing, which its timeout counts from (see
    -- runs::expire), so other writes to the run do not restart it. It only counts
    -- while briefing = 1.
    ALTER TABLE flow_runs ADD COLUMN briefing_at INTEGER;
    UPDATE flow_runs SET briefing_at = updated_at WHERE briefing = 1;
"#, r#"
    -- The cross-column rules of a run's state (runs::Phase), so every writer keeps
    -- them, for active runs only: an ended run is read by `closing` alone, and a raw
    -- stop (of a run a version cannot read) leaves the rest as it was.
    --
    -- First, in this transaction, `normalise_runs` makes the rows older versions
    -- left fit them (see apply_migrations).
    --
    -- SQLite cannot add a CHECK to a table, so flow_runs is rebuilt (see the
    -- foreign_keys handling in `migrate` for the foreign keys of flow_run_events
    -- and flow_run_resumes, which name it).
    CREATE TABLE flow_runs_new (
        id            INTEGER PRIMARY KEY,
        flow_id       INTEGER NOT NULL REFERENCES flows (id) ON DELETE CASCADE,
        tab_id        TEXT NOT NULL,
        graph         TEXT NOT NULL,
        node_id       TEXT NOT NULL,
        status        TEXT NOT NULL CHECK (status IN ('running', 'manual', 'attention', 'done', 'stopped')),
        -- Steps taken since a person last stepped in; see runs::MAX_RUN_STEPS.
        steps         INTEGER NOT NULL DEFAULT 0,
        -- Why the run needs attention.
        note          TEXT,
        started_at    INTEGER NOT NULL,
        updated_at    INTEGER NOT NULL,
        -- How Continue goes on with a run that needs attention (see runs::Resume);
        -- NULL when it cannot help, and the person picks an output.
        resume        TEXT CHECK (resume IN ('remind', 'prompt')),
        -- For a run whose Start clears, while it waits at Start for Claude's /clear
        -- (see runs::Clear); NULL otherwise.
        clear         TEXT CHECK (clear IN ('pending', 'done')),
        -- 0 while Claude lacks Start's base info: from a run's start, and after each
        -- /clear in its tab, until a prompt carries it (see runs::go).
        briefed       INTEGER NOT NULL DEFAULT 1 CHECK (briefed IN (0, 1)),
        -- 1 while a run waits at a manual prompt whose text the app has yet to put
        -- into Claude's input (see runs::prefill_text).
        prefill       INTEGER NOT NULL DEFAULT 0 CHECK (prefill IN (0, 1)),
        -- Names the paste a run owes while prefill is 1, so the record of an earlier
        -- paste does not cancel one owed since (see runs::prefilled).
        prefill_token INTEGER,
        -- 1 while a run waits at Start for Claude's reply to its briefing (see
        -- runs::on_stop): only that reply takes the first step.
        briefing      INTEGER NOT NULL DEFAULT 0 CHECK (briefing IN (0, 1)),
        -- 1 on a finished or stopped run whose Claude has yet to be told the flow is
        -- finished, which comes with the person's next prompt (see runs::on_prompt).
        closing       INTEGER NOT NULL DEFAULT 0 CHECK (closing IN (0, 1)),
        -- When the run sent its briefing, which its timeout counts from (see
        -- runs::expire), so other writes to the run do not restart it.
        briefing_at   INTEGER,
        -- A run needs attention (NeedsPerson) exactly when it says why.
        CONSTRAINT note_only_when_attention
            CHECK (status IN ('done', 'stopped') OR (note IS NOT NULL) = (status = 'attention')),
        CONSTRAINT resume_only_when_attention
            CHECK (status IN ('done', 'stopped') OR resume IS NULL OR status = 'attention'),
        -- Clearing and Briefing are at Start, running, and not both at once (a
        -- briefing also needs the time it was sent).
        CONSTRAINT clear_only_when_running
            CHECK (status IN ('done', 'stopped') OR clear IS NULL OR status = 'running'),
        CONSTRAINT briefing_only_when_running
            CHECK (status IN ('done', 'stopped') OR briefing = 0
                   OR (status = 'running' AND clear IS NULL AND briefing_at IS NOT NULL)),
        CONSTRAINT briefing_at_only_when_briefing
            CHECK (status IN ('done', 'stopped') OR briefing = 1 OR briefing_at IS NULL),
        -- A paste is owed only at a manual prompt (Waiting, NeedsPerson).
        CONSTRAINT prefill_only_when_waiting
            CHECK (status IN ('done', 'stopped') OR prefill = 0 OR status IN ('manual', 'attention')),
        -- The note that a flow is finished is owed only by an ended run.
        CONSTRAINT closing_only_when_ended
            CHECK (closing = 0 OR status IN ('done', 'stopped'))
    );
    INSERT INTO flow_runs_new (id, flow_id, tab_id, graph, node_id, status, steps, note, started_at, updated_at,
                               resume, clear, briefed, prefill, prefill_token, briefing, closing, briefing_at)
    SELECT id, flow_id, tab_id, graph, node_id, status, steps, note, started_at, updated_at,
           resume, clear, briefed, prefill, prefill_token, briefing, closing, briefing_at
    FROM flow_runs;
    DROP TABLE flow_runs;
    ALTER TABLE flow_runs_new RENAME TO flow_runs;
    CREATE UNIQUE INDEX one_active_run_per_tab ON flow_runs (tab_id)
        WHERE status IN ('running', 'manual', 'attention');
    CREATE INDEX flow_runs_by_flow ON flow_runs (flow_id);
    CREATE INDEX flow_runs_closing ON flow_runs (tab_id) WHERE closing = 1;
"#];

/// The index in MIGRATIONS of the migration adding flow_runs' CHECKs, which
/// `normalise_runs` precedes.
const RUN_CHECKS: usize = 21;

/// Makes the rows older versions left fit flow_runs' CHECKs, before the migration
/// adding them; the ids of the active runs it held for the person, which SQL
/// cannot report, so it is Rust's.
fn normalise_runs(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    // First, rows from_columns still accepts but that carry a stale briefing_at get it cleared.
    conn.execute(
        "UPDATE flow_runs SET briefing_at = NULL
         WHERE status IN ('running', 'manual', 'attention') AND briefing = 0 AND briefing_at IS NOT NULL",
        [],
    )?;
    // Then the active rows it refuses wait for the person where they are, keeping
    // the steps, what Claude has (briefed), the last paste's token and a note a run
    // needing attention had: the person sees the run and stops it, or picks an
    // output if they know where it was.
    let mut stmt = conn.prepare(
        "UPDATE flow_runs SET
             status = 'attention',
             note = 'This run could not be read after an update, so it waits here. Stop it, or pick an output if you know where it was.'
                 || CASE WHEN status = 'attention' AND trim(COALESCE(note, '')) <> '' THEN ' (it said: ' || note || ')' ELSE '' END,
             resume = NULL, clear = NULL, prefill = 0, briefing = 0, briefing_at = NULL, closing = 0
         WHERE status IN ('running', 'manual', 'attention') AND NOT (
             (note IS NOT NULL) = (status = 'attention')
             AND (resume IS NULL OR status = 'attention')
             AND (clear IS NULL OR status = 'running')
             AND (briefing = 0 OR (status = 'running' AND clear IS NULL AND briefing_at IS NOT NULL))
             AND (briefing = 1 OR briefing_at IS NULL)
             AND (prefill = 0 OR status IN ('manual', 'attention'))
             AND closing = 0
         )
         RETURNING id",
    )?;
    let mut held = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
    held.sort_unstable();
    Ok(held)
}

/// A row whose foreign key leads nowhere, as `PRAGMA foreign_key_check` reports it.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
struct Orphan {
    table: String,
    /// None for a table without rowids.
    rowid: Option<i64>,
    /// The table the key names.
    parent: String,
}

impl fmt::Display for Orphan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.rowid {
            Some(rowid) => write!(f, "{} row {rowid} (no {} row)", self.table, self.parent),
            None => write!(f, "a {} row (no {} row)", self.table, self.parent),
        }
    }
}

fn orphans(conn: &Connection) -> rusqlite::Result<Vec<Orphan>> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let found = stmt.query_map([], |r| Ok(Orphan { table: r.get(0)?, rowid: r.get(1)?, parent: r.get(2)? }))?;
    found.collect()
}

fn list(orphans: &[Orphan]) -> String {
    orphans.iter().map(Orphan::to_string).collect::<Vec<_>>().join(", ")
}

/// Why the database could not be opened. Only `Corrupt` justifies replacing the file.
#[derive(Debug, PartialEq)]
pub enum OpenError {
    Corrupt(String),
    /// Written by a newer misag; this version must not touch it.
    TooNew { found: i64, known: i64 },
    Other(String),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Corrupt(e) => write!(f, "the database is damaged ({e})"),
            OpenError::TooNew { found, known } => write!(
                f,
                "the database was written by a newer Mischief of Agents (schema {found}, this version knows {known})"
            ),
            OpenError::Other(e) => write!(f, "the database could not be opened ({e})"),
        }
    }
}

fn classify(what: &str, e: rusqlite::Error) -> OpenError {
    let corrupt = matches!(
        &e,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)
    );
    let message = format!("{what}: {e}");
    if corrupt { OpenError::Corrupt(message) } else { OpenError::Other(message) }
}

/// Runs are deleted with their flow by foreign keys, which SQLite enforces only
/// when each connection asks.
fn enforce_foreign_keys(conn: &Connection) -> Result<(), OpenError> {
    conn.pragma_update(None, "foreign_keys", true).map_err(|e| classify("foreign keys", e))
}

fn configure(conn: &Connection, path: &Path) -> Result<(), OpenError> {
    conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| classify("busy timeout", e))?;
    enforce_foreign_keys(conn)?;
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "wal", |r| r.get(0))
        .map_err(|e| classify("journal mode", e))?;
    if mode != "wal" {
        eprintln!("misag: {} runs in {mode} journal mode, not WAL; concurrent writes may wait", path.display());
    }
    Ok(())
}

fn schema_version(conn: &Connection) -> Result<i64, OpenError> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|e| classify("schema version", e))
}

/// Opens (creating if needed) and migrates the database at `path`. App side only.
pub fn connect(path: &Path) -> Result<Connection, OpenError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| OpenError::Other(format!("could not create {}: {e}", dir.display())))?;
    }
    let mut conn = Connection::open(path).map_err(|e| classify(&format!("open {}", path.display()), e))?;
    configure(&conn, path)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// Opens an existing database without creating or migrating it, and only if its
/// schema is exactly the one this binary knows. Used by the hook and by `misag
/// flow`, either of which may be a different misag version than the running app.
/// Across an upgrade that adds a migration, a new hook therefore refuses the old
/// schema (and logs it) until the app has migrated: session changes in that window
/// are not recorded.
pub fn connect_existing(path: &Path) -> Result<Connection, String> {
    let known = MIGRATIONS.len() as i64;
    connect_if_current(path)?.ok_or_else(|| {
        let found = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()
            .and_then(|conn| schema_version(&conn).ok())
            .map_or_else(|| "?".to_string(), |v| v.to_string());
        format!("schema {found} in {}, this binary knows {known}", path.display())
    })
}

/// Like `connect_existing`, but None (no error) when the schema is not this
/// binary's, for callers that have nothing to do across an upgrade.
pub fn connect_if_current(path: &Path) -> Result<Option<Connection>, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    configure(&conn, path).map_err(|e| e.to_string())?;
    let found = schema_version(&conn).map_err(|e| e.to_string())?;
    Ok((found == MIGRATIONS.len() as i64).then_some(conn))
}

fn migrate(conn: &mut Connection) -> Result<(), OpenError> {
    migrate_with(conn, MIGRATIONS)
}

fn migrate_with(conn: &mut Connection, migrations: &[&str]) -> Result<(), OpenError> {
    let known = migrations.len() as i64;
    // Usually current: then no write lock is taken, so startup never waits on the hook.
    if schema_version(conn)? == known {
        return Ok(());
    }
    // A migration that rebuilds a table drops it, which with foreign keys enforced
    // would delete the rows of the tables naming it (ON DELETE CASCADE). So, as
    // SQLite's ALTER TABLE docs prescribe, they are off while migrating (which only
    // takes outside a transaction), each migration checks them before it commits,
    // and they are back on after, however migrating ends.
    conn.pragma_update(None, "foreign_keys", false).map_err(|e| classify("foreign keys", e))?;
    let migrated = apply_migrations(conn, migrations);
    let enforced = enforce_foreign_keys(conn);
    if let (Err(_), Err(e)) = (&migrated, &enforced) {
        // The migration's error says what went wrong; this one only follows.
        eprintln!("misag: after a failed migration, {e}");
        return migrated;
    }
    migrated.and(enforced)
}

fn apply_migrations(conn: &mut Connection, migrations: &[&str]) -> Result<(), OpenError> {
    let known = migrations.len() as i64;
    let mut logged = Vec::new();
    loop {
        // IMMEDIATE takes the write lock first, so two processes cannot both apply
        // the same migration; the version is re-read under that lock.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| classify("migration lock", e))?;
        let done = schema_version(&tx)?;
        if done > known {
            return Err(OpenError::TooNew { found: done, known });
        }
        if done == known {
            return Ok(());
        }
        let number = done + 1;
        // Rows that already lead nowhere are logged, not fatal: they would otherwise
        // keep the database from ever being upgraded. Only new ones fail the migration.
        let before = orphans(&tx).map_err(|e| classify("foreign key check", e))?;
        if !before.is_empty() && before != logged {
            eprintln!("misag: before migration {number}, rows' foreign keys lead nowhere: {}", list(&before));
            logged.clone_from(&before);
        }
        if done as usize == RUN_CHECKS {
            let held = normalise_runs(&tx).map_err(|e| classify(&format!("migration {number}"), e))?;
            if !held.is_empty() {
                eprintln!("misag: migration {number} holds {} active runs it cannot read for the person: ids {held:?}", held.len());
            }
        }
        tx.execute_batch(migrations[done as usize]).map_err(|e| classify(&format!("migration {number}"), e))?;
        let before: HashSet<_> = before.into_iter().collect();
        let new: Vec<_> = orphans(&tx)
            .map_err(|e| classify("foreign key check", e))?
            .into_iter()
            .filter(|o| !before.contains(o))
            .collect();
        if !new.is_empty() {
            eprintln!("misag: migration {number} would leave rows whose foreign keys lead nowhere: {}", list(&new));
            const SHOWN: usize = 3;
            let more = new.len().saturating_sub(SHOWN);
            let more = if more > 0 { format!(" and {more} more") } else { String::new() };
            return Err(OpenError::Other(format!(
                "migration {number} would leave rows whose foreign keys lead nowhere: {}{more}",
                list(&new[..new.len().min(SHOWN)])
            )));
        }
        tx.pragma_update(None, "user_version", number).map_err(|e| classify("schema version", e))?;
        tx.commit().map_err(|e| classify("migration commit", e))?;
    }
}

fn sql(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

/// Whether tabs are being saved, for the UI. Mirrored by `StoreStatus` in src/main.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum StoreStatus {
    Saving,
    /// Saving, but the old database was damaged and replaced.
    Recovered { notice: String },
    /// Running on an in-memory database: nothing survives a restart.
    NotSaving { reason: String },
}

/// What the database is stored in. Only a file can be reached by the hook.
enum Backing {
    File { path: PathBuf, recovered_notice: Option<String> },
    Memory { reason: String },
}

pub struct Db {
    backing: Backing,
    conn: Mutex<Connection>,
    opened: SystemTime,
    /// Messages already logged, for problems every refresh would log again.
    logged: Mutex<HashSet<String>>,
}

/// Unix seconds, the one time format misag stores and compares.
pub(crate) fn unix_secs(time: SystemTime) -> u64 {
    // Before 1970 only on a badly wrong clock; 0 then reads as "long ago".
    time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Every path that makes up the database: the file and its WAL sidecars, sidecars first.
fn database_files(path: &Path) -> [PathBuf; 3] {
    ["-wal", "-shm", ""].map(|suffix| PathBuf::from(format!("{}{suffix}", path.display())))
}

impl Db {
    pub fn open(path: PathBuf) -> Result<Self, OpenError> {
        Self::open_with_notice(path, None)
    }

    /// Opens an existing database the way the hook does (see `connect_existing`):
    /// never created or migrated, and only at this binary's schema. For other
    /// processes than the app, such as `misag flow`, which may be another misag
    /// version than the running app.
    pub fn open_existing(path: PathBuf) -> Result<Self, String> {
        let conn = connect_existing(&path)?;
        Ok(Self::with(Backing::File { path, recovered_notice: None }, conn))
    }

    fn open_with_notice(path: PathBuf, recovered_notice: Option<String>) -> Result<Self, OpenError> {
        // The path is handed to the hook in an environment variable.
        if path.to_str().is_none() {
            return Err(OpenError::Other(format!("the path {path:?} is not valid UTF-8")));
        }
        let conn = connect(&path)?;
        Ok(Self::with(Backing::File { path, recovered_notice }, conn))
    }

    fn with(backing: Backing, conn: Connection) -> Self {
        Self { backing, conn: Mutex::new(conn), opened: SystemTime::now(), logged: Mutex::default() }
    }

    fn in_memory(reason: String) -> Self {
        eprintln!("misag: {reason}; tabs will not be saved this run");
        let mut conn = Connection::open_in_memory().expect("in-memory SQLite always opens");
        enforce_foreign_keys(&conn).expect("in-memory SQLite has foreign keys");
        migrate(&mut conn).expect("migrations apply to an empty in-memory database");
        Self::with(Backing::Memory { reason }, conn)
    }

    /// Opens the database without ever failing. A damaged file is moved aside to
    /// `<FILE>.broken-<unix time>` (with its `-wal`/`-shm` files) and replaced. Any
    /// other problem (no data directory, permissions, a lock, a newer schema) leaves
    /// the files untouched and runs on an empty in-memory database instead. Either
    /// way `status()` says what happened.
    pub fn open_or_recover(path: Result<PathBuf, String>) -> Self {
        let path = match path {
            Ok(path) => path,
            Err(e) => return Self::in_memory(format!("there is no app data directory ({e})")),
        };
        let damage = match Self::open(path.clone()) {
            Ok(db) => return db,
            Err(OpenError::Corrupt(e)) => e,
            Err(other) => return Self::in_memory(other.to_string()),
        };
        eprintln!("misag: {damage}; moving the database aside and starting fresh");
        let aside = PathBuf::from(format!("{}.broken-{}", path.display(), unix_secs(SystemTime::now())));
        if let Err(e) = move_files(&database_files(&path), &database_files(&aside)) {
            return Self::in_memory(format!("the database is damaged and could not be moved aside ({e})"));
        }
        let notice = format!(
            "Saved tabs could not be read, so Mischief of Agents started fresh. The old database was kept at {}.",
            aside.display()
        );
        match Self::open_with_notice(path, Some(notice)) {
            Ok(db) => db,
            Err(e) => Self::in_memory(format!("{e}; the damaged database was kept at {}", aside.display())),
        }
    }

    /// Like `open_or_recover`, but on the first start, while there is no database
    /// at `path` yet, raterm's at `old` is copied there first, so saved tabs,
    /// settings and flows carry over the rename. raterm's own is left as it was.
    pub fn open_or_import(path: Result<PathBuf, String>, old: Option<PathBuf>) -> Self {
        let imported = match (&path, old) {
            (Ok(path), Some(old)) => import(path, &old),
            _ => None,
        };
        let mut db = Self::open_or_recover(path);
        if let (Some(notice), Backing::File { recovered_notice: slot @ None, .. }) = (imported, &mut db.backing) {
            *slot = Some(notice);
        }
        db
    }

    /// Logs `message` unless it already was this run.
    pub fn log_once(&self, message: String) {
        if self.logged.lock().unwrap_or_else(|e| e.into_inner()).insert(message.clone()) {
            eprintln!("misag: {message}");
        }
    }

    /// The database file for the hook to write to; None on the in-memory fallback.
    pub fn hook_path(&self) -> Option<&Path> {
        match &self.backing {
            Backing::File { path, .. } => Some(path),
            Backing::Memory { .. } => None,
        }
    }

    pub fn status(&self) -> StoreStatus {
        match &self.backing {
            Backing::File { recovered_notice: None, .. } => StoreStatus::Saving,
            Backing::File { recovered_notice: Some(notice), .. } => StoreStatus::Recovered { notice: notice.clone() },
            Backing::Memory { reason } => StoreStatus::NotSaving { reason: reason.clone() },
        }
    }

    /// When this database was opened, i.e. when this run started.
    pub fn opened(&self) -> SystemTime {
        self.opened
    }

    /// When the hook last reported a session (unix seconds), for `tab_id` or for any tab.
    pub fn latest_report_at(&self, tab_id: Option<&TabId>) -> Result<Option<u64>, String> {
        self.conn()
            .query_row(
                "SELECT MAX(updated_at) FROM claude_sessions WHERE ?1 IS NULL OR tab_id = ?1",
                [tab_id.map(TabId::as_str)],
                |r| r.get::<_, Option<i64>>(0),
            )
            .map(|at| at.map(|at| u64::try_from(at).unwrap_or(0)))
            .map_err(sql)
    }

    /// The connection; a transaction unwound by a (debug-build) panic was rolled
    /// back, so recovering the lock is safe.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        crate::session::lock(&self.conn)
    }

    /// The saved tabs, each Claude tab with the session its hook last reported.
    /// A tab this version cannot represent (a newer kind, a bad session id) is an
    /// error rather than being skipped, so the caller never saves over it.
    pub fn load_layout(&self) -> Result<Layout, String> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT t.tab_id, t.kind, t.name, t.renamed, t.cwd, t.command,
                        CASE WHEN t.kind = 'claude' THEN COALESCE(c.session_id, t.claude_session)
                             ELSE t.claude_session END
                 FROM tabs t LEFT JOIN claude_sessions c USING (tab_id)
                 ORDER BY t.position",
            )
            .map_err(sql)?;
        type Row = (String, String, String, bool, Option<String>, Option<String>, Option<String>);
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
            .map_err(sql)?
            .collect::<Result<Vec<Row>, _>>()
            .map_err(sql)?;
        let mut tabs = Vec::with_capacity(rows.len());
        for (tab_id, kind, name, renamed, cwd, command, session) in rows {
            let tab_id = TabId::parse(&tab_id)?;
            let claude_session = session.map(|s| SessionId::parse(&s)).transpose();
            // The names match the serde tags of `TabKind` and the schema's CHECK.
            let kind = match kind.as_str() {
                // Only a hint for reopening Claude in the shell, so a bad one is dropped
                // rather than failing the whole restore.
                "terminal" => TabKind::Terminal {
                    command,
                    claude_session: claude_session.unwrap_or_else(|e| {
                        eprintln!("misag: tab {tab_id}: {e}; not reopening its Claude session");
                        None
                    }),
                },
                "claude" => TabKind::Claude { claude_session: claude_session.map_err(|e| format!("tab {tab_id}: {e}"))? },
                _ => return Err(format!("tab {tab_id} has a kind this version does not know ({kind:?})")),
            };
            tabs.push(Tab { tab_id, kind, name, renamed, cwd });
        }
        let saved_active = get_state(&conn, "active_tab")?;
        let active = match saved_active.as_deref().map(str::parse::<usize>) {
            Some(Ok(n)) => n,
            Some(Err(_)) => {
                eprintln!("misag: ignoring unreadable active tab {saved_active:?}");
                0
            }
            None => 0,
        };
        Layout { active, tabs }.validated()
    }

    /// Stores the layout. Each Claude tab is stored with the session its hook last
    /// reported, so the `tabs` table holds the current session too; a terminal tab
    /// with the Claude session the caller found open in it.
    pub fn save_layout(&self, layout: Layout) -> Result<(), String> {
        let layout = layout.validated()?;
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(sql)?;
        tx.execute("DELETE FROM tabs", []).map_err(sql)?;
        let mut insert = tx
            .prepare_cached(
                "INSERT INTO tabs (position, tab_id, kind, name, renamed, cwd, command, claude_session)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                         CASE WHEN ?3 = 'claude'
                              THEN COALESCE((SELECT session_id FROM claude_sessions WHERE tab_id = ?2), ?8)
                              ELSE ?8
                         END)",
            )
            .map_err(sql)?;
        for (position, t) in layout.tabs.iter().enumerate() {
            let (kind, command, session) = match &t.kind {
                TabKind::Terminal { command, claude_session } => ("terminal", command.as_deref(), claude_session),
                TabKind::Claude { claude_session } => ("claude", None, claude_session),
            };
            let session = session.as_ref().map(SessionId::as_str);
            insert
                .execute(params![position as i64, t.tab_id.as_str(), kind, t.name, t.renamed, t.cwd, command, session])
                .map_err(sql)?;
        }
        drop(insert);
        set_state(&tx, "active_tab", &layout.active.to_string())?;
        tx.commit().map_err(sql)
    }

    /// The stored settings (the migration created their row with the defaults).
    pub fn load_settings(&self) -> Result<Settings, String> {
        load_settings(&self.conn())
    }

    /// Stores the settings (checked by the caller, see `Settings::validated`),
    /// returning them as stored.
    pub fn save_settings(&self, settings: Settings) -> Result<Settings, String> {
        let changed = self
            .conn()
            .execute(
                "UPDATE settings SET skip_permissions = ?1, workspace = ?2, disable_auto_memory = ?3, font_size = ?4, finish_sound = ?5 WHERE id = 1",
                params![settings.skip_permissions, settings.workspace, settings.disable_auto_memory, settings.font_size, settings.finish_sound],
            )
            .map_err(sql)?;
        if changed != 1 {
            return Err("the settings row is missing, so nothing was saved".into());
        }
        self.load_settings()
    }

    /// The transcript the hook last reported for `session`, if any. Two tabs can be
    /// in the same session (`/resume` of it in both); the newest report wins.
    pub fn transcript_for(&self, session: &SessionId) -> Result<Option<PathBuf>, String> {
        self.conn()
            .query_row(
                "SELECT transcript_path FROM claude_sessions
                 WHERE session_id = ?1 AND transcript_path IS NOT NULL
                 ORDER BY updated_at DESC LIMIT 1",
                [session.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map(|path| path.map(PathBuf::from))
            .map_err(sql)
    }

    /// The session the hook last reported for `tab_id`, so a restarted tab goes
    /// back to the session it was in, not the one it was first launched with.
    pub fn reported_session(&self, tab_id: &TabId) -> Result<Option<SessionId>, String> {
        let id: Option<String> = self
            .conn()
            .query_row("SELECT session_id FROM claude_sessions WHERE tab_id = ?1", [tab_id.as_str()], |r| r.get(0))
            .optional()
            .map_err(sql)?;
        id.map(|id| SessionId::parse(&id).map_err(|e| format!("tab {tab_id}: {e}"))).transpose()
    }

    /// The sessions pinned in the Session history box, in no particular order. A
    /// row this version cannot read is left out (and logged).
    pub fn pinned_sessions(&self) -> Result<Vec<SessionId>, String> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT session_id FROM pinned_sessions").map_err(sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(sql)?;
        let mut pinned = Vec::new();
        for row in rows {
            match SessionId::parse(&row.map_err(sql)?) {
                Ok(id) => pinned.push(id),
                Err(e) => self.log_once(format!("pinned session: {e}")),
            }
        }
        Ok(pinned)
    }

    /// Pins the session (pinning it again changes nothing) or unpins it.
    pub fn set_session_pinned(&self, id: &SessionId, pinned: bool) -> Result<(), String> {
        let conn = self.conn();
        if pinned {
            conn.execute(
                "INSERT OR IGNORE INTO pinned_sessions (session_id, pinned_at) VALUES (?1, ?2)",
                params![id.as_str(), now_secs()],
            )
        } else {
            conn.execute("DELETE FROM pinned_sessions WHERE session_id = ?1", [id.as_str()])
        }
        .map(drop)
        .map_err(sql)
    }

    /// The session the hook last reported for every tab, by tab id. A row this
    /// version cannot read is left out (and logged).
    pub fn reported_sessions(&self) -> Result<HashMap<String, SessionId>, String> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT tab_id, session_id FROM claude_sessions").map_err(sql)?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(sql)?;
        let mut sessions = HashMap::new();
        for row in rows {
            let (tab_id, id) = row.map_err(sql)?;
            match SessionId::parse(&id) {
                Ok(id) => {
                    sessions.insert(tab_id, id);
                }
                Err(e) => self.log_once(format!("tab {tab_id}: {e}")),
            }
        }
        Ok(sessions)
    }

    /// The session the hook last reported for `tab_id` if the Claude process `pid`
    /// reported it, so that process still being in the tab means the session is open.
    pub fn session_run_by(&self, tab_id: &TabId, pid: u32) -> Result<Option<SessionId>, String> {
        let id: Option<String> = self
            .conn()
            .query_row(
                "SELECT session_id FROM claude_sessions WHERE tab_id = ?1 AND claude_pid = ?2",
                params![tab_id.as_str(), pid],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql)?;
        id.map(|id| SessionId::parse(&id).map_err(|e| format!("tab {tab_id}: {e}"))).transpose()
    }

    /// Whether the hook has reported a session for `tab_id` since `since` (unix
    /// seconds, the tab's start); a report from an earlier run does not count.
    pub fn claude_reported(&self, tab_id: &TabId, since: u64) -> Result<bool, String> {
        let since = i64::try_from(since).map_err(|_| format!("bad start time {since}"))?;
        self.conn()
            .query_row(
                "SELECT 1 FROM claude_sessions WHERE tab_id = ?1 AND updated_at >= ?2",
                params![tab_id.as_str(), since],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(sql)
    }

    /// When (unix ms) each tab's Claude ended its last turn waiting on background
    /// tasks, by tab id; tabs waiting on none are left out.
    pub fn background_tabs(&self) -> Result<HashMap<String, i64>, String> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT tab_id, background_at FROM claude_sessions WHERE background_at IS NOT NULL")
            .map_err(sql)?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(sql)?.collect::<Result<_, _>>().map_err(sql)
    }

    /// Drops hook reports for tabs that no longer exist, and forgets the Claude
    /// processes of the last run and the background tasks they waited on: they are
    /// gone, and a new process could reuse a pid. Run at startup; the single-instance guard means no other misag's tabs
    /// can be reporting.
    pub fn prune_claude_sessions(&self) -> Result<(), String> {
        self.conn()
            .execute_batch(
                "DELETE FROM claude_sessions WHERE tab_id NOT IN (SELECT tab_id FROM tabs);
                 UPDATE claude_sessions SET claude_pid = NULL, background_at = NULL;",
            )
            .map_err(sql)
    }
}

/// A flow as the sidebar lists it. Mirrored by `Flow` in src/flows.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FlowSummary {
    pub id: i64,
    pub name: String,
    pub archived: bool,
    /// Whether a run begins by clearing the conversation (`flows::clears`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub clear: bool,
    /// Added by a program in a tab and not yet confirmed by the person, so it does
    /// not run (see `import_unconfirmed_flow`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unconfirmed: bool,
}

/// The longest flow name, in characters, as long as a step title.
const MAX_FLOW_NAME: usize = 200;

/// A flow's name as stored: trimmed, not empty, at most `MAX_FLOW_NAME` characters,
/// and plain text, as it is printed in terminals (`misag flow list`) and read by Claude.
fn flow_name(name: &str) -> Result<&str, String> {
    match name.trim() {
        "" => Err("a flow needs a name".into()),
        name if name.chars().count() > MAX_FLOW_NAME => Err(format!("a flow name can be at most {MAX_FLOW_NAME} characters")),
        name => match name.chars().find(|&c| c.is_control() || flows::hidden(c)) {
            Some(c) => Err(format!("a flow name cannot hold control or hidden characters (U+{:04X})", c as u32)),
            None => Ok(name),
        },
    }
}

fn graph_json(graph: &Graph) -> Result<String, String> {
    serde_json::to_string(graph).map_err(|e| format!("the steps could not be stored ({e})"))
}

fn now_secs() -> i64 {
    i64::try_from(unix_secs(SystemTime::now())).unwrap_or(i64::MAX)
}

/// Fails unless a statement changed exactly the flow `id`.
fn one_flow(id: i64, changed: usize) -> Result<(), String> {
    if changed == 1 { Ok(()) } else { Err(format!("there is no flow {id}")) }
}

impl Db {
    /// Every flow in order, archived ones included.
    pub fn list_flows(&self) -> Result<Vec<FlowSummary>, String> {
        let conn = self.conn();
        // Read in SQL, so listing does not parse every flow's steps; it must agree
        // with `flows::clears` (a test checks), and a graph that is not JSON does not clear.
        let mut stmt = conn
            .prepare(
                "SELECT id, name, archived, CASE WHEN json_valid(graph) THEN EXISTS (
                    SELECT 1 FROM json_each(graph, '$.nodes')
                    WHERE json_extract(value, '$.kind') = 'start' AND json_extract(value, '$.clear') = 1
                 ) ELSE 0 END, unconfirmed
                 FROM flows ORDER BY position, id",
            )
            .map_err(sql)?;
        stmt.query_map([], |r| {
            Ok(FlowSummary { id: r.get(0)?, name: r.get(1)?, archived: r.get(2)?, clear: r.get(3)?, unconfirmed: r.get(4)? })
        })
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)
    }

    /// Adds a flow after the others, with only its Start step.
    pub fn create_flow(&self, name: &str) -> Result<FlowSummary, String> {
        self.add_flow(name, &Graph::default(), false)
    }

    /// Adds a flow after the others with these steps, if they pass `flows::check`
    /// (see `flow_file` for where they come from).
    pub fn import_flow(&self, name: &str, graph: &Graph) -> Result<FlowSummary, String> {
        flows::check(graph)?;
        self.add_flow(name, &flows::stored(graph.clone()), false)
    }

    /// Like `import_flow`, for a flow a program in a tab added (Claude, say, which a
    /// file or page it read may have steered): it runs only once the person has
    /// confirmed it (`confirm_flow`), as ▶ would otherwise send its steps unseen.
    pub fn import_unconfirmed_flow(&self, name: &str, graph: &Graph) -> Result<FlowSummary, String> {
        flows::check(graph)?;
        self.add_flow(name, &flows::stored(graph.clone()), true)
    }

    /// The person has seen the flow and lets it run.
    pub fn confirm_flow(&self, id: i64) -> Result<(), String> {
        let changed = self.conn().execute("UPDATE flows SET unconfirmed = 0 WHERE id = ?1", [id]).map_err(sql)?;
        one_flow(id, changed)
    }

    fn add_flow(&self, name: &str, graph: &Graph, unconfirmed: bool) -> Result<FlowSummary, String> {
        let name = flow_name(name)?;
        let clear = flows::clears(graph);
        let graph = graph_json(graph)?;
        let conn = self.conn();
        let now = now_secs();
        conn.execute(
            "INSERT INTO flows (name, position, graph, created_at, updated_at, unconfirmed)
             VALUES (?1, (SELECT COALESCE(MAX(position), -1) + 1 FROM flows), ?3, ?2, ?2, ?4)",
            params![name, now, graph, unconfirmed],
        )
        .map_err(sql)?;
        Ok(FlowSummary { id: conn.last_insert_rowid(), name: name.into(), archived: false, clear, unconfirmed })
    }

    pub fn rename_flow(&self, id: i64, name: &str) -> Result<(), String> {
        let name = flow_name(name)?;
        let changed = self
            .conn()
            .execute("UPDATE flows SET name = ?2, updated_at = ?3 WHERE id = ?1", params![id, name, now_secs()])
            .map_err(sql)?;
        one_flow(id, changed)
    }

    /// Archived flows keep their place, so unarchiving puts them back where they were.
    pub fn set_flow_archived(&self, id: i64, archived: bool) -> Result<(), String> {
        let changed = self
            .conn()
            .execute("UPDATE flows SET archived = ?2, updated_at = ?3 WHERE id = ?1", params![id, archived, now_secs()])
            .map_err(sql)?;
        one_flow(id, changed)
    }

    /// Puts the flows in the order of `ids`, which must list every flow exactly once.
    pub fn reorder_flows(&self, ids: &[i64]) -> Result<(), String> {
        let mut conn = self.conn();
        // IMMEDIATE, so the hook cannot write between the check and the update.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        let mut stored: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM flows").map_err(sql)?;
            stmt.query_map([], |r| r.get(0)).map_err(sql)?.collect::<Result<_, _>>().map_err(sql)?
        };
        let mut given = ids.to_vec();
        stored.sort_unstable();
        given.sort_unstable();
        if given != stored {
            return Err("the new order must list every flow exactly once; the flows may have changed, so reload them".into());
        }
        for (position, id) in ids.iter().enumerate() {
            tx.execute("UPDATE flows SET position = ?2 WHERE id = ?1", params![id, position as i64]).map_err(sql)?;
        }
        tx.commit().map_err(sql)
    }

    /// The flow and its steps; one stored without any steps (the column's default)
    /// loads as a new flow.
    pub fn load_flow(&self, id: i64) -> Result<(FlowSummary, Graph), String> {
        let (name, archived, graph, unconfirmed): (String, bool, String, bool) = self
            .conn()
            .query_row("SELECT name, archived, graph, unconfirmed FROM flows WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()
            .map_err(sql)?
            .ok_or_else(|| format!("there is no flow {id}"))?;
        let graph: Graph = serde_json::from_str(&graph)
            .map_err(|e| format!("the steps of \"{name}\" cannot be read ({e})"))?;
        let graph = if graph.nodes.is_empty() && graph.edges.is_empty() { Graph::default() } else { graph };
        Ok((FlowSummary { id, name, archived, clear: flows::clears(&graph), unconfirmed }, graph))
    }

    /// Stores the flow's steps if they pass `flows::check`.
    pub fn save_flow_graph(&self, id: i64, graph: &Graph) -> Result<(), String> {
        flows::check(graph)?;
        let graph = graph_json(&flows::stored(graph.clone()))?;
        let changed = self
            .conn()
            .execute("UPDATE flows SET graph = ?2, updated_at = ?3 WHERE id = ?1", params![id, graph, now_secs()])
            .map_err(sql)?;
        one_flow(id, changed)
    }

    /// Deletes a flow and its finished runs; refused while a tab is running it.
    pub fn delete_flow(&self, id: i64) -> Result<(), String> {
        let changed = self
            .conn()
            .execute(
                &format!(
                    "DELETE FROM flows WHERE id = ?1 AND NOT EXISTS (
                         SELECT 1 FROM flow_runs WHERE flow_id = ?1 AND status IN {})",
                    runs::ACTIVE
                ),
                [id],
            )
            .map_err(sql)?;
        if changed == 0 && self.conn().query_row("SELECT 1 FROM flows WHERE id = ?1", [id], |_| Ok(())).optional().map_err(sql)?.is_some() {
            return Err("the flow is running in a tab; stop that run first".into());
        }
        one_flow(id, changed)
    }

    /// At startup: ends the runs of tabs that were not saved, and makes the running
    /// ones wait (see `runs::after_restart`).
    pub fn resume_runs(&self) -> Result<(), String> {
        let mut conn = self.conn();
        let tabs: Vec<TabId> = {
            let mut stmt = conn.prepare("SELECT tab_id FROM tabs").map_err(sql)?;
            let ids: Vec<String> = stmt.query_map([], |r| r.get(0)).map_err(sql)?.collect::<Result<_, _>>().map_err(sql)?;
            ids.iter().filter_map(|id| TabId::parse(id).ok()).collect()
        };
        runs::after_restart(&mut conn, &tabs)
    }

    pub fn stop_tab_run(&self, tab_id: &TabId) -> Result<(), String> {
        runs::stop_tab(&self.conn(), tab_id)
    }

    pub fn start_run(&self, flow_id: i64, tab_id: &TabId) -> Result<runs::Started, String> {
        runs::start(&mut self.conn(), flow_id, tab_id)
    }

    pub fn cleared_run(&self, run_id: i64) -> Result<runs::Cleared, String> {
        runs::cleared(&mut self.conn(), run_id)
    }

    pub fn prefill_text(&self, run_id: i64, step_id: &str) -> Result<Option<runs::Prefill>, String> {
        runs::prefill_text(&mut self.conn(), run_id, step_id)
    }

    pub fn prefilled(&self, run_id: i64, step_id: &str, token: i64) -> Result<(), String> {
        runs::prefilled(&mut self.conn(), run_id, step_id, token)
    }

    pub fn proceed_run(&self, run_id: i64, step_id: &str, output: &str) -> Result<Option<runs::StepPrompt>, String> {
        runs::proceed(&mut self.conn(), run_id, step_id, output)
    }

    pub fn resume_run(&self, run_id: i64, step_id: &str) -> Result<runs::StepPrompt, String> {
        runs::resume(&mut self.conn(), run_id, step_id)
    }

    pub fn unsent_run(&self, run_id: i64, step_id: &str, briefs: bool, why: &str) -> Result<bool, String> {
        runs::unsent(&mut self.conn(), run_id, step_id, briefs, why)
    }

    pub fn stop_run(&self, run_id: i64) -> Result<(), String> {
        runs::stop(&self.conn(), run_id)
    }

    /// Makes the runs that have timed out at Start wait for the person (see `runs::expire`).
    pub fn expire_runs(&self) -> Result<(), String> {
        runs::expire(&mut self.conn())
    }

    pub fn active_runs(&self) -> Result<Vec<runs::Run>, String> {
        runs::active(&self.conn())
    }
}

/// Copies the database at `old` to `path` if there is no database there yet, and
/// returns the notice that says whether it worked. The copy is a snapshot taken
/// through SQLite, so it is consistent even while raterm still runs and writes;
/// conversations still open in a running raterm are left out of the copy (see
/// `release_open_sessions`). A failed copy leaves nothing at `path`, so the app
/// starts fresh and says why.
fn import(path: &Path, old: &Path) -> Option<String> {
    match (path.try_exists(), old.try_exists()) {
        (Ok(false), Ok(true)) => {}
        (Ok(_), Ok(_)) => return None,
        (Err(e), _) => {
            eprintln!("misag: could not check whether {} exists: {e}", path.display());
            return None;
        }
        (_, Err(e)) => {
            eprintln!("misag: could not check whether {} exists: {e}", old.display());
            return None;
        }
    }
    // Written under another name first, so a copy cut short is never opened.
    let copying = PathBuf::from(format!("{}.importing", path.display()));
    let copied = (|| -> Result<usize, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // Left by a copy cut short on an earlier start.
        remove_files(&database_files(&copying));
        let from = Connection::open_with_flags(old, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
        let into = copying.to_str().ok_or("the path is not UTF-8")?;
        from.execute("VACUUM INTO ?1", [into]).map_err(|e| e.to_string())?;
        drop(from);
        let released = release_open_sessions(&Connection::open(&copying).map_err(|e| e.to_string())?)?;
        move_files(&database_files(&copying), &database_files(path)).map_err(|e| e.to_string())?;
        Ok(released)
    })();
    let released = match copied {
        Ok(released) => released,
        Err(e) => {
            eprintln!("misag: could not copy raterm's database from {}: {e}; starting fresh", old.display());
            remove_files(&database_files(&copying));
            return Some(format!(
                "Saved tabs, settings and flows could not be copied from raterm, the app's old name ({e}). \
                 raterm's database is still at {}. To try again, quit Mischief of Agents, delete {} and start it again.",
                old.display(),
                path.display()
            ));
        }
    };
    eprintln!("misag: copied raterm's database from {} to {}", old.display(), path.display());
    let open = match released {
        0 => String::new(),
        1 => " raterm is still running, so the 1 Claude conversation open in it was not reopened here; \
              once you quit raterm, find it in Session history."
            .to_owned(),
        n => format!(
            " raterm is still running, so the {n} Claude conversations open in it were not reopened here; \
             once you quit raterm, find them in Session history."
        ),
    };
    Some(format!(
        "Saved tabs, settings and flows were copied from raterm, the app's old name. raterm's own database was left at {}.{open}",
        old.display()
    ))
}

/// In a fresh copy of raterm's database: forgets the Claude session of each tab
/// whose Claude still runs (raterm is running, and has it open), so its tab starts
/// a new conversation rather than resuming one open in two places, and stops that
/// tab's flow run, which raterm still drives. Returns how many were forgotten.
fn release_open_sessions(conn: &Connection) -> Result<usize, String> {
    let has_pids = conn
        .prepare("SELECT tab_id, claude_pid FROM claude_sessions WHERE claude_pid IS NOT NULL")
        .and_then(|mut stmt| stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<Result<Vec<_>, _>>());
    let tabs = match has_pids {
        Ok(rows) => rows.into_iter().filter(|&(_, pid)| alive(pid)).map(|(tab, _)| tab).collect::<Vec<_>>(),
        // A raterm too old to record pids: nothing to tell by.
        Err(e) => {
            eprintln!("misag: could not tell which of raterm's conversations are still open: {e}");
            return Ok(0);
        }
    };
    let has_runs: bool = conn
        .query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'flow_runs')", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    for tab in &tabs {
        conn.execute("UPDATE tabs SET claude_session = NULL WHERE tab_id = ?1", [tab]).map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM claude_sessions WHERE tab_id = ?1", [tab]).map_err(|e| e.to_string())?;
        if has_runs {
            // One UPDATE of the status, not a `runs::RunState` save: the copy of raterm's
            // database may not have the columns `RunState` saves.
            conn.execute(&format!("UPDATE flow_runs SET status = 'stopped' WHERE tab_id = ?1 AND status IN {}", runs::ACTIVE), [tab])
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(tabs.len())
}

/// Whether process `pid` exists (one owned by another user counts).
#[cfg(windows)]
fn alive(pid: i64) -> bool {
    u32::try_from(pid).is_ok_and(|pid| pid > 0 && crate::win::alive(pid))
}

/// Whether process `pid` exists (one owned by another user counts).
#[cfg(unix)]
fn alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists; nothing is sent.
    (unsafe { libc::kill(pid, 0) } == 0) || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Removes those of `files` that exist, logging what could not be removed.
fn remove_files(files: &[PathBuf]) {
    for file in files {
        match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                eprintln!("misag: could not remove {}: {e}", file.display());
            }
            _ => {}
        }
    }
}

/// Renames each existing `from` file to the matching `to`, putting back whatever
/// was already moved if one fails. Sidecars come first, so a failure does not leave
/// a stale WAL next to a fresh database (unless putting it back fails too, which
/// is logged).
fn move_files(from: &[PathBuf], to: &[PathBuf]) -> std::io::Result<()> {
    let mut moved: Vec<(&PathBuf, &PathBuf)> = Vec::new();
    for (src, dst) in from.iter().zip(to) {
        let result = match src.try_exists() {
            Ok(false) => continue,
            Ok(true) => std::fs::rename(src, dst),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            for (src, dst) in moved.into_iter().rev() {
                if let Err(back) = std::fs::rename(dst, src) {
                    eprintln!("misag: could not move {} back: {back}", dst.display());
                }
            }
            return Err(e);
        }
        moved.push((src, dst));
    }
    Ok(())
}

fn get_state(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    conn.query_row("SELECT value FROM app_state WHERE key = ?1", [key], |r| r.get(0))
        .optional()
        .map_err(sql)
}

fn set_state(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO app_state (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [key, value],
    )
    .map(drop)
    .map_err(sql)
}

/// What the SessionStart hook learned about the session now running in a tab.
pub struct SessionReport<'a> {
    pub tab_id: &'a TabId,
    pub session_id: &'a SessionId,
    pub source: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub transcript_path: Option<&'a str>,
    /// The Claude process (`CLAUDE_PID` in the hook's environment).
    pub claude_pid: Option<u32>,
}

/// The settings stored in `conn`; see `Db::load_settings`.
pub fn load_settings(conn: &Connection) -> Result<Settings, String> {
    conn.query_row("SELECT skip_permissions, workspace, disable_auto_memory, font_size, finish_sound FROM settings WHERE id = 1", [], |r| {
        // Only a sound: one this version cannot read is Off, rather than losing the other settings.
        let finish_sound = r.get(4).unwrap_or_else(|e| {
            eprintln!("misag: the stored finish sound cannot be read ({e}); using none");
            FinishSound::Off
        });
        Ok(Settings { skip_permissions: r.get(0)?, workspace: r.get(1)?, disable_auto_memory: r.get(2)?, font_size: r.get(3)?, finish_sound })
    })
    .map_err(sql)
}

/// Records the session a tab is now in, replacing any earlier report for it; a new
/// session waits on no background tasks yet. The tab may not be saved yet (the hook can run before the first save), so this does
/// not require a `tabs` row; startup pruning removes reports for tabs that are gone.
pub fn record_claude_session(conn: &Connection, report: &SessionReport) -> Result<(), String> {
    let now = i64::try_from(unix_secs(SystemTime::now())).unwrap_or(i64::MAX);
    conn.execute(
        "INSERT INTO claude_sessions (tab_id, session_id, source, cwd, transcript_path, updated_at, claude_pid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (tab_id) DO UPDATE SET
            session_id = excluded.session_id, source = excluded.source, cwd = excluded.cwd,
            transcript_path = excluded.transcript_path, updated_at = excluded.updated_at,
            claude_pid = excluded.claude_pid, background_at = NULL",
        params![
            report.tab_id.as_str(),
            report.session_id.as_str(),
            report.source,
            report.cwd,
            report.transcript_path,
            now,
            report.claude_pid
        ],
    )
    .map(drop)
    .map_err(sql)
}

/// Records whether the session `session_id` in `tab_id` ended its turn with
/// background tasks in flight, and when, so the app can tell a later turn that
/// ended unreported (an interrupt) from this one; a session the tab is no longer
/// in is left alone.
pub fn record_background(conn: &Connection, tab_id: &TabId, session_id: &str, background: bool) -> Result<(), String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    // Clearing what is already clear writes nothing, as most turns end with no tasks.
    conn.execute(
        "UPDATE claude_sessions SET background_at = ?3
         WHERE tab_id = ?1 AND session_id = ?2 AND (?3 IS NOT NULL OR background_at IS NOT NULL)",
        params![tab_id.as_str(), session_id, background.then_some(now)],
    )
    .map(drop)
    .map_err(sql)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::flows::Graph;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn sid(s: &str) -> SessionId {
        SessionId::parse(s).unwrap()
    }

    pub(crate) fn tid(s: &str) -> TabId {
        TabId::parse(s).unwrap()
    }
    const S1: &str = "1b4e28ba-2fa1-41d2-883f-0016d3cca427";
    const S2: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const S3: &str = "6fa459ea-ee8a-4ca4-894e-db77e160355e";

    /// Where a test's database goes, in a fresh directory.
    pub(crate) fn temp_db(name: &str) -> PathBuf {
        crate::test_support::temp_dir(name).join(FILE)
    }

    fn claude_tab(id: &str, session: Option<&str>) -> Tab {
        Tab {
            tab_id: tid(id),
            kind: TabKind::Claude { claude_session: session.map(sid) },
            name: format!("tab {id}"),
            renamed: false,
            cwd: Some("/tmp".into()),
        }
    }

    fn terminal_tab(id: &str) -> Tab {
        Tab {
            tab_id: tid(id),
            kind: TabKind::Terminal { command: Some("htop".into()), claude_session: None },
            name: format!("tab {id}"),
            renamed: true,
            cwd: None,
        }
    }

    /// A report with only a tab and a session (source `clear`).
    pub(crate) fn report<'a>(tab_id: &'a TabId, session_id: &'a SessionId) -> SessionReport<'a> {
        SessionReport { tab_id, session_id, source: Some("clear"), cwd: None, transcript_path: None, claude_pid: None }
    }

    #[test]
    fn a_raterm_database_is_copied_when_there_is_none_yet() {
        let old = crate::test_support::temp_dir("import-old").join(RATERM_FILE);
        let raterm = Db::open(old.clone()).unwrap();
        raterm.save_layout(Layout { active: 0, tabs: vec![claude_tab("a", None)] }).unwrap();
        // Left open, so the tab may still be only in the WAL.
        let path = crate::test_support::temp_dir("import-new").join("sub").join(FILE);
        let db = Db::open_or_import(Ok(path.clone()), Some(old.clone()));
        assert_eq!(db.load_layout().unwrap().tabs.len(), 1);
        assert_eq!(db.hook_path(), Some(path.as_path()));
        let StoreStatus::Recovered { notice } = db.status() else { panic!("expected a notice") };
        assert!(notice.contains(&old.display().to_string()));
        // The old database is left as it was, for raterm itself.
        assert_eq!(raterm.load_layout().unwrap().tabs.len(), 1);
        assert!(files_named(path.parent().unwrap(), &format!("{FILE}.")).is_empty());
    }

    #[test]
    fn conversations_still_open_in_a_running_raterm_are_not_reopened() {
        const S2: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
        let old = crate::test_support::temp_dir("import-live-old").join(RATERM_FILE);
        let raterm = Db::open(old.clone()).unwrap();
        raterm.save_layout(Layout { active: 0, tabs: vec![claude_tab("a", Some(S1)), claude_tab("b", Some(S2))] }).unwrap();
        // Tab a's Claude still runs (this process stands in for it); tab b's has quit.
        let mut ended = std::process::Command::new("true").spawn().unwrap();
        let dead = ended.id();
        ended.wait().unwrap();
        let conn = connect_existing(&old).unwrap();
        for (tab, session, pid) in [("a", S1, std::process::id()), ("b", S2, dead)] {
            let (tab_id, session_id) = (tid(tab), sid(session));
            let report = SessionReport { claude_pid: Some(pid), ..report(&tab_id, &session_id) };
            record_claude_session(&conn, &report).unwrap();
        }
        let path = crate::test_support::temp_dir("import-live-new").join(FILE);
        let db = Db::open_or_import(Ok(path), Some(old));
        let sessions: Vec<_> = db
            .load_layout()
            .unwrap()
            .tabs
            .into_iter()
            .map(|t| match t.kind {
                TabKind::Claude { claude_session } => claude_session,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(sessions, [None, Some(sid(S2))]);
        let StoreStatus::Recovered { notice } = db.status() else { panic!("expected a notice") };
        assert!(notice.contains("1 Claude conversation"), "{notice}");
        // raterm's own database still has it.
        assert_eq!(raterm.load_layout().unwrap().tabs[0].kind, TabKind::Claude { claude_session: Some(sid(S1)) });
    }

    #[test]
    fn a_raterm_database_never_replaces_an_existing_one() {
        let old = crate::test_support::temp_dir("import-kept-old").join(RATERM_FILE);
        Db::open(old.clone()).unwrap().save_layout(Layout { active: 0, tabs: vec![claude_tab("a", None)] }).unwrap();
        let path = temp_db("import-kept-new");
        drop(Db::open(path.clone()).unwrap());
        let db = Db::open_or_import(Ok(path), Some(old));
        assert_eq!(db.load_layout().unwrap().tabs.len(), 0);
        assert_eq!(db.status(), StoreStatus::Saving);
    }

    #[test]
    fn a_raterm_database_that_cannot_be_copied_says_so() {
        let old = crate::test_support::temp_dir("import-bad-old").join(RATERM_FILE);
        fs::write(&old, b"not a database, but long enough to be read as a page header").unwrap();
        let path = crate::test_support::temp_dir("import-bad-new").join(FILE);
        let db = Db::open_or_import(Ok(path.clone()), Some(old.clone()));
        assert_eq!(db.load_layout().unwrap().tabs.len(), 0);
        assert_eq!(db.hook_path(), Some(path.as_path()));
        let StoreStatus::Recovered { notice } = db.status() else { panic!("expected a notice") };
        assert!(notice.contains("could not be copied"), "{notice}");
        assert!(notice.contains(&old.display().to_string()), "{notice}");
        assert!(files_named(path.parent().unwrap(), &format!("{FILE}.importing")).is_empty());
    }

    #[test]
    fn without_a_raterm_database_a_fresh_one_starts_quietly() {
        let missing = crate::test_support::temp_dir("import-none").join(RATERM_FILE);
        let db = Db::open_or_import(Ok(temp_db("import-none-new")), Some(missing));
        assert_eq!(db.status(), StoreStatus::Saving);
    }

    fn files_named(dir: &Path, prefix: &str) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(prefix))
            .collect();
        names.sort();
        names
    }

    fn set_version(path: &Path, version: i64) {
        Connection::open(path).unwrap().pragma_update(None, "user_version", version).unwrap();
    }

    #[test]
    fn creates_the_schema_once() {
        let path = temp_db("schema");
        drop(Db::open(path.clone()).unwrap());
        let conn = connect(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len() as i64);
        let mode: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn upgrading_a_first_version_database_keeps_its_data_and_adds_the_settings() {
        let path = temp_db("upgrade-v1");
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute(
            "INSERT INTO tabs (position, tab_id, kind, name, renamed, claude_session) VALUES (0, 'a', 'claude', 'tab a', 0, ?1)",
            [S1],
        )
        .unwrap();
        tx.commit().unwrap();
        drop(conn);

        let db = Db::open(path.clone()).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs, vec![Tab { cwd: None, ..claude_tab("a", Some(S1)) }]);
        assert_eq!(db.load_settings().unwrap(), Settings::default());
        drop(db);
        // Opening again does not re-run migrations (a second settings row, or a duplicate column, would fail).
        let db = Db::open(path).unwrap();
        assert!(db.save_settings(Settings { skip_permissions: true, ..Settings::default() }).unwrap().skip_permissions);
    }

    #[test]
    fn upgrading_from_schema_2_keeps_the_settings_and_adds_the_workspace() {
        let path = temp_db("upgrade-v2");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&MIGRATIONS[..2].concat()).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute("UPDATE settings SET skip_permissions = 1", []).unwrap();
        drop(conn);
        let db = Db::open(path).unwrap();
        assert_eq!(db.load_settings().unwrap(), Settings { skip_permissions: true, ..Settings::default() });
    }

    #[test]
    fn upgrading_from_schema_4_keeps_the_settings_and_adds_auto_memory() {
        let path = temp_db("upgrade-v4");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&MIGRATIONS[..4].concat()).unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
        conn.execute("UPDATE settings SET skip_permissions = 1, workspace = '~/src'", []).unwrap();
        drop(conn);
        let db = Db::open(path).unwrap();
        let upgraded = Settings { skip_permissions: true, workspace: Some("~/src".into()), disable_auto_memory: false, ..Settings::default() };
        assert_eq!(db.load_settings().unwrap(), upgraded);
        let saved = db.save_settings(Settings { disable_auto_memory: true, ..upgraded }).unwrap();
        assert!(saved.disable_auto_memory);
    }

    #[test]
    fn saving_without_the_settings_row_is_an_error() {
        let path = temp_db("settings-no-row");
        let db = Db::open(path.clone()).unwrap();
        connect_existing(&path).unwrap().execute("DELETE FROM settings", []).unwrap();
        let err = db.save_settings(Settings { skip_permissions: true, ..Settings::default() }).unwrap_err();
        assert!(err.contains("settings row is missing"), "{err}");
    }

    #[test]
    fn layout_round_trips() {
        let db = Db::open(temp_db("roundtrip")).unwrap();
        assert_eq!(db.load_layout().unwrap(), Layout::default());
        let layout = Layout { active: 1, tabs: vec![claude_tab("a", Some(S1)), terminal_tab("b")] };
        db.save_layout(layout.clone()).unwrap();
        assert_eq!(db.load_layout().unwrap(), layout);

        let smaller = Layout { active: 0, tabs: vec![terminal_tab("b")] };
        db.save_layout(smaller.clone()).unwrap();
        assert_eq!(db.load_layout().unwrap(), smaller);
    }

    #[test]
    fn settings_round_trip_and_start_as_the_defaults() {
        let path = temp_db("settings");
        let db = Db::open(path.clone()).unwrap();
        assert_eq!(db.load_settings().unwrap(), Settings::default());
        let all = Settings { skip_permissions: true, workspace: Some("~".into()), disable_auto_memory: true, font_size: 18, finish_sound: FinishSound::Bell };
        let saved = db.save_settings(all.clone()).unwrap();
        assert_eq!(saved, all, "returned as stored");
        drop(db);
        assert_eq!(Db::open(path).unwrap().load_settings().unwrap(), saved);
    }

    #[test]
    fn upgrading_adds_no_pinned_sessions() {
        let path = temp_db("upgrade-pins");
        let conn = Connection::open(&path).unwrap();
        // The schema just before the migration that adds the pins.
        let before = MIGRATIONS.iter().position(|m| m.contains("CREATE TABLE pinned_sessions")).unwrap();
        conn.execute_batch(&MIGRATIONS[..before].concat()).unwrap();
        conn.pragma_update(None, "user_version", before as i64).unwrap();
        drop(conn);
        let db = Db::open(path).unwrap();
        assert_eq!(db.pinned_sessions().unwrap(), []);
        let a = SessionId::parse("0f8fad5b-d9cb-469f-a165-70867728950e").unwrap();
        db.set_session_pinned(&a, true).unwrap();
        assert_eq!(db.pinned_sessions().unwrap(), [a]);
    }

    #[test]
    fn pinning_a_session_is_stored_until_it_is_unpinned() {
        let db = Db::open(temp_db("pinned")).unwrap();
        let a = SessionId::parse("0f8fad5b-d9cb-469f-a165-70867728950e").unwrap();
        let b = SessionId::parse("7c9e6679-7425-40de-944b-e07fc1f90ae7").unwrap();
        assert_eq!(db.pinned_sessions().unwrap(), []);
        db.set_session_pinned(&a, true).unwrap();
        db.set_session_pinned(&b, true).unwrap();
        // Pinning again changes nothing.
        db.set_session_pinned(&a, true).unwrap();
        assert_eq!(db.pinned_sessions().unwrap().len(), 2);
        db.set_session_pinned(&a, false).unwrap();
        assert_eq!(db.pinned_sessions().unwrap(), [b]);
    }

    #[test]
    fn the_schema_refuses_settings_values_it_does_not_know() {
        let path = temp_db("settings-check");
        let db = Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        assert!(conn.execute("UPDATE settings SET skip_permissions = 2", []).is_err());
        assert!(conn.execute("UPDATE settings SET disable_auto_memory = 2", []).is_err());
        assert!(conn.execute("UPDATE settings SET font_size = 7", []).is_err());
        assert!(conn.execute("UPDATE settings SET font_size = 33", []).is_err());
        assert!(conn.execute("UPDATE settings SET finish_sound = 'siren'", []).is_err());
        assert!(conn.execute("INSERT INTO settings (id) VALUES (2)", []).is_err(), "one row only");
        assert_eq!(db.load_settings().unwrap(), Settings::default());
    }

    #[test]
    fn refuses_layouts_that_would_break_restores() {
        let db = Db::open(temp_db("invalid")).unwrap();
        let dupes = Layout { active: 0, tabs: vec![terminal_tab("a"), claude_tab("a", None)] };
        assert!(db.save_layout(dupes).is_err());
        assert_eq!(db.load_layout().unwrap(), Layout::default(), "a failed save changes nothing");
    }

    #[test]
    fn the_hook_report_wins_and_is_stored_with_the_tab() {
        let path = temp_db("hook");
        let db = Db::open(path.clone()).unwrap();
        db.save_layout(Layout { active: 0, tabs: vec![claude_tab("a", Some(S1))] }).unwrap();
        // The hook runs as a separate process with its own connection.
        record_claude_session(&connect_existing(&path).unwrap(), &report(&tid("a"), &sid(S2))).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs[0], claude_tab("a", Some(S2)));

        // The frontend still sends the launch-time id; the stored tab keeps the reported one.
        db.save_layout(Layout { active: 0, tabs: vec![claude_tab("a", Some(S1))] }).unwrap();
        let stored: String = connect(&path)
            .unwrap()
            .query_row("SELECT claude_session FROM tabs WHERE tab_id = 'a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, S2);

        record_claude_session(&connect_existing(&path).unwrap(), &report(&tid("a"), &sid(S3))).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs[0], claude_tab("a", Some(S3)));
    }

    #[test]
    fn a_terminal_tab_keeps_the_session_saved_with_it_not_the_latest_report() {
        let path = temp_db("terminal-session");
        let db = Db::open(path.clone()).unwrap();
        let open = |session: Option<&str>| Tab {
            kind: TabKind::Terminal { command: None, claude_session: session.map(sid) },
            ..terminal_tab("t")
        };
        db.save_layout(Layout { active: 0, tabs: vec![open(Some(S1))] }).unwrap();
        // A later report (Claude ran again, then quit) does not override what the save found open.
        record_claude_session(&connect_existing(&path).unwrap(), &report(&tid("t"), &sid(S2))).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs, vec![open(Some(S1))]);
        db.save_layout(Layout { active: 0, tabs: vec![open(None)] }).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs, vec![open(None)]);
    }

    #[test]
    fn knows_the_session_a_claude_process_reported() {
        let path = temp_db("run-by");
        let db = Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        let (t, s1) = (tid("t"), sid(S1));
        assert_eq!(db.session_run_by(&t, 42).unwrap(), None);
        record_claude_session(&conn, &SessionReport { claude_pid: Some(42), ..report(&t, &s1) }).unwrap();
        assert_eq!(db.session_run_by(&t, 42).unwrap(), Some(s1.clone()));
        assert_eq!(db.session_run_by(&t, 43).unwrap(), None, "another process");
        assert_eq!(db.session_run_by(&tid("u"), 42).unwrap(), None, "another tab");
        // Claude quit and ran again in the same tab: only the new process matches.
        let s2 = sid(S2);
        record_claude_session(&conn, &SessionReport { claude_pid: Some(43), ..report(&t, &s2) }).unwrap();
        assert_eq!(db.session_run_by(&t, 42).unwrap(), None);
        assert_eq!(db.session_run_by(&t, 43).unwrap(), Some(s2));
        // A report without a pid (an older hook) never matches.
        record_claude_session(&conn, &report(&t, &s1)).unwrap();
        assert_eq!(db.session_run_by(&t, 42).unwrap(), None);
    }

    #[test]
    fn knows_which_tabs_have_reported_since_they_started() {
        let path = temp_db("reported");
        let db = Db::open(path.clone()).unwrap();
        let now = unix_secs(SystemTime::now());
        assert!(!db.claude_reported(&tid("a"), now).unwrap());
        record_claude_session(&connect_existing(&path).unwrap(), &report(&tid("a"), &sid(S1))).unwrap();
        assert!(db.claude_reported(&tid("a"), now).unwrap());
        assert!(!db.claude_reported(&tid("a"), now + 3600).unwrap(), "a report from before the tab started does not count");
    }

    #[test]
    fn finds_reported_transcripts() {
        let path = temp_db("transcript");
        let db = Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        let s2 = sid(S2);
        let a = tid("a");
        let with_path = SessionReport { transcript_path: Some("/x/s2.jsonl"), ..report(&a, &s2) };
        record_claude_session(&conn, &with_path).unwrap();
        assert_eq!(db.transcript_for(&s2).unwrap(), Some(PathBuf::from("/x/s2.jsonl")));
        assert_eq!(db.transcript_for(&sid(S1)).unwrap(), None);
    }

    #[test]
    fn a_tab_it_cannot_represent_makes_loading_fail_instead_of_dropping_it() {
        let path = temp_db("unknown-kind");
        let db = Db::open(path.clone()).unwrap();
        db.save_layout(Layout { active: 0, tabs: vec![terminal_tab("a"), terminal_tab("b")] }).unwrap();
        let conn = connect(&path).unwrap();
        // Simulates a newer misag's kind; CHECK must be bypassed to write it.
        conn.execute_batch("PRAGMA ignore_check_constraints = ON; UPDATE tabs SET kind = 'notes' WHERE tab_id = 'b';")
            .unwrap();
        assert!(db.load_layout().unwrap_err().contains("notes"));

        conn.execute_batch("UPDATE tabs SET kind = 'claude', claude_session = 'bad' WHERE tab_id = 'b';").unwrap();
        assert!(db.load_layout().unwrap_err().contains("tab b"));
    }

    #[test]
    fn a_bad_terminal_session_is_dropped_not_fatal() {
        let path = temp_db("bad-terminal-session");
        let db = Db::open(path.clone()).unwrap();
        db.save_layout(Layout { active: 0, tabs: vec![terminal_tab("a")] }).unwrap();
        connect(&path).unwrap().execute("UPDATE tabs SET claude_session = 'bad'", []).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs, vec![terminal_tab("a")]);
    }

    #[test]
    fn prunes_reports_for_closed_tabs() {
        let path = temp_db("prune");
        let db = Db::open(path.clone()).unwrap();
        db.save_layout(Layout { active: 0, tabs: vec![claude_tab("a", None)] }).unwrap();
        let conn = connect_existing(&path).unwrap();
        record_claude_session(&conn, &report(&tid("a"), &sid(S1))).unwrap();
        record_claude_session(&conn, &report(&tid("gone"), &sid(S2))).unwrap();
        db.prune_claude_sessions().unwrap();
        let ids: Vec<String> = conn
            .prepare("SELECT tab_id FROM claude_sessions")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ids, ["a"]);
    }

    #[test]
    fn pruning_forgets_the_claude_processes_of_the_last_run() {
        let path = temp_db("prune-pids");
        let db = Db::open(path.clone()).unwrap();
        db.save_layout(Layout { active: 0, tabs: vec![terminal_tab("t")] }).unwrap();
        let conn = connect_existing(&path).unwrap();
        record_claude_session(&conn, &SessionReport { claude_pid: Some(42), ..report(&tid("t"), &sid(S1)) }).unwrap();
        record_background(&conn, &tid("t"), S1, true).unwrap();
        db.prune_claude_sessions().unwrap();
        assert!(db.background_tabs().unwrap().is_empty(), "its background tasks went with it");
        // A new process reusing pid 42 must not look like the old Claude.
        assert_eq!(db.session_run_by(&tid("t"), 42).unwrap(), None);
        assert_eq!(db.reported_session(&tid("t")).unwrap(), Some(sid(S1)), "the report itself is kept");
    }

    #[test]
    fn background_tasks_are_recorded_only_for_the_tabs_current_session() {
        let path = temp_db("background-session");
        let db = Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        record_claude_session(&conn, &report(&tid("t"), &sid(S1))).unwrap();
        // A late Stop from a session the tab has left.
        record_background(&conn, &tid("t"), S2, true).unwrap();
        assert!(db.background_tabs().unwrap().is_empty());
        record_background(&conn, &tid("t"), S1, true).unwrap();
        assert_eq!(db.background_tabs().unwrap().into_keys().collect::<Vec<_>>(), ["t"]);
        record_background(&conn, &tid("t"), S1, false).unwrap();
        assert!(db.background_tabs().unwrap().is_empty());
    }

    #[test]
    fn upgrading_leaves_reported_sessions_waiting_on_nothing() {
        let path = temp_db("upgrade-background");
        let conn = Connection::open(&path).unwrap();
        let before = MIGRATIONS.iter().position(|m| m.contains("ADD COLUMN background_at")).unwrap();
        conn.execute_batch(&MIGRATIONS[..before].concat()).unwrap();
        conn.pragma_update(None, "user_version", before as i64).unwrap();
        conn.execute(
            "INSERT INTO claude_sessions (tab_id, session_id, updated_at) VALUES ('t', ?1, 0)",
            [S1],
        )
        .unwrap();
        drop(conn);
        let db = Db::open(path.clone()).unwrap();
        assert!(db.background_tabs().unwrap().is_empty());
        assert_eq!(db.reported_session(&tid("t")).unwrap(), Some(sid(S1)));
    }

    #[test]
    fn moves_a_corrupt_database_and_its_sidecars_aside() {
        let path = temp_db("corrupt");
        let dir = path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "this is not a database, just text that is long enough to have a header").unwrap();
        fs::write(dir.join(format!("{FILE}-wal")), "stale wal").unwrap();
        fs::write(dir.join(format!("{FILE}-shm")), "stale shm").unwrap();
        let db = Db::open_or_recover(Ok(path.clone()));
        db.save_layout(Layout { active: 0, tabs: vec![claude_tab("a", None)] }).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs.len(), 1);

        let aside = files_named(&dir, &format!("{FILE}.broken-"));
        let main = aside.iter().find(|n| !n.ends_with("-wal") && !n.ends_with("-shm")).unwrap();
        assert!(fs::read_to_string(dir.join(main)).unwrap().starts_with("this is not a database"));
        // SQLite may already have discarded the invalid sidecars; either way the stale
        // ones must not be left next to the new database.
        for sidecar in [format!("{FILE}-wal"), format!("{FILE}-shm")] {
            let left = fs::read_to_string(dir.join(&sidecar)).unwrap_or_default();
            assert!(!left.starts_with("stale"), "{sidecar} still holds the old contents");
        }
        let StoreStatus::Recovered { notice } = db.status() else { panic!("expected Recovered") };
        assert!(notice.contains(main.as_str()));
    }

    #[test]
    fn damage_past_the_header_never_loses_tabs_silently() {
        let path = temp_db("deep-corrupt");
        let db = Db::open(path.clone()).unwrap();
        let tabs = (0..200).map(|i| terminal_tab(&format!("tab-{i:03}-{}", "x".repeat(40)))).collect();
        db.save_layout(Layout { active: 0, tabs }).unwrap();
        drop(db);
        // Fold the WAL back in, then scribble over a page after the header.
        Connection::open(&path).unwrap().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        let mut bytes = fs::read(&path).unwrap();
        assert!(bytes.len() > 3 * 4096, "the test needs several pages");
        bytes[2 * 4096..3 * 4096].fill(0xA5);
        fs::write(&path, bytes).unwrap();

        let db = Db::open_or_recover(Ok(path.clone()));
        match db.status() {
            // Detected on open: moved aside and said so.
            StoreStatus::Recovered { .. } => {}
            // Otherwise loading either fails (the frontend then stops saving) or, if
            // the damaged page held no tab rows, still returns every tab.
            _ => {
                if let Ok(layout) = db.load_layout() {
                    assert_eq!(layout.tabs.len(), 200, "a damaged table must not load as fewer tabs");
                }
            }
        }
    }

    #[test]
    fn a_failed_move_puts_everything_back() {
        let dir = temp_db("move-back").parent().unwrap().to_path_buf();
        fs::create_dir_all(&dir).unwrap();
        let from = [dir.join("a-wal"), dir.join("a")];
        fs::write(&from[0], "wal").unwrap();
        fs::write(&from[1], "db").unwrap();
        // The second target's directory does not exist, so that rename fails.
        let to = [dir.join("b-wal"), dir.join("missing").join("b")];
        assert!(move_files(&from, &to).is_err());
        assert_eq!(fs::read_to_string(&from[0]).unwrap(), "wal");
        assert_eq!(fs::read_to_string(&from[1]).unwrap(), "db");
        assert!(!to[0].exists());
    }

    #[test]
    fn leaves_a_newer_database_alone() {
        let path = temp_db("too-new");
        drop(Db::open(path.clone()).unwrap());
        set_version(&path, 99);
        assert!(matches!(Db::open(path.clone()), Err(OpenError::TooNew { found: 99, .. })));
        let db = Db::open_or_recover(Ok(path.clone()));
        assert!(files_named(path.parent().unwrap(), &format!("{FILE}.broken-")).is_empty());
        assert_eq!(db.hook_path(), None);
        let StoreStatus::NotSaving { reason } = db.status() else { panic!("expected NotSaving") };
        assert!(reason.contains("newer Mischief of Agents"));
    }

    #[cfg(unix)]
    #[test]
    fn leaves_an_unreadable_database_alone() {
        if crate::test_support::running_as_root() {
            return;
        }
        let path = temp_db("no-access");
        drop(Db::open(path.clone()).unwrap());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let db = Db::open_or_recover(Ok(path.clone()));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(files_named(path.parent().unwrap(), &format!("{FILE}.broken-")).is_empty());
        assert!(path.exists());
        assert!(matches!(db.status(), StoreStatus::NotSaving { .. }));
        db.save_layout(Layout { active: 0, tabs: vec![terminal_tab("a")] }).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs.len(), 1, "still works, in memory");
    }

    #[test]
    fn runs_in_memory_without_a_data_directory() {
        let db = Db::open_or_recover(Err("no home".into()));
        assert_eq!(db.hook_path(), None);
        let StoreStatus::NotSaving { reason } = db.status() else { panic!("expected NotSaving") };
        assert!(reason.contains("no home"), "{reason}");
        db.save_layout(Layout { active: 0, tabs: vec![terminal_tab("a")] }).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs.len(), 1);
    }

    #[test]
    fn finds_the_latest_report_per_tab_or_overall() {
        let path = temp_db("latest-report");
        let db = Db::open(path.clone()).unwrap();
        assert_eq!(db.latest_report_at(None).unwrap(), None);
        record_claude_session(&connect_existing(&path).unwrap(), &report(&tid("a"), &sid(S1))).unwrap();
        assert!(db.latest_report_at(Some(&tid("a"))).unwrap().is_some());
        assert!(db.latest_report_at(None).unwrap().is_some());
        assert_eq!(db.latest_report_at(Some(&tid("b"))).unwrap(), None);
    }

    #[test]
    fn knows_the_session_a_tab_moved_to() {
        let path = temp_db("reported-session");
        let db = Db::open(path.clone()).unwrap();
        assert_eq!(db.reported_session(&tid("a")).unwrap(), None);
        record_claude_session(&connect_existing(&path).unwrap(), &report(&tid("a"), &sid(S2))).unwrap();
        assert_eq!(db.reported_session(&tid("a")).unwrap(), Some(sid(S2)));
    }

    #[test]
    fn lists_the_session_each_tab_reported() {
        let path = temp_db("reported-sessions");
        let db = Db::open(path.clone()).unwrap();
        assert!(db.reported_sessions().unwrap().is_empty());
        let conn = connect_existing(&path).unwrap();
        record_claude_session(&conn, &report(&tid("a"), &sid(S1))).unwrap();
        record_claude_session(&conn, &report(&tid("b"), &sid(S2))).unwrap();
        let all = db.reported_sessions().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["a"], sid(S1));
        assert_eq!(all["b"], sid(S2));
    }

    #[test]
    fn the_newest_transcript_wins_when_tabs_share_a_session() {
        let path = temp_db("shared-session");
        let db = Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        let s1 = sid(S1);
        let (a, b) = (tid("a"), tid("b"));
        record_claude_session(&conn, &SessionReport { transcript_path: Some("/old.jsonl"), ..report(&a, &s1) }).unwrap();
        record_claude_session(&conn, &SessionReport { transcript_path: Some("/new.jsonl"), ..report(&b, &s1) }).unwrap();
        conn.execute("UPDATE claude_sessions SET updated_at = updated_at - 10 WHERE tab_id = 'a'", []).unwrap();
        assert_eq!(db.transcript_for(&s1).unwrap(), Some(PathBuf::from("/new.jsonl")));
        conn.execute("UPDATE claude_sessions SET updated_at = updated_at - 20 WHERE tab_id = 'b'", []).unwrap();
        assert_eq!(db.transcript_for(&s1).unwrap(), Some(PathBuf::from("/old.jsonl")));
    }

    #[test]
    fn a_saved_bad_tab_id_fails_loading() {
        let path = temp_db("bad-tab-id");
        let db = Db::open(path.clone()).unwrap();
        db.save_layout(Layout { active: 0, tabs: vec![terminal_tab("a")] }).unwrap();
        connect(&path).unwrap().execute("UPDATE tabs SET tab_id = 'a b'", []).unwrap();
        assert!(db.load_layout().unwrap_err().contains("invalid tab id"));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_is_not_used() {
        #[cfg(unix)]
        use std::os::unix::ffi::OsStrExt;
        let dir = crate::test_support::temp_dir("non-utf8");
        let path = dir.join(std::ffi::OsStr::from_bytes(b"misag-\xff.db"));
        assert!(matches!(Db::open(path.clone()), Err(OpenError::Other(_))));
        let db = Db::open_or_recover(Ok(path.clone()));
        assert_eq!(db.hook_path(), None, "the hook could not be told where it is");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_damaged_database_that_cannot_be_moved_runs_in_memory() {
        if crate::test_support::running_as_root() {
            return;
        }
        let path = temp_db("corrupt-locked");
        let dir = path.parent().unwrap().to_path_buf();
        fs::write(&path, "this is not a database, just text that is long enough to have a header").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let db = Db::open_or_recover(Ok(path.clone()));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let StoreStatus::NotSaving { reason } = db.status() else { panic!("expected NotSaving") };
        assert!(reason.contains("could not be moved aside"), "{reason}");
        assert!(fs::read_to_string(&path).unwrap().starts_with("this is not a database"), "left alone");
    }

    #[test]
    fn the_hook_refuses_an_older_schema_too() {
        let path = temp_db("hook-older");
        drop(Db::open(path.clone()).unwrap());
        set_version(&path, 0);
        let err = connect_existing(&path).unwrap_err();
        assert!(err.contains("schema 0"), "{err}");
    }

    #[test]
    fn the_hook_connection_never_creates_or_migrates() {
        let path = temp_db("hook-conn");
        assert!(connect_existing(&path).is_err());
        assert!(!path.exists(), "nothing is created");

        drop(Db::open(path.clone()).unwrap());
        set_version(&path, 99);
        let err = connect_existing(&path).unwrap_err();
        assert!(err.contains("schema 99"), "{err}");
    }

    #[test]
    fn an_imported_flow_is_added_after_the_others_with_its_steps() {
        let db = Db::open(temp_db("import-flow")).unwrap();
        db.create_flow("First").unwrap();
        let graph = crate::flows::tests::runnable();
        let flow = db.import_flow("  Second ", &graph).unwrap();
        assert_eq!((flow.name.as_str(), flow.archived), ("Second", false));
        assert_eq!(flow_names(&db), names(&["First", "Second"]));
        assert_eq!(db.load_flow(flow.id).unwrap().1, graph);
    }

    #[test]
    fn a_flow_that_cannot_be_stored_is_not_imported() {
        let db = Db::open(temp_db("import-bad-flow")).unwrap();
        let mut graph = crate::flows::tests::runnable();
        graph.nodes[1].kind = crate::flows::Kind::Start;
        graph.nodes[1].outputs = vec!["next".into()];
        assert_eq!(db.import_flow("Two starts", &graph).unwrap_err(), "a flow has exactly one Start step");
        assert_eq!(db.import_flow(" ", &crate::flows::tests::runnable()).unwrap_err(), "a flow needs a name");
        // A name is printed in terminals (misag flow list) and shown to Claude.
        for (name, code) in [("red\u{1b}[31m", "001B"), ("a\u{200B}b", "200B"), ("x\u{202E}y", "202E")] {
            assert_eq!(
                db.import_flow(name, &crate::flows::tests::runnable()).unwrap_err(),
                format!("a flow name cannot hold control or hidden characters (U+{code})")
            );
            assert!(db.create_flow(name).is_err());
        }
        assert!(db.list_flows().unwrap().is_empty());
    }

    fn flow_names(db: &Db) -> Vec<(String, bool)> {
        db.list_flows().unwrap().into_iter().map(|f| (f.name, f.archived)).collect()
    }

    fn names(list: &[&str]) -> Vec<(String, bool)> {
        list.iter().map(|n| (n.to_string(), false)).collect()
    }

    #[test]
    fn flows_are_created_in_order_with_trimmed_names() {
        let db = Db::open(temp_db("flows-create")).unwrap();
        assert!(db.list_flows().unwrap().is_empty());
        let deploy = db.create_flow("  Deploy ").unwrap();
        assert_eq!((deploy.name.as_str(), deploy.archived), ("Deploy", false));
        db.create_flow("Review").unwrap();
        assert_eq!(flow_names(&db), names(&["Deploy", "Review"]));
        assert_ne!(db.list_flows().unwrap()[0].id, db.list_flows().unwrap()[1].id);
    }

    #[test]
    fn a_flow_needs_a_name() {
        let db = Db::open(temp_db("flows-name")).unwrap();
        let err = db.create_flow("   ").unwrap_err();
        assert!(err.contains("name"), "{err}");
        assert!(db.list_flows().unwrap().is_empty());
        let id = db.create_flow("Deploy").unwrap().id;
        assert!(db.rename_flow(id, " ").is_err());
        db.rename_flow(id, " Ship it ").unwrap();
        assert_eq!(flow_names(&db), names(&["Ship it"]));
        let err = db.rename_flow(id + 100, "Gone").unwrap_err();
        assert!(err.contains("no flow"), "{err}");
        assert!(db.rename_flow(id, &"x".repeat(MAX_FLOW_NAME)).is_ok());
        let err = db.rename_flow(id, &"x".repeat(MAX_FLOW_NAME + 1)).unwrap_err();
        assert!(err.contains("at most"), "{err}");
        assert!(db.create_flow(&"x".repeat(MAX_FLOW_NAME + 1)).is_err());
    }

    #[test]
    fn archiving_keeps_a_flows_place() {
        let db = Db::open(temp_db("flows-archive")).unwrap();
        let ids: Vec<i64> = ["a", "b", "c"].iter().map(|n| db.create_flow(n).unwrap().id).collect();
        db.set_flow_archived(ids[1], true).unwrap();
        let archived = vec![("a".into(), false), ("b".into(), true), ("c".into(), false)];
        assert_eq!(flow_names(&db), archived);
        db.set_flow_archived(ids[1], false).unwrap();
        assert_eq!(flow_names(&db), names(&["a", "b", "c"]));
        assert!(db.set_flow_archived(ids[2] + 100, true).unwrap_err().contains("no flow"));
    }

    #[test]
    fn flows_are_reordered_only_by_a_complete_order() {
        let db = Db::open(temp_db("flows-reorder")).unwrap();
        let ids: Vec<i64> = ["a", "b", "c"].iter().map(|n| db.create_flow(n).unwrap().id).collect();
        db.reorder_flows(&[ids[2], ids[0], ids[1]]).unwrap();
        assert_eq!(flow_names(&db), names(&["c", "a", "b"]));
        // Missing, repeated and unknown ids leave the order alone.
        for bad in [vec![ids[0], ids[1]], vec![ids[0], ids[0], ids[1]], vec![ids[0], ids[1], ids[2], 999]] {
            assert!(db.reorder_flows(&bad).is_err(), "{bad:?}");
            assert_eq!(flow_names(&db), names(&["c", "a", "b"]));
        }
        // A new flow still goes last.
        db.create_flow("d").unwrap();
        assert_eq!(flow_names(&db), names(&["c", "a", "b", "d"]));
    }

    #[test]
    fn a_deleted_flow_is_gone() {
        let db = Db::open(temp_db("flows-delete")).unwrap();
        let a = db.create_flow("a").unwrap().id;
        db.create_flow("b").unwrap();
        db.delete_flow(a).unwrap();
        assert_eq!(flow_names(&db), names(&["b"]));
        assert!(db.delete_flow(a).unwrap_err().contains("no flow"));
    }

    #[test]
    fn the_schema_refuses_a_blank_flow_name() {
        let db = Db::open(temp_db("flows-blank")).unwrap();
        let blank = db.conn().execute(
            "INSERT INTO flows (name, position, created_at, updated_at) VALUES ('  ', 0, 0, 0)",
            [],
        );
        assert!(blank.is_err());
    }

    #[test]
    fn a_new_flow_starts_with_its_start_step_and_its_graph_round_trips() {
        let db = Db::open(temp_db("flows-graph")).unwrap();
        let id = db.create_flow("Deploy").unwrap().id;
        let (flow, graph) = db.load_flow(id).unwrap();
        assert_eq!(flow.name, "Deploy");
        assert_eq!(graph, Graph::default());
        let runnable = crate::flows::tests::runnable();
        db.save_flow_graph(id, &runnable).unwrap();
        assert_eq!(db.load_flow(id).unwrap().1, runnable);
        assert!(db.load_flow(id + 100).unwrap_err().contains("no flow"));
        assert!(db.save_flow_graph(id + 100, &runnable).unwrap_err().contains("no flow"));
    }

    #[test]
    fn a_graph_is_stored_as_the_oldest_format_that_holds_it() {
        let db = Db::open(temp_db("flows-version")).unwrap();
        let id = db.create_flow("Deploy").unwrap().id;
        let mut g = crate::flows::tests::runnable();
        g.nodes[1].max_runs = Some(2);
        db.save_flow_graph(id, &g).unwrap();
        let (_, loaded) = db.load_flow(id).unwrap();
        assert_eq!((loaded.version, loaded.nodes[1].max_runs), (2, Some(2)));
    }

    #[test]
    fn the_list_says_which_flows_start_with_clear() {
        let db = Db::open(temp_db("flows-clear")).unwrap();
        let plain = db.create_flow("Deploy").unwrap().id;
        let clears = db.create_flow("Review").unwrap().id;
        let mut g = crate::flows::tests::runnable();
        g.nodes[0].clear = true;
        db.save_flow_graph(clears, &g).unwrap();
        let listed: Vec<_> = db.list_flows().unwrap().into_iter().map(|f| (f.id, f.clear)).collect();
        assert_eq!(listed, vec![(plain, false), (clears, true)]);
    }

    #[test]
    fn the_list_reads_clear_as_flows_clears_does() {
        let db = Db::open(temp_db("flows-clear-parity")).unwrap();
        let id = db.create_flow("Deploy").unwrap().id;
        let listed = || db.list_flows().unwrap()[0].clear;
        let mut g = crate::flows::tests::runnable();
        for clear in [false, true] {
            g.nodes[0].clear = clear;
            db.save_flow_graph(id, &g).unwrap();
            assert_eq!(listed(), crate::flows::clears(&g));
        }
        // Rows check would refuse, which only a hand edit or an older bug could store.
        let raw = |json: &str| db.conn().execute("UPDATE flows SET graph = ?1 WHERE id = ?2", params![json, id]).unwrap();
        raw(r#"{"nodes":[{"id":"p","kind":"prompt","clear":true,"x":0,"y":0}],"edges":[]}"#);
        assert!(!listed(), "only Start clears");
        raw(r#"{"nodes":[{"id":"start","kind":"start","clear":"yes","x":0,"y":0}],"edges":[]}"#);
        assert!(!listed());
        raw("not json");
        assert!(!listed());
    }

    #[test]
    fn a_graph_that_fails_its_check_is_not_saved() {
        let db = Db::open(temp_db("flows-bad-graph")).unwrap();
        let id = db.create_flow("Deploy").unwrap().id;
        let mut bad = crate::flows::tests::runnable();
        bad.edges[0].to = "ghost".into();
        assert!(db.save_flow_graph(id, &bad).unwrap_err().contains("ghost"));
        assert_eq!(db.load_flow(id).unwrap().1, Graph::default());
    }

    #[test]
    fn a_flow_stored_without_steps_loads_as_a_new_one() {
        let db = Db::open(temp_db("flows-empty-graph")).unwrap();
        let id = db.create_flow("Deploy").unwrap().id;
        db.conn().execute("UPDATE flows SET graph = '{\"nodes\":[],\"edges\":[]}'", []).unwrap();
        assert_eq!(db.load_flow(id).unwrap().1, Graph::default());
        db.conn().execute("UPDATE flows SET graph = 'not json'", []).unwrap();
        assert!(db.load_flow(id).unwrap_err().contains("cannot be read"));
    }

    #[test]
    fn every_connection_enforces_foreign_keys() {
        let path = temp_db("foreign-keys");
        let db = Db::open(path.clone()).unwrap();
        let on = |conn: &Connection| conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0)).unwrap();
        assert!(on(&db.conn()));
        assert!(on(&connect_existing(&path).unwrap()));
        assert!(on(&Db::open_or_recover(Err("none".into())).conn()));
    }

    #[test]
    fn upgrading_from_schema_6_keeps_the_flows_and_adds_runs() {
        let path = temp_db("upgrade-v6");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&MIGRATIONS[..6].concat()).unwrap();
        conn.pragma_update(None, "user_version", 6).unwrap();
        conn.execute("INSERT INTO flows (name, position, created_at, updated_at) VALUES ('Deploy', 0, 0, 0)", []).unwrap();
        drop(conn);
        let db = Db::open(path.clone()).unwrap();
        assert_eq!(schema_version(&connect(&path).unwrap()).unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(flow_names(&db), names(&["Deploy"]));
        assert!(crate::runs::active(&connect_existing(&path).unwrap()).unwrap().is_empty());
    }

    #[test]
    fn upgrading_asks_before_running_a_flow_whose_hidden_start_text_now_goes_to_claude() {
        let path = temp_db("upgrade-base-info");
        let conn = Connection::open(&path).unwrap();
        let before = MIGRATIONS.iter().position(|m| m.contains("ADD COLUMN briefed")).unwrap();
        conn.execute_batch(&MIGRATIONS[..before].concat()).unwrap();
        conn.pragma_update(None, "user_version", before as i64).unwrap();
        let start = |text: &str, version: u32| {
            format!(r#"{{"version": {version}, "nodes": [{{"id": "start", "kind": "start", "text": "{text}", "outputs": ["next"], "x": 0, "y": 0}}], "edges": []}}"#)
        };
        for (name, graph) in [("hidden", start("Do more.", 3)), ("blank", start(" ", 3)), ("none", r#"{"nodes":[],"edges":[]}"#.into())] {
            conn.execute(
                "INSERT INTO flows (name, position, graph, created_at, updated_at) VALUES (?1, 0, ?2, 0, 0)",
                params![name, graph],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, started_at, updated_at) VALUES (1, 't1', '{}', 'p', 'running', 0, 0)",
            [],
        )
        .unwrap();
        drop(conn);
        let db = Db::open(path.clone()).unwrap();
        let unconfirmed: Vec<_> = db.list_flows().unwrap().into_iter().map(|f| (f.name, f.unconfirmed)).collect();
        assert_eq!(unconfirmed, [("hidden".to_string(), true), ("blank".to_string(), false), ("none".to_string(), false)]);
        // A run started before had sent no base info, and owes none.
        let briefed: bool = connect_existing(&path).unwrap().query_row("SELECT briefed FROM flow_runs", [], |r| r.get(0)).unwrap();
        assert!(briefed);
    }

    #[test]
    fn upgrading_times_a_briefing_sent_before_from_the_runs_last_change() {
        let path = temp_db("upgrade-briefing-at");
        let conn = Connection::open(&path).unwrap();
        let before = MIGRATIONS.iter().position(|m| m.contains("ADD COLUMN briefing_at")).unwrap();
        conn.execute_batch(&MIGRATIONS[..before].concat()).unwrap();
        conn.pragma_update(None, "user_version", before as i64).unwrap();
        conn.execute("INSERT INTO flows (name, position, graph, created_at, updated_at) VALUES ('F', 0, '{}', 0, 0)", []).unwrap();
        conn.execute(
            "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, started_at, updated_at, briefing) VALUES (1, 't1', '{}', 'start', 'running', 0, 0, 1)",
            [],
        )
        .unwrap();
        drop(conn);
        Db::open(path.clone()).unwrap();
        let mut conn = connect_existing(&path).unwrap();
        let briefing_at: Option<i64> = conn.query_row("SELECT briefing_at FROM flow_runs", [], |r| r.get(0)).unwrap();
        assert_eq!(briefing_at, Some(0));
        // Long past its timeout, so the run waits for the person.
        crate::runs::expire(&mut conn).unwrap();
        let runs = crate::runs::active(&conn).unwrap();
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].status, crate::runs::Status::Attention);
    }

    #[test]
    fn a_running_flow_is_not_deleted_and_a_finished_one_takes_its_runs_along() {
        let path = temp_db("flows-delete-runs");
        let db = Db::open(path.clone()).unwrap();
        let mut runs = connect_existing(&path).unwrap();
        let runnable = crate::flows::tests::runnable();
        let ids: Vec<i64> = ["busy", "idle"].iter().map(|n| db.create_flow(n).unwrap().id).collect();
        for (id, tab) in ids.iter().zip(["t1", "t2"]) {
            db.save_flow_graph(*id, &runnable).unwrap();
            crate::runs::start(&mut runs, *id, &tid(tab)).unwrap();
            // Claude's reply to the briefing takes the first step.
            crate::runs::on_stop(&mut runs, &tid(tab), Some("Ready")).unwrap();
        }
        let idle_run = crate::runs::active(&runs).unwrap()[1].id;
        crate::runs::stop(&runs, idle_run).unwrap();
        for status in ["running", "manual", "attention"] {
            // A run needs attention saying why (see the CHECKs on flow_runs).
            runs.execute("UPDATE flow_runs SET status = ?1, note = iif(?1 = 'attention', 'why', NULL) WHERE tab_id = 't1'", [status])
                .unwrap();
            let err = db.delete_flow(ids[0]).unwrap_err();
            assert!(err.contains("running"), "{status}: {err}");
        }
        db.delete_flow(ids[1]).unwrap();
        assert!(db.delete_flow(ids[1]).unwrap_err().contains("no flow"));
        let left: i64 = runs.query_row("SELECT COUNT(*) FROM flow_runs", [], |r| r.get(0)).unwrap();
        let events: i64 = runs.query_row("SELECT COUNT(*) FROM flow_run_events", [], |r| r.get(0)).unwrap();
        assert_eq!((left, events), (1, 1), "only the busy flow's run and its first step remain");
    }

    #[test]
    fn upgrading_keeps_the_settings_and_adds_the_font_size() {
        let path = temp_db("upgrade-font-size");
        let conn = Connection::open(&path).unwrap();
        let before = MIGRATIONS.iter().position(|m| m.contains("ADD COLUMN font_size")).unwrap();
        conn.execute_batch(&MIGRATIONS[..before].concat()).unwrap();
        conn.pragma_update(None, "user_version", before as i64).unwrap();
        conn.execute("UPDATE settings SET skip_permissions = 1", []).unwrap();
        drop(conn);
        let db = Db::open(path).unwrap();
        assert_eq!(db.load_settings().unwrap(), Settings { skip_permissions: true, ..Settings::default() });
        assert_eq!(db.save_settings(Settings { font_size: 20, ..Settings::default() }).unwrap().font_size, 20);
    }

    #[test]
    fn every_finish_sound_round_trips() {
        let db = Db::open(temp_db("finish-sounds")).unwrap();
        for sound in [FinishSound::Chime, FinishSound::Bell, FinishSound::Pop, FinishSound::Off] {
            let saved = db.save_settings(Settings { finish_sound: sound, ..Settings::default() }).unwrap();
            assert_eq!(saved.finish_sound, sound);
        }
    }

    #[test]
    fn an_unknown_stored_finish_sound_reads_as_off_and_keeps_the_other_settings() {
        let path = temp_db("finish-sound-unknown");
        let db = Db::open(path.clone()).unwrap();
        db.save_settings(Settings { skip_permissions: true, ..Settings::default() }).unwrap();
        let conn = connect_existing(&path).unwrap();
        conn.pragma_update(None, "ignore_check_constraints", true).unwrap();
        conn.execute("UPDATE settings SET finish_sound = 'siren'", []).unwrap();
        let loaded = db.load_settings().unwrap();
        assert_eq!(loaded, Settings { skip_permissions: true, ..Settings::default() });
        // So the Settings tab can still save, which replaces it.
        assert_eq!(db.save_settings(Settings { finish_sound: FinishSound::Pop, ..loaded }).unwrap().finish_sound, FinishSound::Pop);
    }

    #[test]
    fn upgrading_keeps_the_settings_and_adds_the_finish_sound() {
        let path = temp_db("upgrade-finish-sound");
        let conn = Connection::open(&path).unwrap();
        let before = MIGRATIONS.iter().position(|m| m.contains("ADD COLUMN finish_sound")).unwrap();
        conn.execute_batch(&MIGRATIONS[..before].concat()).unwrap();
        conn.pragma_update(None, "user_version", before as i64).unwrap();
        conn.execute("UPDATE settings SET font_size = 20", []).unwrap();
        drop(conn);
        let db = Db::open(path).unwrap();
        assert_eq!(db.load_settings().unwrap(), Settings { font_size: 20, ..Settings::default() });
        let saved = db.save_settings(Settings { finish_sound: FinishSound::Pop, ..Settings::default() }).unwrap();
        assert_eq!(saved.finish_sound, FinishSound::Pop);
    }

    #[test]
    fn upgrading_from_schema_5_keeps_the_settings_and_adds_flows() {
        let path = temp_db("upgrade-v5");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&MIGRATIONS[..5].concat()).unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();
        conn.execute("UPDATE settings SET disable_auto_memory = 1", []).unwrap();
        drop(conn);
        let db = Db::open(path.clone()).unwrap();
        assert_eq!(schema_version(&connect(&path).unwrap()).unwrap(), MIGRATIONS.len() as i64);
        assert!(db.load_settings().unwrap().disable_auto_memory);
        assert!(db.list_flows().unwrap().is_empty());
        db.create_flow("Deploy").unwrap();
    }

    /// The database at `path` at the schema before flow_runs' cross-column CHECKs,
    /// holding one flow, whose runs `old_run` adds.
    fn before_run_checks(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        let before = MIGRATIONS.iter().position(|m| m.contains("CREATE TABLE flow_runs_new")).unwrap();
        assert_eq!(before, RUN_CHECKS, "normalise_runs goes with the migration adding the CHECKs");
        conn.execute_batch(&MIGRATIONS[..before].concat()).unwrap();
        conn.pragma_update(None, "user_version", before as i64).unwrap();
        let graph = serde_json::to_string(&crate::flows::tests::runnable()).unwrap();
        conn.execute("INSERT INTO flows (name, position, graph, created_at, updated_at) VALUES ('F', 0, ?1, 0, 0)", [graph]).unwrap();
        conn
    }

    /// Adds a run at step p of flow 1 in tab `tab`: inserts it as stopped, then sets
    /// the columns `set` gives, bypassing the typed writers, as an older version may
    /// have written them; its id.
    fn old_run(conn: &Connection, tab: &str, set: &str) -> i64 {
        conn.execute(
            "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, steps, started_at, updated_at)
             SELECT 1, ?1, graph, 'p', 'stopped', 3, 10, 20 FROM flows WHERE id = 1",
            [tab],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute(&format!("UPDATE flow_runs SET {set} WHERE id = ?1"), [id]).unwrap();
        id
    }

    /// Every column of every run, by id.
    fn run_rows(conn: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
        let columns = "id, flow_id, tab_id, graph, node_id, status, steps, note, started_at, updated_at, resume, clear, briefed,
                       prefill, prefill_token, briefing, closing, briefing_at";
        let mut stmt = conn.prepare(&format!("SELECT {columns} FROM flow_runs ORDER BY id")).unwrap();
        let n = stmt.column_count();
        stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect()).unwrap().collect::<Result<_, _>>().unwrap()
    }

    #[test]
    fn upgrading_keeps_runs_of_every_phase_as_they_were() {
        let path = temp_db("upgrade-run-checks-kept");
        let conn = before_run_checks(&path);
        for (tab, set) in [
            ("clearing", "status = 'running', clear = 'pending', briefed = 0"),
            ("cleared", "status = 'running', clear = 'done', briefed = 0"),
            ("briefing", "status = 'running', briefing = 1, briefing_at = 15, briefed = 0"),
            ("working", "status = 'running'"),
            ("waiting", "status = 'manual'"),
            ("pasting", "status = 'manual', prefill = 1, prefill_token = 7"),
            ("needs", "status = 'attention', note = 'why', resume = 'prompt', prefill = 1, prefill_token = 8"),
            ("stuck", "status = 'attention', note = 'why'"),
            // An ended run keeps what a stop left behind: only `closing` is read.
            ("done", "status = 'done', closing = 1, note = 'old', resume = 'remind', clear = 'pending', prefill = 1, briefing = 1, briefing_at = 5"),
            ("stopped", "status = 'stopped', briefing_at = 5"),
        ] {
            old_run(&conn, tab, set);
        }
        let before = run_rows(&conn);
        drop(conn);
        Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        assert_eq!(run_rows(&conn), before);
        let runs = crate::runs::active(&conn).unwrap();
        assert_eq!(runs.len(), 8);
        for run in runs {
            assert!(!run.note.as_deref().unwrap_or_default().contains("cannot be read"), "{run:?}");
        }
        // The rebuilt table keeps its indexes.
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'flow_runs' AND sql IS NOT NULL ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(indexes, ["flow_runs_by_flow", "flow_runs_closing", "one_active_run_per_tab"]);
    }

    #[test]
    fn upgrading_keeps_a_run_s_events_and_resumes_which_still_go_with_it() {
        let path = temp_db("upgrade-run-checks-children");
        let conn = before_run_checks(&path);
        let run = old_run(&conn, "t1", "status = 'running'");
        conn.execute("INSERT INTO flow_run_events (run_id, node_id, output, at) VALUES (?1, 'start', 'next', 0)", [run]).unwrap();
        conn.execute("INSERT INTO flow_run_resumes (run_id, node_id, note, at) VALUES (?1, 'p', 'why', 0)", [run]).unwrap();
        drop(conn);
        // The connection that migrated enforces foreign keys again, and they still
        // lead to the rebuilt table.
        let conn = connect(&path).unwrap();
        let fk: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert!(fk);
        let db = Db::open(path.clone()).unwrap();
        let count = |table: &str| -> i64 { conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap() };
        assert_eq!((count("flow_run_events"), count("flow_run_resumes")), (1, 1));
        crate::runs::stop(&conn, run).unwrap();
        db.delete_flow(1).unwrap();
        assert_eq!((count("flow_runs"), count("flow_run_events"), count("flow_run_resumes")), (0, 0, 0));
    }

    #[test]
    fn upgrading_drops_what_runs_left_behind_and_holds_runs_it_cannot_read_for_the_person() {
        let path = temp_db("upgrade-run-checks-normalised");
        let conn = before_run_checks(&path);
        // A briefing's time, which versions before kept after the briefing, goes.
        let stale = old_run(&conn, "stale", "status = 'running', briefing_at = 5");
        let stale_manual = old_run(&conn, "stale-manual", "status = 'manual', prefill = 1, prefill_token = 4, briefing_at = 5");
        let odd: Vec<i64> = [
            "status = 'running', note = 'old'",
            "status = 'running', resume = 'remind'",
            "status = 'running', prefill = 1",
            "status = 'running', clear = 'pending', briefing = 1, briefing_at = 5",
            "status = 'running', briefing = 1",
            "status = 'manual', closing = 1",
            "status = 'manual', note = 'old'",
            "status = 'manual', clear = 'done'",
            "status = 'attention'",
            "status = 'attention', note = 'old', briefing = 1, briefing_at = 5",
        ]
        .iter()
        .enumerate()
        .map(|(i, set)| old_run(&conn, &format!("odd{i}"), &format!("{set}, briefed = {}, prefill_token = 9", i % 2)))
        .collect();
        drop(conn);
        Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        type Row = (String, String, i64, Option<String>, Option<String>, Option<String>, bool, bool, Option<i64>, bool, Option<i64>, bool);
        let row = |id: i64| -> Row {
            conn.query_row(
                "SELECT node_id, status, steps, note, resume, clear, briefed, prefill, prefill_token, briefing, briefing_at, closing
                 FROM flow_runs WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?)),
            )
            .unwrap()
        };
        let p = || "p".to_string();
        assert_eq!(row(stale), (p(), "running".into(), 3, None, None, None, true, false, None, false, None, false));
        assert_eq!(row(stale_manual), (p(), "manual".into(), 3, None, None, None, true, true, Some(4), false, None, false));
        // A run no version could have left so is held for the person, where it was,
        // with what it knew of Claude (briefed) and its last paste's token.
        for (i, id) in odd.into_iter().enumerate() {
            let (node, status, steps, note, resume, clear, briefed, prefill, token, briefing, briefing_at, closing) = row(id);
            assert_eq!(
                (node, status, steps, resume, clear, briefed, prefill, token, briefing, briefing_at, closing),
                (p(), "attention".into(), 3, None, None, i % 2 == 1, false, Some(9), false, None, false),
                "odd{i}"
            );
            let note = note.unwrap();
            assert!(note.contains("could not be read"), "odd{i}");
            // Only a run that already needed attention had a note worth keeping.
            assert_eq!(note.ends_with(" (it said: old)"), i == 9, "odd{i}: {note}");
        }
        for run in crate::runs::active(&conn).unwrap() {
            assert!(!run.note.as_deref().unwrap_or_default().contains("cannot be read"), "{run:?}");
        }
    }

    #[test]
    fn upgrading_names_the_runs_it_holds_for_the_person() {
        let path = temp_db("upgrade-run-checks-held");
        let mut conn = before_run_checks(&path);
        old_run(&conn, "stale", "status = 'running', briefing_at = 5");
        old_run(&conn, "fine", "status = 'manual'");
        let odd = old_run(&conn, "odd", "status = 'manual', note = 'old'");
        old_run(&conn, "ended", "status = 'done', note = 'old'");
        let tx = conn.transaction().unwrap();
        assert_eq!(normalise_runs(&tx).unwrap(), [odd]);
    }

    #[test]
    fn a_failed_upgrade_changes_nothing_and_enforces_foreign_keys_again() {
        let path = temp_db("upgrade-run-checks-failed");
        let conn = before_run_checks(&path);
        // A value no version knows, as a database edited by hand may hold, on an
        // ended run, which the upgrade leaves as it is; the rebuilt table refuses it.
        conn.pragma_update(None, "ignore_check_constraints", true).unwrap();
        old_run(&conn, "later", "status = 'stopped', resume = 'later'");
        old_run(&conn, "working", "status = 'running'");
        let table_sql = |conn: &Connection| -> String {
            conn.query_row("SELECT sql FROM sqlite_master WHERE name = 'flow_runs'", [], |r| r.get(0)).unwrap()
        };
        let (before_sql, before_rows) = (table_sql(&conn), run_rows(&conn));
        let version = schema_version(&conn).unwrap();
        drop(conn);
        let e = Db::open(path.clone()).err().unwrap().to_string();
        assert!(e.contains(&format!("migration {}", version + 1)) && e.contains("CHECK constraint failed"), "{e}");
        let mut conn = Connection::open(&path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), version);
        assert_eq!((table_sql(&conn), run_rows(&conn)), (before_sql, before_rows));
        // The connection that tried is back to enforcing foreign keys.
        assert!(migrate(&mut conn).is_err());
        let fk: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert!(fk);
    }

    #[test]
    fn rows_whose_foreign_keys_led_nowhere_before_do_not_block_an_upgrade() {
        let path = temp_db("upgrade-old-orphan");
        let conn = before_run_checks(&path);
        // As a connection with foreign keys off may have left it.
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute(
            "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, started_at, updated_at) VALUES (99, 'orphan', '{}', 'p', 'stopped', 0, 0)",
            [],
        )
        .unwrap();
        let orphan = conn.last_insert_rowid();
        let found = orphans(&conn).unwrap();
        assert_eq!(found, [Orphan { table: "flow_runs".into(), rowid: Some(orphan), parent: "flows".into() }]);
        drop(conn);
        Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        // Still there, for the person (and the log) to see.
        assert_eq!(orphans(&conn).unwrap(), found);
    }

    #[test]
    fn a_migration_that_leaves_rows_leading_nowhere_fails_naming_them() {
        let path = temp_db("upgrade-new-orphan");
        let db = Db::open(path.clone()).unwrap();
        let flow = db.create_flow("F").unwrap().id;
        db.save_flow_graph(flow, &crate::flows::tests::runnable()).unwrap();
        let mut runs = connect_existing(&path).unwrap();
        let run = crate::runs::start(&mut runs, flow, &tid("t1")).unwrap().run_id;
        crate::runs::stop(&runs, run).unwrap();
        drop((db, runs));
        let mut migrations = MIGRATIONS.to_vec();
        migrations.push("DELETE FROM flows;");
        let mut conn = Connection::open(&path).unwrap();
        let e = migrate_with(&mut conn, &migrations).unwrap_err().to_string();
        assert!(e.contains(&format!("migration {}", migrations.len())) && e.contains(&format!("flow_runs row {run} (no flows row)")), "{e}");
        assert_eq!(schema_version(&conn).unwrap(), MIGRATIONS.len() as i64);
        let flows: i64 = conn.query_row("SELECT COUNT(*) FROM flows", [], |r| r.get(0)).unwrap();
        assert_eq!(flows, 1);
        let fk: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert!(fk);
    }

    #[test]
    fn after_upgrading_the_database_refuses_a_run_state_that_cannot_be() {
        let path = temp_db("upgrade-run-checks-refuse");
        Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        let graph = serde_json::to_string(&crate::flows::tests::runnable()).unwrap();
        conn.execute("INSERT INTO flows (name, position, graph, created_at, updated_at) VALUES ('F', 0, ?1, 0, 0)", [graph]).unwrap();
        let run = old_run(&conn, "t1", "status = 'running'");
        for (set, check) in [
            ("note = 'why'", "note_only_when_attention"),
            ("status = 'attention'", "note_only_when_attention"),
            ("status = 'manual', note = 'why'", "note_only_when_attention"),
            ("resume = 'remind'", "resume_only_when_attention"),
            ("status = 'manual', resume = 'prompt'", "resume_only_when_attention"),
            ("status = 'manual', clear = 'pending'", "clear_only_when_running"),
            ("status = 'attention', note = 'why', clear = 'done'", "clear_only_when_running"),
            ("briefing = 1", "briefing_only_when_running"),
            ("briefing = 1, briefing_at = 1, clear = 'pending'", "briefing_only_when_running"),
            ("status = 'manual', briefing = 1, briefing_at = 1", "briefing_only_when_running"),
            ("briefing_at = 1", "briefing_at_only_when_briefing"),
            ("prefill = 1", "prefill_only_when_waiting"),
            ("closing = 1", "closing_only_when_ended"),
            ("status = 'manual', closing = 1", "closing_only_when_ended"),
        ] {
            let e = conn.execute(&format!("UPDATE flow_runs SET {set} WHERE id = ?1"), [run]).unwrap_err().to_string();
            assert!(e.contains("CHECK constraint failed") && e.contains(check), "{set}: {e}");
        }
        let e = conn
            .execute(
                "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, started_at, updated_at, closing) VALUES (1, 't2', '{}', 'p', 'manual', 0, 0, 1)",
                [],
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("closing_only_when_ended"), "{e}");
        // Every state Phase has is allowed, and an ended run keeps whatever it was left with.
        for set in [
            "clear = 'pending'",
            "clear = 'done'",
            "clear = NULL, briefing = 1, briefing_at = 1",
            "briefing = 0, briefing_at = NULL",
            "status = 'manual', prefill = 1",
            "status = 'attention', note = 'why', resume = 'remind'",
            "status = 'stopped', briefing = 1, clear = 'done', closing = 1",
            "status = 'done', closing = 0, prefill = 1",
        ] {
            conn.execute(&format!("UPDATE flow_runs SET {set} WHERE id = ?1"), [run]).unwrap_or_else(|e| panic!("{set}: {e}"));
        }
    }

    #[test]
    fn after_upgrading_a_run_this_version_cannot_read_still_stops_and_its_note_owed_can_be_dropped() {
        let path = temp_db("upgrade-run-checks-unreadable");
        let db = Db::open(path.clone()).unwrap();
        let flow = db.create_flow("F").unwrap().id;
        db.save_flow_graph(flow, &crate::flows::tests::runnable()).unwrap();
        let mut conn = connect_existing(&path).unwrap();
        let run = crate::runs::start(&mut conn, flow, &tid("t1")).unwrap().run_id;
        // As a newer version might store it, past the CHECKs, which are back for the stop.
        conn.execute_batch("PRAGMA ignore_check_constraints = ON").unwrap();
        conn.execute("UPDATE flow_runs SET briefing = 0, briefing_at = NULL, briefed = 1, note = 'odd', resume = 'someday' WHERE id = ?1", [run])
            .unwrap();
        conn.execute_batch("PRAGMA ignore_check_constraints = OFF").unwrap();
        crate::runs::stop(&conn, run).unwrap();
        let (status, closing): (String, bool) =
            conn.query_row("SELECT status, closing FROM flow_runs WHERE id = ?1", [run], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((status.as_str(), closing), ("stopped", true));
        // The next run in the tab drops the note the stopped one owed.
        crate::runs::start(&mut conn, flow, &tid("t1")).unwrap();
        let closing: bool = conn.query_row("SELECT closing FROM flow_runs WHERE id = ?1", [run], |r| r.get(0)).unwrap();
        assert!(!closing);
    }
}
