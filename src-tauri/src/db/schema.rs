//! The schema and its migrations, and the connections to the database: the app's,
//! which migrates, and the hook's and `misag flow`'s, which never do.

use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior};
use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::time::Duration;

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
    -- 1 while a run waited at a manual prompt whose text the app had yet to put into
    -- Claude's input. Unused since manual prompts went: see runs::RunState::from_columns.
    ALTER TABLE flow_runs ADD COLUMN prefill INTEGER NOT NULL DEFAULT 0 CHECK (prefill IN (0, 1));
"#, r#"
    -- Named the paste a run owed while prefill was 1. Unused since manual prompts
    -- went: see runs::RunState::from_columns.
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
        -- 1 while a run waited at a manual prompt whose text the app had yet to put
        -- into Claude's input. Unused since: see runs::RunState::from_columns.
        prefill       INTEGER NOT NULL DEFAULT 0 CHECK (prefill IN (0, 1)),
        -- Named the paste a run owed while prefill was 1. Unused since.
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
        -- A paste was owed only at a manual prompt (Waiting, NeedsPerson); every save
        -- writes 0 since manual prompts went.
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
"#, r#"
    -- Not 0 while a run runs a step whose prompt the app typed into the tab while
    -- Claude may still have been answering the step before (see runs::Phase::Typed),
    -- until Claude Code reports that prompt submitted: a turn that ends before then
    -- is not its reply. 1 until a turn ends, 2 after (a second one waits for the
    -- person). Like the CHECKs above, it holds active runs only: an ended run keeps
    -- what it was left with, as a raw stop of a run a version cannot read leaves the
    -- rest of the row as it was, and runs::RunState::from_columns ignores it there.
    ALTER TABLE flow_runs ADD COLUMN typed INTEGER NOT NULL DEFAULT 0
        CONSTRAINT typed_only_when_running
        CHECK (typed IN (0, 1, 2) AND (status IN ('done', 'stopped') OR typed = 0
               OR (status = 'running' AND clear IS NULL AND briefing = 0)));
"#, r#"
    -- The tab titles Claude wrote at flow tab title steps, written by the Stop hook
    -- and taken by the app, which names the tabs (see runs::take_tab_titles).
    CREATE TABLE tab_titles (
        tab_id TEXT PRIMARY KEY,
        title  TEXT NOT NULL
    );
"#, r#"
    -- The output Claude picked at a prompt step that pauses once done, which the
    -- person goes on by with ▶ (see runs::Phase::Waiting): set only while the run
    -- waits for the person, owing no paste (manual prompts, which owed one, are gone). Like
    -- typed, it holds active runs only: an ended run keeps what it was left with,
    -- and runs::RunState::from_columns ignores it there.
    ALTER TABLE flow_runs ADD COLUMN suggested TEXT
        CONSTRAINT suggested_only_when_waiting
        CHECK (suggested IS NULL OR status IN ('done', 'stopped') OR (status = 'manual' AND prefill = 0));
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
    // the steps, what Claude has (briefed), the token of a paste a manual prompt owed
    // (which nothing reads now that they are gone) and a note a run needing attention had: the person sees the run and stops it, or picks an
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
pub(super) fn enforce_foreign_keys(conn: &Connection) -> Result<(), OpenError> {
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
        format!(
            "schema {found} in {}, this binary knows {known}: misag was likely updated and not yet restarted; restart it",
            path.display()
        )
    })
}

/// Like `connect_existing`, but None (no error) when the schema is not this
/// binary's, for callers that have nothing to do across an upgrade (the hook at a
/// prompt or a session end).
pub(crate) fn connect_if_current(path: &Path) -> Result<Option<Connection>, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    configure(&conn, path).map_err(|e| e.to_string())?;
    let found = schema_version(&conn).map_err(|e| e.to_string())?;
    Ok((found == MIGRATIONS.len() as i64).then_some(conn))
}

pub(super) fn migrate(conn: &mut Connection) -> Result<(), OpenError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_util::*;
    use crate::claude::SessionId;
    use crate::db::Db;
    use crate::layout::Tab;
    use crate::settings::{FinishSound, Settings};
    use rusqlite::params;

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
    fn upgrading_keeps_a_waiting_run_waiting_with_nothing_suggested() {
        let path = temp_db("upgrade-suggested");
        let conn = Connection::open(&path).unwrap();
        let before = MIGRATIONS.iter().position(|m| m.contains("ADD COLUMN suggested")).unwrap();
        conn.execute_batch(&MIGRATIONS[..before].concat()).unwrap();
        conn.pragma_update(None, "user_version", before as i64).unwrap();
        // A pause as the version before stored it, in the flow and in the run's copy.
        let graph = r#"{"version": 1, "nodes": [{"id": "start", "kind": "start", "outputs": ["next"], "x": 0, "y": 0},
            {"id": "m", "kind": "manual", "text": "Try it", "outputs": ["ok"], "x": 0, "y": 0}],
            "edges": [{"from": "start", "output": "next", "to": "m"}]}"#;
        conn.execute("INSERT INTO flows (name, position, graph, created_at, updated_at) VALUES ('F', 0, ?1, 0, 0)", [graph]).unwrap();
        conn.execute(
            "INSERT INTO flow_runs (flow_id, tab_id, graph, node_id, status, started_at, updated_at) VALUES (1, 't1', ?1, 'm', 'manual', 0, 0)",
            [graph],
        )
        .unwrap();
        drop(conn);
        Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        let suggested: Option<String> = conn.query_row("SELECT suggested FROM flow_runs", [], |r| r.get(0)).unwrap();
        assert_eq!(suggested, None);
        let runs = crate::runs::active(&conn).unwrap();
        assert_eq!((runs[0].status, runs[0].suggested.clone(), runs[0].outputs.clone()), (crate::runs::Status::Manual, None, vec!["ok".to_string()]));
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
        let mut conn = connect(&path).unwrap();
        let fk: bool = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert!(fk);
        let db = Db::open(path.clone()).unwrap();
        let count = |conn: &Connection, table: &str| -> i64 {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
        };
        assert_eq!((count(&conn, "flow_run_events"), count(&conn, "flow_run_resumes")), (1, 1));
        crate::runs::stop(&mut conn, run).unwrap();
        db.delete_flow(1).unwrap();
        assert_eq!((count(&conn, "flow_runs"), count(&conn, "flow_run_events"), count(&conn, "flow_run_resumes")), (0, 0, 0));
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
        crate::runs::stop(&mut runs, run).unwrap();
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
            ("typed = 1, clear = 'pending'", "typed_only_when_running"),
            ("typed = 1, briefing = 1, briefing_at = 1", "typed_only_when_running"),
            ("status = 'manual', typed = 1", "typed_only_when_running"),
            ("status = 'attention', note = 'why', typed = 2", "typed_only_when_running"),
            ("typed = 3", "typed_only_when_running"),
            ("suggested = 'ok'", "suggested_only_when_waiting"),
            ("status = 'attention', note = 'why', suggested = 'ok'", "suggested_only_when_waiting"),
            ("status = 'manual', prefill = 1, suggested = 'ok'", "suggested_only_when_waiting"),
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
            "typed = 1",
            "typed = 2",
            "typed = 0",
            "status = 'manual', prefill = 1",
            "prefill = 0, suggested = 'ok'",
            "suggested = NULL",
            "status = 'attention', note = 'why', resume = 'remind'",
            "status = 'stopped', briefing = 1, clear = 'done', closing = 1, typed = 2, suggested = 'ok'",
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
        crate::runs::stop(&mut conn, run).unwrap();
        let (status, closing): (String, bool) =
            conn.query_row("SELECT status, closing FROM flow_runs WHERE id = ?1", [run], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((status.as_str(), closing), ("stopped", true));
        // The next run in the tab drops the note the stopped one owed.
        crate::runs::start(&mut conn, flow, &tid("t1")).unwrap();
        let closing: bool = conn.query_row("SELECT closing FROM flow_runs WHERE id = ?1", [run], |r| r.get(0)).unwrap();
        assert!(!closing);
    }
}
