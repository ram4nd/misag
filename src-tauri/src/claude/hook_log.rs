//! The hook's log next to the database, of its failures and of when it works again
//! after them, and the newest failure in it that the app should still show.

use crate::db::{unix_secs, Db};
use crate::layout::TabId;
use serde::Serialize;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The hook's log, next to the database, since Claude does not show a successful
/// hook's stderr: the failures the hook logs, and working lines saying which of
/// them it has since worked again for (see `note_working`).
pub const HOOK_LOG: &str = "hook.log";
const HOOK_LOG_LIMIT: u64 = 256 * 1024;
/// Messages are cut to this many characters, so every entry fits in the tail
/// `unresolved_failures` reads.
const HOOK_MESSAGE_LIMIT: usize = 1000;
/// Event names longer than this are logged as unknown (see `event_field`).
const EVENT_LIMIT: usize = 64;
/// Longer than any entry (up to 4 bytes per character, plus the time, a tab id of
/// at most 128 bytes, an event name of at most `EVENT_LIMIT` and separators), so
/// the newest entry is always read whole. Only this tail is read: entries before it
/// count as absent, so an older failure no longer shows, and a working line whose
/// failure is no longer in it clears nothing.
const HOOK_LOG_TAIL: u64 = 4 * HOOK_MESSAGE_LIMIT as u64 + 256;
/// Written in place of the tab id or the event name when the hook does not know it.
const UNKNOWN: &str = "-";
/// Starts a working line, `working <unix time> <tab id> <event> <at>:<fingerprint>`:
/// the hook worked for that tab and event, which clears the failure it names (when
/// it was logged, and `HookFailure::fingerprint`) and the ones logged before it
/// that it would clear (see `HookFailure::cleared_by`). Not a number, so the misag
/// before working lines, which may still be running while a new hook writes them
/// (until it is restarted), skips the line rather than showing it as a failure.
const WORKING: &str = "working";

/// The log next to the database at `db_path`.
fn log_path(db_path: Option<&Path>) -> Option<PathBuf> {
    db_path.and_then(Path::parent).map(|dir| dir.join(HOOK_LOG))
}

/// `event` if it can be written as one field of a log line and read back.
fn event_field(event: Option<&str>) -> Option<&str> {
    event.filter(|e| {
        !e.is_empty()
            && e.len() <= EVENT_LIMIT
            && *e != UNKNOWN
            && !e.chars().any(|c| c.is_control() || c.is_whitespace() || c == '[' || c == ']')
    })
}

/// Logs a failure of the hook for `tab_id` at hook event `event` (each None when
/// the hook did not know it) on stderr and in the hook log next to the database, as
/// `<unix time> <tab id or -> [<event or ->] <message>` (see `append`). It is not
/// appended again while the same failure (tab, event and message) is logged and not
/// cleared since, so a hook that keeps failing the same way keeps the time the app
/// knows the failure by, and a dismissal of it holds.
pub(super) fn log_failure(db_path: Option<&Path>, tab_id: Option<&TabId>, event: Option<&str>, message: &str) {
    let mut message = message.replace(['\r', '\n'], " | ");
    if let Some((cut, _)) = message.char_indices().nth(HOOK_MESSAGE_LIMIT) {
        message.truncate(cut);
        message.push('…');
    }
    let _ = writeln!(std::io::stderr(), "misag: {message}");
    let Some(log) = log_path(db_path) else { return };
    let event = event_field(event);
    // A log that cannot be read is appended to all the same.
    let logged = unresolved_failures(&log).is_ok_and(|failures| {
        failures.iter().any(|f| f.tab_id.as_ref() == tab_id && f.event.as_deref() == event && f.message == message)
    });
    if !logged {
        let tab = tab_id.map_or(UNKNOWN, TabId::as_str);
        let now = unix_secs(SystemTime::now());
        append(&log, &format!("{now} {tab} [{}] {message}", event.unwrap_or(UNKNOWN)));
    }
}

/// After the hook did its work for `tab_id` at hook event `event`, logging no
/// failure, appends a working line if the log holds failures this clears (see
/// `HookFailure::cleared_by`) that nothing cleared since, so the app stops showing
/// them. The line names the newest of them, and clears only up to that one, so a
/// failure another hook appends between this read and the append still shows.
/// Nothing is written when there is nothing to clear, so a working hook leaves the
/// log alone.
pub(super) fn note_working(db_path: Option<&Path>, tab_id: Option<&TabId>, event: &str) {
    let (Some(db_path), Some(tab_id)) = (db_path, tab_id) else { return };
    let Some(log) = log_path(Some(db_path)) else { return };
    match unresolved_failures(&log) {
        Ok(failures) => {
            if let Some(newest) = failures.iter().find(|f| f.cleared_by(tab_id, event)) {
                append_working(db_path, tab_id, event, newest);
            }
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "misag: could not read {}: {e}", log.display());
        }
    }
}

/// Appends the working line for `tab_id` at `event` that clears up to `newest` (see
/// `WORKING`).
fn append_working(db_path: &Path, tab_id: &TabId, event: &str, newest: &HookFailure) {
    let (Some(log), Some(event)) = (log_path(Some(db_path)), event_field(Some(event))) else { return };
    let now = unix_secs(SystemTime::now());
    append(&log, &format!("{WORKING} {now} {tab_id} {event} {}:{:016x}", newest.at, newest.fingerprint()));
}

/// Appends `entry` as a line to the hook log at `log`, starting the log over once
/// it grows past `HOOK_LOG_LIMIT`. Best effort: logging must not fail the hook, so
/// a log that cannot be written is only reported on stderr.
fn append(log: &Path, entry: &str) {
    let file = crate::open_capped_log(log, HOOK_LOG_LIMIT);
    // One write of the whole line, so lines from hooks of other tabs, appending at
    // the same time, cannot interleave with it.
    let written = file.and_then(|mut file| file.write_all(format!("{entry}\n").as_bytes()));
    if let Err(e) = written {
        let _ = writeln!(std::io::stderr(), "misag: could not write {}: {e}", log.display());
    }
}

/// A failure the hook logged.
#[derive(Debug, PartialEq)]
struct HookFailure {
    at: u64,
    tab_id: Option<TabId>,
    /// The hook event it failed at; None if the hook did not know it (it failed
    /// before reading the payload, or panicked), or for a failure logged by a hook
    /// from before failures named their event.
    event: Option<String>,
    message: String,
}

impl HookFailure {
    /// Whether the hook working for `tab_id` at `event` clears this failure: one for
    /// that tab and event; one for that tab that did not know its event (such as a
    /// panic) at any event; and one that did not know its tab (MISAG_TAB missing, so
    /// it did not know its event either) for any tab.
    fn cleared_by(&self, tab_id: &TabId, event: &str) -> bool {
        self.tab_id.as_ref().is_none_or(|t| t == tab_id) && self.event.as_deref().is_none_or(|e| e == event)
    }

    /// Tells this failure from others logged in the same second for working lines
    /// (64-bit FNV-1a of the message, which stays the same across builds). Two
    /// failures alike in all of that are the same failure, so `log_failure` does not
    /// append the second unless a working line cleared the first.
    fn fingerprint(&self) -> u64 {
        self.message.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, b| (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
    }
}

/// An entry of the hook log.
enum Entry {
    Failure(HookFailure),
    /// See `WORKING`.
    Working { tab_id: TabId, event: String, clears: (u64, u64) },
}

/// The entry `line` holds; None if it holds none.
fn parse_entry(line: &str) -> Option<Entry> {
    if let Some(rest) = line.strip_prefix(WORKING).and_then(|rest| rest.strip_prefix(' ')) {
        let fields: Vec<_> = rest.split(' ').collect();
        let [time, tab, event, clears] = fields[..] else { return None };
        time.parse::<u64>().ok()?;
        let (at, fingerprint) = clears.split_once(':')?;
        return Some(Entry::Working {
            tab_id: TabId::parse(tab).ok()?,
            event: event_field(Some(event))?.to_owned(),
            clears: (at.parse().ok()?, u64::from_str_radix(fingerprint, 16).ok()?),
        });
    }
    let (time, rest) = line.split_once(' ')?;
    let (tab, rest) = rest.split_once(' ')?;
    let tab_id = match tab {
        UNKNOWN => None,
        tab => Some(TabId::parse(tab).ok()?),
    };
    // A hook from before failures named their event wrote none.
    let (event, message) = match rest.strip_prefix('[').and_then(|rest| rest.split_once("] ")) {
        Some((UNKNOWN, message)) => (None, message),
        Some((event, message)) if event_field(Some(event)).is_some() => (Some(event.to_owned()), message),
        _ => (None, rest),
    };
    Some(Entry::Failure(HookFailure { at: time.parse().ok()?, tab_id, event, message: message.to_owned() }))
}

/// The failures in the hook log at `log` that no later working line clears, newest
/// first. A working line clears the failure it names and the earlier ones it would
/// clear (see `HookFailure::cleared_by`), not later ones. Empty if there is no log.
/// Only the end of the file is read (see `HOOK_LOG_TAIL`), and when that cuts into
/// an entry, the partial first line is dropped. The order of the entries decides,
/// not their times, which are whole seconds.
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
    // The working lines seen so far (newer than the entry at hand), each with
    // whether the failure it names has been reached, from which on it clears.
    let mut working: Vec<(TabId, String, (u64, u64), bool)> = Vec::new();
    let mut unresolved = Vec::new();
    for entry in complete.lines().rev().filter_map(parse_entry) {
        match entry {
            Entry::Working { tab_id, event, clears } => working.push((tab_id, event, clears, false)),
            Entry::Failure(failure) => {
                let id = (failure.at, failure.fingerprint());
                let mut cleared = false;
                for (tab_id, event, clears, reached) in &mut working {
                    if failure.cleared_by(tab_id, event) {
                        *reached |= *clears == id;
                        cleared |= *reached;
                    }
                }
                if !cleared {
                    unresolved.push(failure);
                }
            }
        }
    }
    Ok(unresolved)
}

/// The newest failure the session hook logged that nothing cleared since (see
/// `note_working`). Mirrored by `HookProblem` in src/ipcTypes.ts.
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

/// The newest failure the session hook logged during this run that no working
/// line clears since (see `note_working`), or that the log cannot be read.
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

    fn lines(db_path: &Path) -> Vec<String> {
        fs::read_to_string(log_of(db_path)).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    #[test]
    fn failures_are_logged_next_to_the_database_with_their_tab_and_event() {
        let path = temp_db("hook-log");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        log_failure(Some(&path), Some(&tid("tab-1")), Some("Stop"), "first problem");
        log_failure(Some(&path), None, None, "second problem");
        // An event name that would not read back as one is logged as unknown.
        log_failure(Some(&path), Some(&tid("tab-1")), Some("Pre Tool]"), "third problem");
        let log = fs::read_to_string(log_of(&path)).unwrap();
        let lines: Vec<_> = log.lines().map(|l| l.split_once(' ').unwrap().1).collect();
        assert_eq!(lines, ["tab-1 [Stop] first problem", "- [-] second problem", "tab-1 [-] third problem"]);
        log_failure(None, None, None, "nowhere to log");
    }

    #[test]
    fn entries_from_concurrent_hooks_do_not_interleave() {
        let path = temp_db("hook-log-concurrent");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // About 56 KB in all, well under HOOK_LOG_LIMIT, so the log is not started over.
        let (threads, entries) = (8, 200);
        std::thread::scope(|s| {
            for t in 0..threads {
                let path = &path;
                s.spawn(move || {
                    let tab = tid(&format!("tab-{t}"));
                    for i in 0..entries {
                        log_failure(Some(path), Some(&tab), Some("Stop"), &format!("failure {t} {i}"));
                    }
                });
            }
        });
        let log = fs::read_to_string(log_of(&path)).unwrap();
        assert_eq!(log.lines().count(), threads * entries);
        for line in log.lines() {
            let parts: Vec<_> = line.split(' ').collect();
            let well_formed = matches!(parts[..], [time, tab, "[Stop]", "failure", t, _]
                if time.parse::<u64>().is_ok() && tab == format!("tab-{t}"));
            assert!(well_formed, "interleaved: {line:?}");
        }
    }

    #[test]
    fn multi_line_messages_are_logged_as_one_entry() {
        let path = temp_db("hook-log-lines");
        let db = Db::open(path.clone()).unwrap();
        log_failure(Some(&path), Some(&tid("t")), None, "the hook panicked: panicked at x.rs:1:1:\nboom");
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
        log_failure(Some(&path), None, None, "fresh problem");
        let content = fs::read_to_string(&log).unwrap();
        assert!(content.len() < 100 && content.contains("fresh problem"), "{} bytes", content.len());
    }

    #[test]
    fn a_huge_message_is_cut_so_it_can_still_be_read_back() {
        let path = temp_db("hook-log-huge");
        let db = Db::open(path.clone()).unwrap();
        let (tab, event, message) = (tid(&"t".repeat(128)), "E".repeat(EVENT_LIMIT), "ü".repeat(5000));
        log_failure(Some(&path), Some(&tab), Some(&event), &message);
        let problem = hook_problem(&db).expect("the newest entry is always read whole");
        assert!(problem.message.ends_with('…'));
        assert_eq!(problem.message.chars().filter(|&c| c == 'ü').count(), HOOK_MESSAGE_LIMIT);
        // So is the failure a working line after it names, which then clears it.
        note_working(Some(&path), Some(&tab), &event);
        assert_eq!(hook_problem(&db), None);
    }

    #[test]
    fn reports_failures_logged_during_this_run_only() {
        let path = temp_db("hook-problem");
        fs::write(log_of(&path), "1 - an old failure\n").unwrap();
        let db = Db::open(path.clone()).unwrap();
        assert_eq!(hook_problem(&db), None, "older failures are not this run's");
        let now = unix_secs(SystemTime::now());
        // Written by a hook from before failures named their event.
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
        log_failure(Some(&path), Some(&tid("a")), Some("Stop"), "a failed");
        note_working(Some(&path), Some(&tid("b")), "Stop");
        assert_eq!(message().as_deref(), Some("tab a: a failed"), "another tab working says nothing about tab a");
        note_working(Some(&path), Some(&tid("a")), "Stop");
        assert_eq!(message(), None, "the hook worked for tab a after the failure");

        // Of two failing tabs, the one that still fails is shown.
        log_failure(Some(&path), Some(&tid("a")), Some("Stop"), "a failed again");
        log_failure(Some(&path), Some(&tid("b")), Some("Stop"), "b failed");
        note_working(Some(&path), Some(&tid("b")), "Stop");
        assert_eq!(message().as_deref(), Some("tab a: a failed again"));
        note_working(Some(&path), Some(&tid("a")), "Stop");
        assert_eq!(message(), None);

        // A failure that did not know its tab is cleared by any tab's working hook.
        log_failure(Some(&path), None, None, "no tab");
        note_working(Some(&path), Some(&tid("c")), "SessionEnd");
        assert_eq!(message(), None);
    }

    #[test]
    fn a_failure_clears_only_when_the_same_event_works_again() {
        let path = temp_db("hook-recovered-event");
        let db = Db::open(path.clone()).unwrap();
        let message = || hook_problem(&db).map(|p| p.message);
        log_failure(Some(&path), Some(&tid("a")), Some("SessionStart"), "PID missing");
        note_working(Some(&path), Some(&tid("a")), "Stop");
        assert_eq!(message().as_deref(), Some("tab a: PID missing"), "a working Stop says nothing about SessionStart");
        note_working(Some(&path), Some(&tid("a")), "SessionStart");
        assert_eq!(message(), None);

        // One that did not know its event (a panic) is cleared by any event of its tab.
        log_failure(Some(&path), Some(&tid("a")), None, "the hook panicked");
        note_working(Some(&path), Some(&tid("b")), "Stop");
        assert!(message().is_some());
        note_working(Some(&path), Some(&tid("a")), "UserPromptSubmit");
        assert_eq!(message(), None);
    }

    #[test]
    fn a_working_hook_writes_nothing_when_there_is_nothing_to_clear() {
        let path = temp_db("hook-working-quiet");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        note_working(Some(&path), Some(&tid("a")), "Stop");
        assert!(!log_of(&path).exists());
        log_failure(Some(&path), Some(&tid("a")), Some("Stop"), "a failed");
        note_working(Some(&path), Some(&tid("a")), "SessionStart");
        assert_eq!(lines(&path).len(), 1, "nothing of SessionStart's to clear");
        note_working(Some(&path), Some(&tid("a")), "Stop");
        let before = lines(&path);
        assert_eq!(before.len(), 2);
        note_working(Some(&path), Some(&tid("a")), "Stop");
        assert_eq!(lines(&path), before, "already cleared");
    }

    #[test]
    fn a_repeated_failure_is_logged_once_until_the_hook_works_again() {
        let path = temp_db("hook-log-repeat");
        let db = Db::open(path.clone()).unwrap();
        let a = tid("a");
        log_failure(Some(&path), Some(&a), Some("Stop"), "A");
        let first = hook_problem(&db).unwrap();
        log_failure(Some(&path), Some(&a), Some("Stop"), "A");
        assert_eq!(lines(&path).len(), 1, "the same failure again");
        // So a dismissal, keyed by its time, holds.
        assert_eq!(hook_problem(&db), Some(first));

        log_failure(Some(&path), Some(&a), Some("Stop"), "B");
        log_failure(Some(&path), Some(&a), Some("Stop"), "A");
        assert_eq!(lines(&path).len(), 2, "A is still unresolved: {:?}", lines(&path));
        // Another tab's is its own.
        log_failure(Some(&path), Some(&tid("b")), Some("Stop"), "A");
        assert_eq!(lines(&path).len(), 3);

        note_working(Some(&path), Some(&a), "Stop");
        assert_eq!(lines(&path).len(), 4);
        log_failure(Some(&path), Some(&a), Some("Stop"), "A");
        assert_eq!(lines(&path).len(), 5, "it came back after the hook worked");
        assert_eq!(hook_problem(&db).unwrap().message, "tab a: A");
    }

    #[test]
    fn a_working_line_clears_only_the_failures_its_hook_saw() {
        let path = temp_db("hook-working-race");
        let db = Db::open(path.clone()).unwrap();
        let a = tid("a");
        log_failure(Some(&path), Some(&a), Some("Stop"), "seen");
        // The working hook reads the log ...
        let seen = unresolved_failures(&log_of(&path)).unwrap().remove(0);
        // ... another hook of the tab logs a failure ...
        log_failure(Some(&path), Some(&a), Some("Stop"), "logged meanwhile");
        // ... and only then is the working line appended.
        append_working(&path, &a, "Stop", &seen);
        let unresolved = unresolved_failures(&log_of(&path)).unwrap();
        assert_eq!(unresolved.iter().map(|f| f.message.as_str()).collect::<Vec<_>>(), ["logged meanwhile"]);
        assert_eq!(hook_problem(&db).unwrap().message, "tab a: logged meanwhile");
    }

    #[test]
    fn the_previous_app_skips_working_lines() {
        // `last_hook_failure`'s parser in the misag before working lines, which may
        // still be running while a new hook writes them (until it is restarted).
        fn previous(line: &str) -> Option<(u64, Option<TabId>, String)> {
            let (time, rest) = line.split_once(' ')?;
            let (tab, message) = rest.split_once(' ')?;
            let tab_id = match tab {
                "-" => None,
                tab => Some(TabId::parse(tab).ok()?),
            };
            Some((time.parse().ok()?, tab_id, message.to_owned()))
        }
        let path = temp_db("hook-working-old-app");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        log_failure(Some(&path), Some(&tid("a")), Some("Stop"), "a failed");
        note_working(Some(&path), Some(&tid("a")), "Stop");
        let lines = lines(&path);
        assert_eq!(lines.len(), 2);
        assert!(previous(&lines[0]).is_some());
        assert_eq!(previous(&lines[1]), None, "{}", lines[1]);
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

        fs::write(log_of(&path), format!("{content}{now} - [Stop] the latest failure\nnot an entry\n")).unwrap();
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
