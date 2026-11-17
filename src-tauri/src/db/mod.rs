//! misag's SQLite store (`misag.db` in the app data directory): the open tabs,
//! app state, and the Claude Code session each Claude tab is currently in.
//!
//! The app and the Claude session hook (`misag --claude-hook`, one process per
//! event) write concurrently, so the database runs in WAL mode with a busy timeout.
//! Only the app migrates the schema; the hook refuses to touch a database whose
//! schema it does not know exactly.
//!
//! The submodules each add what one concern needs: `schema` the migrations and
//! connections, `recovery` what happens to a database that cannot be opened,
//! `raterm_import` the copy from raterm's, and `layout`, `settings`, `sessions` and
//! `flows` the `Db` methods over their tables. Flow runs are `runs`'s, reached
//! through `Db::with_conn`.

mod flows;
mod layout;
mod raterm_import;
mod recovery;
mod schema;
mod sessions;
mod settings;
#[cfg(test)]
pub(crate) mod test_util;

pub use flows::FlowSummary;
pub use schema::{connect, connect_existing, connect_if_current, OpenError};
pub use sessions::{record_background, record_claude_session, SessionReport};
pub use settings::load_settings;

use crate::layout::TabId;
use crate::runs;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

pub const FILE: &str = "misag.db";
/// The database of raterm, misag's old name, which misag copies on its first start.
pub const RATERM_FILE: &str = "raterm.db";

fn sql(e: rusqlite::Error) -> String {
    format!("database error: {e}")
}

/// Whether tabs are being saved, for the UI. Mirrored by `StoreStatus` in src/ipcTypes.ts.
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

fn now_secs() -> i64 {
    i64::try_from(unix_secs(SystemTime::now())).unwrap_or(i64::MAX)
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

    /// The connection; a transaction unwound by a (debug-build) panic was rolled
    /// back, so recovering the lock is safe.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        crate::session::lock(&self.conn)
    }

    /// Runs `f` on the connection, holding it for as long as `f` runs; how the
    /// flow runs of `runs` are reached. `f` must not call another `Db` method,
    /// which would wait for the connection forever. It hands out the raw
    /// connection on purpose, so `runs` keeps its own transactions; the rest of the
    /// app goes through `Db`'s methods.
    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T, String>) -> Result<T, String> {
        f(&mut self.conn())
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
}
