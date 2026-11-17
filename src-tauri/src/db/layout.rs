//! The saved tabs and which one is active.

use super::{sql, Db};
use crate::claude::SessionId;
use crate::layout::{Layout, Tab, TabId, TabKind};
use rusqlite::{params, Connection, OptionalExtension};

impl Db {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_util::*;
    use crate::db::{connect, connect_existing, record_claude_session};

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
    fn a_saved_bad_tab_id_fails_loading() {
        let path = temp_db("bad-tab-id");
        let db = Db::open(path.clone()).unwrap();
        db.save_layout(Layout { active: 0, tabs: vec![terminal_tab("a")] }).unwrap();
        connect(&path).unwrap().execute("UPDATE tabs SET tab_id = 'a b'", []).unwrap();
        assert!(db.load_layout().unwrap_err().contains("invalid tab id"));
    }
}
