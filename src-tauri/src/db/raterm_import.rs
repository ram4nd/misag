//! The copy of raterm's database (misag's old name) on misag's first start.

use super::recovery::{database_files, move_files};
use super::{Backing, Db};
use crate::runs;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

impl Db {
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
/// whose Claude still runs (raterm is running, and has it open; told by a live pid
/// whose process did not start after the report, see `started_after`), so its tab starts
/// a new conversation rather than resuming one open in two places, and stops that
/// tab's flow run, which raterm still drives. Returns how many were forgotten.
fn release_open_sessions(conn: &Connection) -> Result<usize, String> {
    let has_pids = conn
        .prepare("SELECT tab_id, claude_pid, updated_at FROM claude_sessions WHERE claude_pid IS NOT NULL")
        .and_then(|mut stmt| {
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?
                .collect::<Result<Vec<_>, _>>()
        });
    let tabs = match has_pids {
        // A process that started well after the report is likely not the Claude that
        // sent it, but one that got its pid after that Claude (and raterm) quit.
        Ok(rows) => rows
            .into_iter()
            .filter(|&(ref tab, pid, at)| {
                let reused = alive(pid) && started_after(pid, at);
                if reused {
                    eprintln!("misag: raterm's tab {tab} reported pid {pid} at {at}, which a later process has now; its conversation is not open");
                }
                alive(pid) && !reused
            })
            .map(|(tab, ..)| tab)
            .collect::<Vec<_>>(),
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
            // database may not have the columns `RunState` saves. Nor does it have
            // `typed`, which only misag added, so no typed prompt's wait is left to drop.
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

/// How long after a report a process may have started and still count as the one
/// that reported: the start is worked out from the wall clock now, so a clock set
/// forward since the report moves it later. A pid reused that soon is rare.
#[cfg(target_os = "linux")]
const START_SLACK_SECS: i64 = 60;

/// Whether process `pid` started more than `START_SLACK_SECS` after `at` (unix
/// seconds); false, logged, when that cannot be told.
#[cfg(target_os = "linux")]
fn started_after(pid: i64, at: i64) -> bool {
    let start = || -> Option<i64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // After the name in parentheses, which may hold spaces, the fields from the
        // 3rd on; the 22nd is the start, in clock ticks since boot.
        let ticks: i64 = stat[stat.rfind(')')? + 1..].split_whitespace().nth(19)?.parse().ok()?;
        let boot: i64 = std::fs::read_to_string("/proc/stat").ok()?.lines().find_map(|l| l.strip_prefix("btime "))?.trim().parse().ok()?;
        // SAFETY: sysconf only reads a configuration value.
        let hz = i64::from(i32::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?);
        (hz > 0).then(|| boot + ticks / hz)
    };
    match start() {
        Some(start) => start > at + START_SLACK_SECS,
        None => {
            eprintln!("misag: could not tell when pid {pid} started, so it counts as the Claude that reported it");
            false
        }
    }
}

/// Whether process `pid` started after `at`: not told here, so a live pid counts
/// as the Claude that reported.
#[cfg(not(target_os = "linux"))]
fn started_after(_pid: i64, _at: i64) -> bool {
    false
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_util::*;
    use crate::db::{connect_existing, record_claude_session, SessionReport, StoreStatus, FILE, RATERM_FILE};
    use crate::layout::{Layout, TabKind};
    use std::fs;

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

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pid_reused_by_a_process_started_after_the_report_does_not_hold_the_conversation() {
        let old = crate::test_support::temp_dir("import-reused-old").join(RATERM_FILE);
        let raterm = Db::open(old.clone()).unwrap();
        raterm.save_layout(Layout { active: 0, tabs: vec![claude_tab("a", Some(S1))] }).unwrap();
        let conn = connect_existing(&old).unwrap();
        let (tab_id, session_id) = (tid("a"), sid(S1));
        record_claude_session(&conn, &SessionReport { claude_pid: Some(std::process::id()), ..report(&tab_id, &session_id) })
            .unwrap();
        // Reported long before this process, which now has the pid, started.
        conn.execute("UPDATE claude_sessions SET updated_at = 1000", []).unwrap();
        let path = crate::test_support::temp_dir("import-reused-new").join(FILE);
        let db = Db::open_or_import(Ok(path), Some(old));
        assert_eq!(db.load_layout().unwrap().tabs[0].kind, TabKind::Claude { claude_session: Some(sid(S1)) });
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
}
