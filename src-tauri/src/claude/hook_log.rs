//! The hook's failure log next to the database, and the last failure in it that
//! the app should still show.

use crate::db::{unix_secs, Db};
use crate::layout::TabId;
use serde::Serialize;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

/// Hook failures are appended here, next to the database, since Claude does not
/// show a successful hook's stderr.
pub const HOOK_LOG: &str = "hook.log";
const HOOK_LOG_LIMIT: u64 = 256 * 1024;
/// Messages are cut to this many characters, so every entry fits in the tail
/// `unresolved_failures` reads.
const HOOK_MESSAGE_LIMIT: usize = 1000;
/// Longer than any entry (up to 4 bytes per character, plus the time, a tab id of
/// at most 128 bytes and separators), so the newest entry is always read whole.
const HOOK_LOG_TAIL: u64 = 4 * HOOK_MESSAGE_LIMIT as u64 + 256;
/// Written in place of the tab id when the hook does not know its tab.
const NO_TAB: &str = "-";
/// The message of an entry that says the hook worked again for its tab, after
/// failures logged before it (see `note_working`).
const WORKING: &str = "(the hook works again)";

/// Whether this process logged a failure (see `failed`).
static FAILED: AtomicBool = AtomicBool::new(false);

/// Whether this process logged a failure, so the hook does not then say it works.
pub(super) fn failed() -> bool {
    FAILED.load(Ordering::Relaxed)
}

/// Appends a failure to the hook log next to the database (see `append`).
pub(super) fn log_failure(db_path: Option<&Path>, tab_id: Option<&TabId>, message: &str) {
    FAILED.store(true, Ordering::Relaxed);
    let mut message = message.replace(['\r', '\n'], " | ");
    if let Some((cut, _)) = message.char_indices().nth(HOOK_MESSAGE_LIMIT) {
        message.truncate(cut);
        message.push('…');
    }
    let _ = writeln!(std::io::stderr(), "misag: {message}");
    append(db_path, tab_id, &message);
}

/// After the hook did its work for `tab_id`, logging no failure, says so in the
/// log if failures for that tab (or for no tab) are logged without that since, so
/// the app stops showing them. Nothing is written otherwise, so a working hook
/// leaves the log alone.
pub(super) fn note_working(db_path: Option<&Path>, tab_id: Option<&TabId>) {
    let (Some(db_path), Some(tab_id)) = (db_path, tab_id) else { return };
    let Some(log) = db_path.parent().map(|dir| dir.join(HOOK_LOG)) else { return };
    match unresolved_failures(&log) {
        Ok(failures) if failures.iter().any(|f| f.tab_id.as_ref().is_none_or(|t| t == tab_id)) => {
            append(Some(db_path), Some(tab_id), WORKING);
        }
        Ok(_) => {}
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "misag: could not read {}: {e}", log.display());
        }
    }
}

/// Appends an entry to the hook log next to the database, starting the log over
/// once it grows past `HOOK_LOG_LIMIT`. Best effort: logging must not fail the hook,
/// so a log that cannot be written is only reported on stderr. Each entry is one
/// line, `<unix time> <tab id or -> <message>`, parsed by `unresolved_failures`.
fn append(db_path: Option<&Path>, tab_id: Option<&TabId>, message: &str) {
    let Some(log) = db_path.and_then(Path::parent).map(|dir| dir.join(HOOK_LOG)) else { return };
    let file = crate::open_capped_log(&log, HOOK_LOG_LIMIT);
    let tab = tab_id.map_or(NO_TAB, TabId::as_str);
    // One write of the whole line, so lines from hooks of other tabs, appending at
    // the same time, cannot interleave with it.
    let line = format!("{} {tab} {message}\n", unix_secs(SystemTime::now()));
    let written = file.and_then(|mut file| file.write_all(line.as_bytes()));
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

/// The failures in the hook log at `log` that no later entry says the hook works
/// again for, newest first: one for a tab is resolved by a later `WORKING` entry
/// for that tab, one for no tab by any. Empty if there is no log. Only the end of
/// the file is read, and when that cuts into an entry, the partial first line is
/// dropped. The order of the entries decides, not their times, which are whole
/// seconds.
fn unresolved_failures(log: &Path) -> std::io::Result<Vec<HookFailure>> {
    let mut file = match std::fs::File::open(log) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let start = file.metadata()?.len().saturating_sub(HOOK_LOG_TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let complete = if start > 0 { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
    let entries = complete.lines().rev().filter_map(|line| {
        let (time, rest) = line.split_once(' ')?;
        let (tab, message) = rest.split_once(' ')?;
        let tab_id = match tab {
            NO_TAB => None,
            tab => Some(TabId::parse(tab).ok()?),
        };
        Some(HookFailure { at: time.parse().ok()?, tab_id, message: message.to_owned() })
    });
    let (mut working, mut unresolved) = (Vec::new(), Vec::new());
    for entry in entries {
        if entry.message == WORKING {
            working.extend(entry.tab_id);
        } else if entry.tab_id.as_ref().map_or(working.is_empty(), |tab| !working.contains(tab)) {
            unresolved.push(entry);
        }
    }
    Ok(unresolved)
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

/// The newest failure the session hook logged during this run that the hook has
/// not since worked for its tab (or, when it did not know its tab, for any tab)
/// after (see `note_working`).
pub fn hook_problem(db: &Db) -> Option<HookProblem> {
    let log = db.hook_path()?.parent()?.join(HOOK_LOG);
    let shown = log.display().to_string();
    let opened = unix_secs(db.opened());
    let failure = match unresolved_failures(&log) {
        Ok(failures) => failures.into_iter().next()?,
        Err(e) => return Some(HookProblem { at: opened, message: format!("cannot read the log: {e}"), log: shown }),
    };
    if failure.at < opened {
        return None;
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
    use crate::db::test_util::{temp_db, tid};
    use std::fs;
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
    fn entries_from_concurrent_hooks_do_not_interleave() {
        let path = temp_db("hook-log-concurrent");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // About 48 KB in all, well under HOOK_LOG_LIMIT, so the log is not started over.
        let (threads, entries) = (8, 200);
        std::thread::scope(|s| {
            for t in 0..threads {
                let path = &path;
                s.spawn(move || {
                    let tab = tid(&format!("tab-{t}"));
                    for i in 0..entries {
                        log_failure(Some(path), Some(&tab), &format!("failure {t} {i}"));
                    }
                });
            }
        });
        let log = fs::read_to_string(log_of(&path)).unwrap();
        assert_eq!(log.lines().count(), threads * entries);
        for line in log.lines() {
            let parts: Vec<_> = line.split(' ').collect();
            let well_formed = matches!(parts[..], [time, tab, "failure", t, _]
                if time.parse::<u64>().is_ok() && tab == format!("tab-{t}"));
            assert!(well_formed, "interleaved: {line:?}");
        }
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
    fn a_tab_s_later_working_hook_clears_its_problem_within_the_same_second() {
        let path = temp_db("hook-recovered");
        let db = Db::open(path.clone()).unwrap();
        let message = || hook_problem(&db).map(|p| p.message);
        // All within a second or so: the order of the entries decides, not their times.
        log_failure(Some(&path), Some(&tid("a")), "a failed");
        note_working(Some(&path), Some(&tid("b")));
        assert_eq!(message().as_deref(), Some("tab a: a failed"), "another tab working says nothing about tab a");
        note_working(Some(&path), Some(&tid("a")));
        assert_eq!(message(), None, "the hook worked for tab a after the failure, whichever event it ran for");

        // Of two failing tabs, the one that still fails is shown.
        log_failure(Some(&path), Some(&tid("a")), "a failed again");
        log_failure(Some(&path), Some(&tid("b")), "b failed");
        note_working(Some(&path), Some(&tid("b")));
        assert_eq!(message().as_deref(), Some("tab a: a failed again"));
        note_working(Some(&path), Some(&tid("a")));
        assert_eq!(message(), None);

        // A failure that did not know its tab is cleared by any tab's working hook.
        log_failure(Some(&path), None, "no tab");
        note_working(Some(&path), Some(&tid("c")));
        assert_eq!(message(), None);
    }

    #[test]
    fn a_working_hook_writes_nothing_when_there_is_nothing_to_clear() {
        let path = temp_db("hook-working-quiet");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        note_working(Some(&path), Some(&tid("a")));
        assert!(!log_of(&path).exists());
        log_failure(Some(&path), Some(&tid("a")), "a failed");
        note_working(Some(&path), Some(&tid("a")));
        let before = fs::read_to_string(log_of(&path)).unwrap();
        note_working(Some(&path), Some(&tid("a")));
        assert_eq!(fs::read_to_string(log_of(&path)).unwrap(), before, "already cleared");
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
