//! raterm's SQLite store (`raterm.db` in the app data directory): the open tabs,
//! app state, and the Claude Code session each Claude tab is currently in.
//!
//! The app and the Claude session hook (`raterm --claude-hook`, one process per
//! event) write concurrently, so the database runs in WAL mode with a busy timeout.
//! Only the app migrates the schema; the hook refuses to touch a database whose
//! schema it does not know exactly.

use crate::claude::SessionId;
use crate::layout::{Layout, Tab, TabId, TabKind};
use crate::settings::Settings;
use rusqlite::{params, Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const FILE: &str = "raterm.db";
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
    -- older raterm versions refuse the database.
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
"#];

/// Why the database could not be opened. Only `Corrupt` justifies replacing the file.
#[derive(Debug, PartialEq)]
pub enum OpenError {
    Corrupt(String),
    /// Written by a newer raterm; this version must not touch it.
    TooNew { found: i64, known: i64 },
    Other(String),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Corrupt(e) => write!(f, "the database is damaged ({e})"),
            OpenError::TooNew { found, known } => write!(
                f,
                "the database was written by a newer raterm (schema {found}, this version knows {known})"
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

fn configure(conn: &Connection, path: &Path) -> Result<(), OpenError> {
    conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| classify("busy timeout", e))?;
    let mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "wal", |r| r.get(0))
        .map_err(|e| classify("journal mode", e))?;
    if mode != "wal" {
        eprintln!("raterm: {} runs in {mode} journal mode, not WAL; concurrent writes may wait", path.display());
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
/// schema is exactly the one this binary knows. Used by the hook, which may be a
/// different raterm version than the running app. Across an upgrade that adds a
/// migration, a new hook therefore refuses the old schema (and logs it) until the
/// app has migrated: session changes in that window are not recorded.
pub fn connect_existing(path: &Path) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    configure(&conn, path).map_err(|e| e.to_string())?;
    let found = schema_version(&conn).map_err(|e| e.to_string())?;
    let known = MIGRATIONS.len() as i64;
    if found != known {
        return Err(format!("schema {found} in {}, this binary knows {known}", path.display()));
    }
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> Result<(), OpenError> {
    let known = MIGRATIONS.len() as i64;
    // Usually current: then no write lock is taken, so startup never waits on the hook.
    if schema_version(conn)? == known {
        return Ok(());
    }
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
        tx.execute_batch(MIGRATIONS[done as usize])
            .map_err(|e| classify(&format!("migration {}", done + 1), e))?;
        tx.pragma_update(None, "user_version", done + 1).map_err(|e| classify("schema version", e))?;
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
}

/// Unix seconds, the one time format raterm stores and compares.
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

    fn open_with_notice(path: PathBuf, recovered_notice: Option<String>) -> Result<Self, OpenError> {
        // The path is handed to the hook in an environment variable.
        if path.to_str().is_none() {
            return Err(OpenError::Other(format!("the path {path:?} is not valid UTF-8")));
        }
        let conn = connect(&path)?;
        Ok(Self::with(Backing::File { path, recovered_notice }, conn))
    }

    fn with(backing: Backing, conn: Connection) -> Self {
        Self { backing, conn: Mutex::new(conn), opened: SystemTime::now() }
    }

    fn in_memory(reason: String) -> Self {
        eprintln!("raterm: {reason}; tabs will not be saved this run");
        let mut conn = Connection::open_in_memory().expect("in-memory SQLite always opens");
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
        eprintln!("raterm: {damage}; moving the database aside and starting fresh");
        let aside = PathBuf::from(format!("{}.broken-{}", path.display(), unix_secs(SystemTime::now())));
        if let Err(e) = move_files(&database_files(&path), &database_files(&aside)) {
            return Self::in_memory(format!("the database is damaged and could not be moved aside ({e})"));
        }
        let notice = format!(
            "Saved tabs could not be read, so raterm started fresh. The old database was kept at {}.",
            aside.display()
        );
        match Self::open_with_notice(path, Some(notice)) {
            Ok(db) => db,
            Err(e) => Self::in_memory(format!("{e}; the damaged database was kept at {}", aside.display())),
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
                        eprintln!("raterm: tab {tab_id}: {e}; not reopening its Claude session");
                        None
                    }),
                },
                "claude" => TabKind::Claude { claude_session: claude_session.map_err(|e| format!("tab {tab_id}: {e}"))? },
                _ => return Err(format!("tab {tab_id} has a kind this raterm does not know ({kind:?})")),
            };
            tabs.push(Tab { tab_id, kind, name, renamed, cwd });
        }
        let saved_active = get_state(&conn, "active_tab")?;
        let active = match saved_active.as_deref().map(str::parse::<usize>) {
            Some(Ok(n)) => n,
            Some(Err(_)) => {
                eprintln!("raterm: ignoring unreadable active tab {saved_active:?}");
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
        self.conn()
            .query_row("SELECT skip_permissions, workspace, disable_auto_memory FROM settings WHERE id = 1", [], |r| {
                Ok(Settings { skip_permissions: r.get(0)?, workspace: r.get(1)?, disable_auto_memory: r.get(2)? })
            })
            .map_err(sql)
    }

    /// Stores the settings (checked by the caller, see `Settings::validated`),
    /// returning them as stored.
    pub fn save_settings(&self, settings: Settings) -> Result<Settings, String> {
        let changed = self
            .conn()
            .execute(
                "UPDATE settings SET skip_permissions = ?1, workspace = ?2, disable_auto_memory = ?3 WHERE id = 1",
                params![settings.skip_permissions, settings.workspace, settings.disable_auto_memory],
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

    /// Drops hook reports for tabs that no longer exist, and forgets the Claude
    /// processes of the last run: they are gone, and a new process could reuse a
    /// pid. Run at startup; the single-instance guard means no other raterm's tabs
    /// can be reporting.
    pub fn prune_claude_sessions(&self) -> Result<(), String> {
        self.conn()
            .execute_batch(
                "DELETE FROM claude_sessions WHERE tab_id NOT IN (SELECT tab_id FROM tabs);
                 UPDATE claude_sessions SET claude_pid = NULL;",
            )
            .map_err(sql)
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
                    eprintln!("raterm: could not move {} back: {back}", dst.display());
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

/// Records the session a tab is now in, replacing any earlier report for it. The
/// tab may not be saved yet (the hook can run before the first save), so this does
/// not require a `tabs` row; startup pruning removes reports for tabs that are gone.
pub fn record_claude_session(conn: &Connection, report: &SessionReport) -> Result<(), String> {
    let now = i64::try_from(unix_secs(SystemTime::now())).unwrap_or(i64::MAX);
    conn.execute(
        "INSERT INTO claude_sessions (tab_id, session_id, source, cwd, transcript_path, updated_at, claude_pid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (tab_id) DO UPDATE SET
            session_id = excluded.session_id, source = excluded.source, cwd = excluded.cwd,
            transcript_path = excluded.transcript_path, updated_at = excluded.updated_at,
            claude_pid = excluded.claude_pid",
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
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
        let upgraded = Settings { skip_permissions: true, workspace: Some("~/src".into()), disable_auto_memory: false };
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
        let all = Settings { skip_permissions: true, workspace: Some("~".into()), disable_auto_memory: true };
        let saved = db.save_settings(all.clone()).unwrap();
        assert_eq!(saved, all, "returned as stored");
        drop(db);
        assert_eq!(Db::open(path).unwrap().load_settings().unwrap(), saved);
    }

    #[test]
    fn the_schema_refuses_settings_values_it_does_not_know() {
        let path = temp_db("settings-check");
        let db = Db::open(path.clone()).unwrap();
        let conn = connect_existing(&path).unwrap();
        assert!(conn.execute("UPDATE settings SET skip_permissions = 2", []).is_err());
        assert!(conn.execute("UPDATE settings SET disable_auto_memory = 2", []).is_err());
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
        // Simulates a newer raterm's kind; CHECK must be bypassed to write it.
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
        db.prune_claude_sessions().unwrap();
        // A new process reusing pid 42 must not look like the old Claude.
        assert_eq!(db.session_run_by(&tid("t"), 42).unwrap(), None);
        assert_eq!(db.reported_session(&tid("t")).unwrap(), Some(sid(S1)), "the report itself is kept");
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
        assert!(reason.contains("newer raterm"));
    }

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

    #[test]
    fn a_non_utf8_path_is_not_used() {
        use std::os::unix::ffi::OsStrExt;
        let dir = crate::test_support::temp_dir("non-utf8");
        let path = dir.join(std::ffi::OsStr::from_bytes(b"raterm-\xff.db"));
        assert!(matches!(Db::open(path.clone()), Err(OpenError::Other(_))));
        let db = Db::open_or_recover(Ok(path.clone()));
        assert_eq!(db.hook_path(), None, "the hook could not be told where it is");
        assert!(!path.exists());
    }

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
}
