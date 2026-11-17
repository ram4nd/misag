//! The Claude session each tab is in, as the hook reports it, and the sessions
//! pinned in Session history.

use super::{now_secs, sql, unix_secs, Db};
use crate::claude::SessionId;
use crate::layout::TabId;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

impl Db {
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
mod tests {
    use super::*;
    use crate::db::test_util::*;
    use crate::db::connect_existing;
    use crate::layout::Layout;

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
}
