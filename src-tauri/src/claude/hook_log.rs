//! The hook's failure log next to the database, and the last failure in it that
//! the app should still show.

use crate::db::{unix_secs, Db};
use crate::layout::TabId;
use serde::Serialize;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

/// Hook failures are appended here, next to the database, since Claude does not
/// show a successful hook's stderr.
pub const HOOK_LOG: &str = "hook.log";
const HOOK_LOG_LIMIT: u64 = 256 * 1024;
/// Messages are cut to this many characters, so every entry fits in the tail
/// `last_hook_failure` reads.
const HOOK_MESSAGE_LIMIT: usize = 1000;
/// Longer than any entry (up to 4 bytes per character, plus the time, a tab id of
/// at most 128 bytes and separators), so the newest entry is always read whole.
const HOOK_LOG_TAIL: u64 = 4 * HOOK_MESSAGE_LIMIT as u64 + 256;
/// Written in place of the tab id when the hook does not know its tab.
const NO_TAB: &str = "-";

/// Appends a failure to the hook log next to the database, starting the log over
/// once it grows past `HOOK_LOG_LIMIT`. Best effort: logging must not fail the hook,
/// so a log that cannot be written is only reported on stderr. Each entry is one
/// line, `<unix time> <tab id or -> <message>`, parsed by `last_hook_failure`.
pub(super) fn log_failure(db_path: Option<&Path>, tab_id: Option<&TabId>, message: &str) {
    use std::io::Write;
    let mut message = message.replace(['\r', '\n'], " | ");
    if let Some((cut, _)) = message.char_indices().nth(HOOK_MESSAGE_LIMIT) {
        message.truncate(cut);
        message.push('…');
    }
    let _ = writeln!(std::io::stderr(), "misag: {message}");
    let Some(log) = db_path.and_then(Path::parent).map(|dir| dir.join(HOOK_LOG)) else { return };
    let file = crate::open_capped_log(&log, HOOK_LOG_LIMIT);
    let tab = tab_id.map_or(NO_TAB, TabId::as_str);
    let written = file.and_then(|mut file| writeln!(file, "{} {tab} {message}", unix_secs(SystemTime::now())));
    if let Err(e) = written {
        let _ = writeln!(std::io::stderr(), "misag: could not write {}: {e}", log.display());
    }
}

/// A failure the hook logged.
#[derive(Debug, PartialEq)]
struct HookFailure {
    at: u64,
    tab_id: Option<TabId>,
    message: String,
}

/// The newest entry in the hook log at `log`; None if there is no log. Only the
/// end of the file is read, and when that cuts into an entry, the partial first
/// line is dropped.
fn last_hook_failure(log: &Path) -> std::io::Result<Option<HookFailure>> {
    let mut file = match std::fs::File::open(log) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let start = file.metadata()?.len().saturating_sub(HOOK_LOG_TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let complete = if start > 0 { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
    Ok(complete.lines().rev().find_map(|line| {
        let (time, rest) = line.split_once(' ')?;
        let (tab, message) = rest.split_once(' ')?;
        let tab_id = match tab {
            NO_TAB => None,
            tab => Some(TabId::parse(tab).ok()?),
        };
        Some(HookFailure { at: time.parse().ok()?, tab_id, message: message.to_owned() })
    }))
}

/// The last failure the session hook logged. Mirrored by `HookProblem` in src/ipcTypes.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HookProblem {
    /// Identifies this failure for the UI, so a dismissed one stays dismissed: when
    /// it was logged (unix seconds), or for an unreadable log when this run started.
    pub at: u64,
    pub message: String,
    /// The log to look at.
    pub log: String,
}

/// The last failure the session hook logged during this run, unless the tab it
/// failed for (or, when it did not know its tab, any tab) has reported a session
/// since, which means the hook works again.
pub fn hook_problem(db: &Db) -> Option<HookProblem> {
    let log = db.hook_path()?.parent()?.join(HOOK_LOG);
    let shown = log.display().to_string();
    let opened = unix_secs(db.opened());
    let failure = match last_hook_failure(&log) {
        Ok(failure) => failure?,
        Err(e) => return Some(HookProblem { at: opened, message: format!("cannot read the log: {e}"), log: shown }),
    };
    if failure.at < opened {
        return None;
    }
    match db.latest_report_at(failure.tab_id.as_ref()) {
        Ok(Some(reported)) if reported > failure.at => return None,
        Ok(_) => {}
        Err(e) => eprintln!("misag: could not check the latest session report: {e}"),
    }
    let message = match &failure.tab_id {
        Some(tab) => format!("tab {tab}: {}", failure.message),
        None => failure.message,
    };
    Some(HookProblem { at: failure.at, message, log: shown })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::test_util::id;
    use crate::db::test_util::{temp_db, tid};
    use crate::db;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    fn log_of(db_path: &Path) -> PathBuf {
        db_path.parent().unwrap().join(HOOK_LOG)
    }

    #[test]
    fn failures_are_logged_next_to_the_database_with_their_tab() {
        let path = temp_db("hook-log");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        log_failure(Some(&path), Some(&tid("tab-1")), "first problem");
        log_failure(Some(&path), None, "second problem");
        let log = fs::read_to_string(log_of(&path)).unwrap();
        let lines: Vec<_> = log.lines().map(|l| l.split_once(' ').unwrap().1).collect();
        assert_eq!(lines, ["tab-1 first problem", "- second problem"]);
        log_failure(None, None, "nowhere to log");
    }

    #[test]
    fn multi_line_messages_are_logged_as_one_entry() {
        let path = temp_db("hook-log-lines");
        let db = Db::open(path.clone()).unwrap();
        log_failure(Some(&path), Some(&tid("t")), "the hook panicked: panicked at x.rs:1:1:\nboom");
        let log = fs::read_to_string(log_of(&path)).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(log.contains("panicked at x.rs:1:1: | boom"), "{log}");
        let problem = hook_problem(&db).unwrap();
        assert_eq!(problem.message, "tab t: the hook panicked: panicked at x.rs:1:1: | boom");
        assert_eq!(problem.log, log_of(&path).display().to_string());
    }

    #[test]
    fn an_oversized_log_starts_over() {
        let path = temp_db("hook-log-big");
        let log = log_of(&path);
        fs::write(&log, "x".repeat(HOOK_LOG_LIMIT as usize + 1)).unwrap();
        log_failure(Some(&path), None, "fresh problem");
        let content = fs::read_to_string(&log).unwrap();
        assert!(content.len() < 100 && content.contains("fresh problem"), "{} bytes", content.len());
    }

    #[test]
    fn a_huge_message_is_cut_so_it_can_still_be_read_back() {
        let path = temp_db("hook-log-huge");
        let db = Db::open(path.clone()).unwrap();
        log_failure(Some(&path), Some(&tid(&"t".repeat(128))), &"ü".repeat(5000));
        let problem = hook_problem(&db).expect("the newest entry is always read whole");
        assert!(problem.message.ends_with('…'));
        assert_eq!(problem.message.chars().filter(|&c| c == 'ü').count(), HOOK_MESSAGE_LIMIT);
    }

    #[test]
    fn reports_failures_logged_during_this_run_only() {
        let path = temp_db("hook-problem");
        fs::write(log_of(&path), "1 - an old failure\n").unwrap();
        let db = Db::open(path.clone()).unwrap();
        assert_eq!(hook_problem(&db), None, "older failures are not this run's");
        let now = unix_secs(SystemTime::now());
        fs::write(log_of(&path), format!("1 - an old failure\n{now} - schema 2 in misag.db, this binary knows 1\n"))
            .unwrap();
        let problem = hook_problem(&db).unwrap();
        assert_eq!((problem.at, problem.message.as_str()), (now, "schema 2 in misag.db, this binary knows 1"));
    }

    #[test]
    fn only_the_failing_tabs_later_report_clears_its_problem() {
        let path = temp_db("hook-recovered");
        let db = Db::open(path.clone()).unwrap();
        let opened = unix_secs(db.opened());
        // Explicit times instead of waiting for the clock: entries and reports `at` seconds after opening.
        let fail = |at: u64, tab: &str| {
            let line = format!("{} {tab} failure\n", opened + at);
            fs::OpenOptions::new().create(true).append(true).open(log_of(&path)).unwrap().write_all(line.as_bytes()).unwrap();
        };
        let conn = db::connect_existing(&path).unwrap();
        let session = id();
        let report = |at: u64, tab: &str| {
            let tab = tid(tab);
            db::record_claude_session(&conn, &db::test_util::report(&tab, &session)).unwrap();
            conn.execute("UPDATE claude_sessions SET updated_at = ?1 WHERE tab_id = ?2", rusqlite::params![(opened + at) as i64, tab.as_str()])
                .unwrap();
        };
        fail(1, "a");
        report(2, "b");
        assert!(hook_problem(&db).is_some(), "another tab working says nothing about tab a");
        report(2, "a");
        assert_eq!(hook_problem(&db), None, "the hook worked for tab a after the failure");

        // A failure that did not know its tab is cleared by any later report.
        fail(3, "-");
        assert!(hook_problem(&db).is_some());
        report(4, "b");
        assert_eq!(hook_problem(&db), None);
    }

    #[test]
    fn reads_the_last_complete_entry_from_a_cut_tail() {
        let path = temp_db("hook-tail");
        let db = Db::open(path.clone()).unwrap();
        let now = unix_secs(SystemTime::now());
        // One entry longer than the tail, cut exactly where its rest looks like an entry.
        let looks_like_one = format!("{now} - cut");
        let rest = format!("{looks_like_one}{}\n", "y".repeat(HOOK_LOG_TAIL as usize - 1 - looks_like_one.len()));
        let content = format!("{now} - {}{rest}", "x".repeat(10));
        fs::write(log_of(&path), &content).unwrap();
        assert_eq!(hook_problem(&db), None, "a line cut by the tail is not read as an entry");

        fs::write(log_of(&path), format!("{content}{now} - the latest failure\nnot an entry\n")).unwrap();
        assert_eq!(hook_problem(&db).unwrap().message, "the latest failure");
    }

    #[test]
    fn an_unreadable_log_is_a_problem_with_a_stable_key() {
        let path = temp_db("hook-log-dir");
        let db = Db::open(path.clone()).unwrap();
        fs::create_dir(log_of(&path)).unwrap();
        let first = hook_problem(&db).unwrap();
        assert!(first.message.contains("cannot read the log"), "{first:?}");
        // Keyed by the run, not the time of the check, so polling again cannot undo a dismissal.
        assert_eq!(first.at, unix_secs(db.opened()));
    }
}
