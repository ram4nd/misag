//! What the tests of `db` and of its callers share: names, paths, tabs and reports.

use super::{Db, SessionReport, FILE};
use crate::claude::SessionId;
use crate::layout::{Tab, TabId, TabKind};
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn sid(s: &str) -> SessionId {
    SessionId::parse(s).unwrap()
}

pub(crate) fn tid(s: &str) -> TabId {
    TabId::parse(s).unwrap()
}

pub(super) const S1: &str = "1b4e28ba-2fa1-41d2-883f-0016d3cca427";
pub(super) const S2: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
pub(super) const S3: &str = "6fa459ea-ee8a-4ca4-894e-db77e160355e";

/// Where a test's database goes, in a fresh directory.
pub(crate) fn temp_db(name: &str) -> PathBuf {
    crate::test_support::temp_dir(name).join(FILE)
}

pub(super) fn claude_tab(id: &str, session: Option<&str>) -> Tab {
    Tab {
        tab_id: tid(id),
        kind: TabKind::Claude { claude_session: session.map(sid) },
        name: format!("tab {id}"),
        renamed: false,
        cwd: Some("/tmp".into()),
    }
}

pub(super) fn terminal_tab(id: &str) -> Tab {
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

pub(super) fn files_named(dir: &Path, prefix: &str) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(prefix))
        .collect();
    names.sort();
    names
}

pub(super) fn set_version(path: &Path, version: i64) {
    Connection::open(path).unwrap().pragma_update(None, "user_version", version).unwrap();
}

pub(super) fn flow_names(db: &Db) -> Vec<(String, bool)> {
    db.list_flows().unwrap().into_iter().map(|f| (f.name, f.archived)).collect()
}

pub(super) fn names(list: &[&str]) -> Vec<(String, bool)> {
    list.iter().map(|n| (n.to_string(), false)).collect()
}
