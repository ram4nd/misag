//! The hook itself: reads Claude's payload and records the session, moves the
//! tab's flow run on, or forgets its background tasks.

use super::hook_log::{log_failure, note_working};
use super::{SessionId, DB_ENV, HOOK_FLAG, PID_ENV, TAB_ENV};
use crate::db::{self, SessionReport};
use crate::layout::TabId;
use serde::Deserialize;
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Hook context longer than this reaches Claude only as a preview, so a longer
/// prompt is handed over as a "block" reason instead.
const MAX_CONTEXT: usize = 9_000;
/// Makes the hook panic on purpose, so tests can check that a panic is still
/// logged and exits 0. Only honoured in debug builds; never set by misag itself.
pub const PANIC_TEST_ENV: &str = "MISAG_HOOK_TEST_PANIC";

/// The part of Claude's SessionStart payload misag records.
#[derive(Deserialize)]
struct SessionStart {
    session_id: SessionId,
    source: Option<String>,
    cwd: Option<String>,
    transcript_path: Option<String>,
}

fn record(input: &str, tab_id: &TabId, db_path: &Path, claude_pid: Option<u32>) -> Result<(), String> {
    let event: SessionStart = serde_json::from_str(input).map_err(|e| format!("bad hook payload: {e}"))?;
    let mut conn = db::connect_existing(db_path)?;
    // A run waiting for this clear goes on; a running step's prompt is gone, so
    // Claude will not answer it. A failure is returned only after the session is
    // recorded, so the tab still does not resume the cleared conversation.
    let cleared = match event.source.as_deref() {
        Some("clear") => crate::runs::clear_tab(
            &mut conn,
            tab_id,
            "The conversation was cleared, so Claude no longer has this step's prompt. Pick an output to go on.",
        )
        .map_err(|e| format!("could not move the flow run on after the clear: {e}")),
        _ => Ok(()),
    };
    let recorded = db::record_claude_session(
        &conn,
        &SessionReport {
            tab_id,
            session_id: &event.session_id,
            source: event.source.as_deref(),
            cwd: event.cwd.as_deref(),
            transcript_path: event.transcript_path.as_deref(),
            claude_pid,
        },
    );
    match (recorded, cleared) {
        (Ok(()), cleared) => cleared,
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(c)) => Err(format!("{e}; {c}")),
    }
}

/// Which hook event a payload is.
#[derive(Deserialize)]
struct Event {
    hook_event_name: Option<String>,
}

/// The part of Claude's Stop payload misag reads.
#[derive(Deserialize)]
struct Stop {
    /// Claude Code always sends it; without it the tab's session cannot be matched.
    session_id: Option<String>,
    /// Tasks still in flight, such as background shells and subagents, which wake
    /// Claude when they finish: a list, empty when there are none. Older Claude Code
    /// versions do not report them. Any value is accepted, and only a non-empty
    /// list counts, so a payload shape this version does not expect cannot stop the
    /// flow run from moving on.
    #[serde(default)]
    background_tasks: Option<Value>,
    /// Claude's final reply; older Claude Code versions do not report it, and the
    /// run then waits for the person, saying so.
    last_assistant_message: Option<String>,
}

impl Stop {
    /// Whether Claude ended the turn waiting on background tasks.
    fn waits(&self) -> bool {
        matches!(&self.background_tasks, Some(Value::Array(tasks)) if !tasks.is_empty())
    }
}

/// The part of Claude's UserPromptSubmit payload misag reads.
#[derive(Deserialize)]
struct PromptSubmit {
    prompt: String,
}

/// The part of Claude's SessionEnd payload misag reads.
#[derive(Deserialize)]
struct SessionEnd {
    session_id: String,
}

/// What the hook did.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// Recorded the session a tab is now in.
    Recorded,
    /// Handled a Stop (background tasks recorded, the tab's flow run, if any, moved
    /// on), with the hook output that hands Claude the next prompt, if there is one.
    Stopped {
        output: Option<String>,
        /// Whether it logged a failure along the way (background tasks it could
        /// not record), so the hook does not then say it works.
        degraded: bool,
    },
    /// Recorded that a session ended, so its tab waits on no background tasks.
    Ended,
    /// Handled the person's prompt (see `runs::on_prompt`), with the hook output
    /// that adds a finished flow's note to Claude's context, if one is owed. It
    /// prints nothing else, as a UserPromptSubmit hook's output reaches Claude.
    Prompted(Option<String>),
    /// Did nothing for a prompt or a session end, as the database has another
    /// schema (across an upgrade, the app not yet restarted). Not logged, as that
    /// would log every prompt, nor taken as the hook working.
    Skipped,
}

/// The hook output that has Claude go on with `prompt`. Context rather than a
/// "block" decision, so Claude Code does not show it as a hook error (the same
/// limit on continuations applies); but context past `MAX_CONTEXT` would reach
/// Claude only as a preview, so a longer prompt is a block reason.
fn continuation(prompt: &str) -> String {
    if prompt.chars().count() > MAX_CONTEXT {
        serde_json::json!({ "decision": "block", "reason": prompt }).to_string()
    } else {
        context("Stop", prompt)
    }
}

/// The hook output that adds `text` to Claude's context at hook event `event`.
fn context(event: &str, text: &str) -> String {
    serde_json::json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": text } }).to_string()
}

/// After Claude ended a turn, records whether it waits on background tasks, then
/// moves the tab's flow run on, returning the hook output that hands Claude the
/// next prompt, if there is one. If it cannot (a payload it cannot read, too), a
/// running step waits for the person instead (see `runs::hook_failed`).
fn stopped(input: &str, tab_id: &TabId, db_path: &Path) -> Result<Outcome, String> {
    // Across an upgrade (the app not yet restarted) runs cannot be read. That is
    // logged, as SessionStart's is, since a run in this tab will not move on.
    let mut conn = db::connect_existing(db_path)?;
    let mut degraded = false;
    let moved = serde_json::from_str::<Stop>(input).map_err(|e| format!("bad hook payload: {e}")).and_then(|event| {
        // So the tab keeps showing Claude working while it waits on background tasks.
        // A failure is logged rather than returned, so the flow run still moves on.
        let recorded = match &event.session_id {
            Some(session) => db::record_background(&conn, tab_id, session, event.waits()),
            None => Err("the Stop payload had no session id".into()),
        };
        if let Err(e) = recorded {
            degraded = true;
            log_failure(Some(db_path), Some(tab_id), Some("Stop"), &format!("could not record background tasks: {e}"));
        }
        crate::runs::on_stop(&mut conn, tab_id, event.last_assistant_message.as_deref())
    });
    match moved {
        Ok(prompt) => Ok(Outcome::Stopped { output: prompt.as_deref().map(continuation), degraded }),
        // hook_failed marks the run waiting so it does not show as running forever.
        Err(e) => Err(crate::runs::hook_failed(&mut conn, tab_id, crate::runs::Hook::Stop, e)),
    }
}

/// Records that a session ended, so its tab no longer shows Claude waiting on
/// background tasks, which end with it. Skipped across an upgrade (see
/// `Outcome::Skipped`).
fn ended(input: &str, tab_id: &TabId, db_path: &Path) -> Result<Outcome, String> {
    let event: SessionEnd = serde_json::from_str(input).map_err(|e| format!("bad hook payload: {e}"))?;
    let Some(conn) = db::connect_if_current(db_path)? else { return Ok(Outcome::Skipped) };
    db::record_background(&conn, tab_id, &event.session_id, false).map(|()| Outcome::Ended)
}

/// Tells the tab's flow run, if any, that the person sent a prompt; returns the
/// hook output that adds a finished flow's note to Claude's context, if one is
/// owed. If it cannot, a run waiting at a manual prompt waits for the person
/// instead, as it would never see Claude's answer. Skipped across an upgrade (see
/// `Outcome::Skipped`).
fn prompted(input: &str, tab_id: &TabId, db_path: &Path) -> Result<Outcome, String> {
    let Some(mut conn) = db::connect_if_current(db_path)? else { return Ok(Outcome::Skipped) };
    let told = serde_json::from_str::<PromptSubmit>(input)
        .map_err(|e| format!("bad hook payload: {e}"))
        .and_then(|event| crate::runs::on_prompt(&mut conn, tab_id, &event.prompt));
    match told {
        Ok(note) => Ok(Outcome::Prompted(note.map(|note| context("UserPromptSubmit", &note)))),
        Err(e) => Err(crate::runs::hook_failed(&mut conn, tab_id, crate::runs::Hook::Prompt, e)),
    }
}

/// Reads one hook payload from `input` and handles it (see `handle`).
#[cfg(test)]
fn hook(input: impl Read, tab_id: Option<&TabId>, db_path: Option<&Path>, claude_pid: Option<u32>) -> Result<Outcome, String> {
    handle(input, tab_id, db_path, claude_pid).1
}

/// Reads one hook payload from `input`: a SessionStart is recorded for `tab_id`,
/// with the Claude process that sent it, a Stop records the background tasks and
/// moves the tab's flow run on, a SessionEnd forgets the background tasks, and a
/// UserPromptSubmit tells a run waiting at a manual prompt that the person sent it
/// (and hands Claude a finished flow's note, if one is owed). Also returns the hook
/// event the payload is for, None if the hook did not get as far as reading it.
fn handle(
    mut input: impl Read,
    tab_id: Option<&TabId>,
    db_path: Option<&Path>,
    claude_pid: Option<u32>,
) -> (Option<String>, Result<Outcome, String>) {
    let (Some(tab_id), Some(db_path)) = (tab_id, db_path) else {
        return (None, Err(format!("{HOOK_FLAG} needs a valid {TAB_ENV} and {DB_ENV}")));
    };
    let mut payload = String::new();
    if let Err(e) = input.read_to_string(&mut payload) {
        return (None, Err(format!("could not read the payload: {e}")));
    }
    // A payload without a readable event name goes to the session recorder, which
    // says what is wrong with it.
    let event = serde_json::from_str::<Event>(&payload).ok().and_then(|e| e.hook_event_name);
    let handled = match event.as_deref() {
        Some("Stop") => stopped(&payload, tab_id, db_path).map_err(|e| format!("could not move the flow run on: {e}")),
        Some("UserPromptSubmit") => {
            prompted(&payload, tab_id, db_path).map_err(|e| format!("could not tell the flow run about the prompt: {e}"))
        }
        Some("SessionEnd") => {
            ended(&payload, tab_id, db_path).map_err(|e| format!("could not record the end of the Claude session: {e}"))
        }
        Some("SessionStart") | None => record(&payload, tab_id, db_path, claude_pid)
            .map(|()| Outcome::Recorded)
            .map_err(|e| format!("SessionStart: {e}")),
        Some(other) => Err(format!("misag's hook was run for {other}, which it does not handle")),
    };
    (Some(event.unwrap_or_else(|| "SessionStart".into())), handled)
}

/// Handles one hook payload from `input` (see `handle`) and logs what failed, or
/// that the hook works again (see `note_working`); returns what to print for
/// Claude, if anything.
fn respond(input: impl Read, tab_id: Option<&TabId>, db_path: Option<&Path>, claude_pid: Option<u32>) -> Option<String> {
    let (event, handled) = handle(input, tab_id, db_path, claude_pid);
    let event = event.as_deref();
    match handled {
        Err(message) => log_failure(db_path, tab_id, event, &message),
        // Recorded, but a terminal tab cannot tell whether this session is still open.
        Ok(Outcome::Recorded) if claude_pid.is_none() => log_failure(
            db_path,
            tab_id,
            event,
            &format!("{PID_ENV} is missing or invalid, so a terminal tab will not reopen this Claude session"),
        ),
        Ok(Outcome::Skipped) => {}
        Ok(outcome) => {
            // The hook works again for this tab and event, unless it logged a failure
            // along the way.
            if !matches!(outcome, Outcome::Stopped { degraded: true, .. })
                && let Some(event) = event
            {
                note_working(db_path, tab_id, event);
            }
            if let Outcome::Stopped { output, .. } | Outcome::Prompted(output) = outcome {
                return output;
            }
        }
    }
    None
}

/// Entry point for `misag --claude-hook`. Always exits 0: a failure here must
/// never disturb the Claude session, so it is only logged. Panics are handled by a
/// panic hook that logs and exits 0 before any unwinding, which also works with
/// the release profile's `panic = "abort"` (see Cargo.toml), where `catch_unwind`
/// could not.
pub fn run_hook() -> i32 {
    let db_path: Option<PathBuf> = std::env::var_os(DB_ENV).map(PathBuf::from);
    let tab_id = std::env::var(TAB_ENV).ok().and_then(|id| TabId::parse(&id).ok());
    let (panic_log, panic_tab) = (db_path.clone(), tab_id.clone());
    std::panic::set_hook(Box::new(move |info| {
        // Without its event, so any event of the tab that works again clears it.
        log_failure(panic_log.as_deref(), panic_tab.as_ref(), None, &format!("the hook panicked: {info}"));
        std::process::exit(0);
    }));
    if cfg!(debug_assertions) && std::env::var_os(PANIC_TEST_ENV).is_some() {
        panic!("panic requested through {PANIC_TEST_ENV}");
    }
    let claude_pid = std::env::var(PID_ENV).ok().and_then(|pid| pid.parse().ok());
    if let Some(output) = respond(std::io::stdin(), tab_id.as_ref(), db_path.as_deref(), claude_pid) {
        println!("{output}");
    }
    0
}


#[cfg(test)]
mod tests {
    use super::*;
    use super::super::hook_log;
    use crate::claude::test_util::{id, ID};
    use crate::db::test_util::{temp_db, tid};
    use crate::db::Db;
    use crate::layout::{Layout, Tab, TabKind};

    #[test]
    fn records_the_reported_session_for_the_tab() {
        let path = temp_db("hook-record");
        let db = Db::open(path.clone()).unwrap();
        let earlier = SessionId::parse("1b4e28ba-2fa1-41d2-883f-0016d3cca427").unwrap();
        let tab = Tab {
            tab_id: tid("tab-1"),
            kind: TabKind::Claude { claude_session: Some(earlier) },
            name: "c".into(),
            renamed: false,
            cwd: None,
        };
        db.save_layout(Layout { active: 0, tabs: vec![tab] }).unwrap();

        let payload = format!(
            r#"{{"session_id":"{ID}","transcript_path":"/x/{ID}.jsonl","cwd":"/home/me",
                 "hook_event_name":"SessionStart","source":"clear"}}"#
        );
        record(&payload, &tid("tab-1"), &path, None).unwrap();
        let layout = db.load_layout().unwrap();
        assert_eq!(layout.tabs[0].kind, TabKind::Claude { claude_session: Some(id()) });
    }

    #[test]
    fn a_clear_lets_a_run_waiting_for_it_take_its_first_step() {
        let path = temp_db("hook-clear-run");
        let db = Db::open(path.clone()).unwrap();
        let flow = db.create_flow("F").unwrap().id;
        let mut g = crate::flows::tests::runnable();
        g.nodes[0].clear = true;
        db.save_flow_graph(flow, &g).unwrap();
        let mut conn = db::connect_existing(&path).unwrap();
        let run = crate::runs::start(&mut conn, flow, &tid("tab-1")).unwrap().run_id;
        let payload = format!(r#"{{"session_id":"{ID}","hook_event_name":"SessionStart","source":"clear"}}"#);
        record(&payload, &tid("tab-1"), &path, None).unwrap();
        assert!(matches!(crate::runs::cleared(&mut conn, run).unwrap(), crate::runs::Cleared::Ready { prompt: Some(_) }));
    }

    #[test]
    fn a_clear_that_cannot_move_the_run_on_still_records_the_new_session() {
        let path = temp_db("hook-clear-fails");
        let db = Db::open(path.clone()).unwrap();
        let earlier = SessionId::parse("1b4e28ba-2fa1-41d2-883f-0016d3cca427").unwrap();
        let tab = Tab {
            tab_id: tid("tab-1"),
            kind: TabKind::Claude { claude_session: Some(earlier) },
            name: "c".into(),
            renamed: false,
            cwd: None,
        };
        db.save_layout(Layout { active: 0, tabs: vec![tab] }).unwrap();
        // So runs::clear_tab fails.
        db::connect_existing(&path).unwrap().execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE flow_runs;").unwrap();
        let payload = format!(r#"{{"session_id":"{ID}","hook_event_name":"SessionStart","source":"clear"}}"#);
        let err = hook(payload.as_bytes(), Some(&tid("tab-1")), Some(&path), Some(42)).unwrap_err();
        assert!(err.contains("flow_runs"), "the clear's failure is still reported: {err}");
        // Not resumed after a restart: the cleared conversation is not the tab's any more.
        assert_eq!(db.load_layout().unwrap().tabs[0].kind, TabKind::Claude { claude_session: Some(id()) });
    }

    #[test]
    fn the_person_s_prompt_moves_a_manual_prompt_on_to_claude_s_answer() {
        let path = temp_db("hook-prompt");
        let db = Db::open(path.clone()).unwrap();
        let flow = db.create_flow("F").unwrap().id;
        let mut g = crate::flows::tests::runnable();
        g.nodes.push(crate::flows::tests::draft("mp", &["sent"]));
        g.edges[0].to = "mp".into();
        g.edges.push(crate::flows::tests::edge("mp", "sent", "p"));
        db.save_flow_graph(flow, &g).unwrap();
        let mut conn = db::connect_existing(&path).unwrap();
        crate::runs::start(&mut conn, flow, &tid("tab-1")).unwrap();
        crate::runs::on_stop(&mut conn, &tid("tab-1"), Some("Ready")).unwrap();
        let payload = format!(r#"{{"session_id":"{ID}","hook_event_name":"UserPromptSubmit","prompt":"Fix it"}}"#);
        // Nothing is printed: a UserPromptSubmit hook's output would reach Claude.
        assert_eq!(hook(payload.as_bytes(), Some(&tid("tab-1")), Some(&path), Some(42)).unwrap(), Outcome::Prompted(None));
        assert_eq!(crate::runs::active(&conn).unwrap()[0].status, crate::runs::Status::Running);
    }

    #[test]
    fn a_prompt_the_hook_cannot_read_makes_the_manual_prompt_wait_for_the_person() {
        let path = temp_db("hook-prompt-bad");
        let db = Db::open(path.clone()).unwrap();
        let flow = db.create_flow("F").unwrap().id;
        let mut g = crate::flows::tests::runnable();
        g.nodes.push(crate::flows::tests::draft("mp", &["sent"]));
        g.edges[0].to = "mp".into();
        db.save_flow_graph(flow, &g).unwrap();
        let mut conn = db::connect_existing(&path).unwrap();
        crate::runs::start(&mut conn, flow, &tid("tab-1")).unwrap();
        crate::runs::on_stop(&mut conn, &tid("tab-1"), Some("Ready")).unwrap();
        let payload = format!(r#"{{"session_id":"{ID}","hook_event_name":"UserPromptSubmit"}}"#);
        let err = hook(payload.as_bytes(), Some(&tid("tab-1")), Some(&path), Some(42)).unwrap_err();
        assert!(err.contains("could not tell the flow run about the prompt"), "{err}");
        // Not left at "Waiting for you" while Claude answers a prompt the run never saw.
        let run = &crate::runs::active(&conn).unwrap()[0];
        assert_eq!(run.status, crate::runs::Status::Attention);
        assert!(run.note.as_deref().unwrap().contains("Pick an output"), "{:?}", run.note);
    }

    #[test]
    fn a_stop_the_hook_cannot_read_makes_the_running_step_wait_for_the_person() {
        let path = temp_db("hook-stop-unreadable");
        let db = Db::open(path.clone()).unwrap();
        let flow = db.create_flow("F").unwrap().id;
        db.save_flow_graph(flow, &crate::flows::tests::runnable()).unwrap();
        let mut conn = db::connect_existing(&path).unwrap();
        crate::runs::start(&mut conn, flow, &tid("tab-1")).unwrap();
        crate::runs::on_stop(&mut conn, &tid("tab-1"), Some("Ready")).unwrap();
        let payload = format!(r#"{{"session_id":"{ID}","hook_event_name":"Stop","last_assistant_message":7}}"#);
        let err = hook(payload.as_bytes(), Some(&tid("tab-1")), Some(&path), Some(42)).unwrap_err();
        assert!(err.contains("bad hook payload"), "{err}");
        // Not left running, waiting for a reply the hook could not read.
        let run = &crate::runs::active(&conn).unwrap()[0];
        assert_eq!(run.status, crate::runs::Status::Attention);
        assert!(run.note.as_deref().unwrap().contains("The Stop hook could not move the run on"), "{:?}", run.note);
    }

    #[test]
    fn a_stop_against_another_schema_is_reported_not_dropped() {
        let path = temp_db("hook-stop-schema");
        drop(Db::open(path.clone()).unwrap());
        // As after an upgrade the app has not yet been restarted for.
        let conn = db::connect_existing(&path).unwrap();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        conn.pragma_update(None, "user_version", version + 1).unwrap();
        let payload = format!(r#"{{"session_id":"{ID}","hook_event_name":"Stop","last_assistant_message":"Done"}}"#);
        let err = hook(payload.as_bytes(), Some(&tid("tab-1")), Some(&path), Some(42)).unwrap_err();
        assert!(err.contains("could not move the flow run on") && err.contains("schema"), "{err}");
    }

    /// The lines of the hook log next to the database at `path`.
    fn log_lines(path: &Path) -> Vec<String> {
        let log = std::fs::read_to_string(path.parent().unwrap().join(hook_log::HOOK_LOG)).unwrap_or_default();
        log.lines().map(str::to_owned).collect()
    }

    /// Bumps the schema of the database at `path`, as after an upgrade the app has
    /// not yet been restarted for.
    fn bump_schema(path: &Path) {
        let conn = db::connect_existing(path).unwrap();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        conn.pragma_update(None, "user_version", version + 1).unwrap();
    }

    #[test]
    fn a_stop_that_logged_a_failure_along_the_way_does_not_say_the_hook_works() {
        let path = temp_db("hook-stop-degraded");
        let db = Db::open(path.clone()).unwrap();
        let tab = tid("tab-1");
        let stop = |payload: &str| respond(payload.as_bytes(), Some(&tab), Some(&path), Some(42));
        stop(&format!(r#"{{"session_id":"{ID}","hook_event_name":"Stop","last_assistant_message":7}}"#));
        assert_eq!(log_lines(&path).len(), 1);
        // Moves the run on, but without a session id cannot record background tasks.
        assert_eq!(stop(r#"{"hook_event_name":"Stop","last_assistant_message":"Done"}"#), None);
        let lines = log_lines(&path);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[1].contains("could not record background tasks"), "{lines:?}");
        assert!(crate::claude::hook_problem(&db).is_some());
        stop(&format!(r#"{{"session_id":"{ID}","hook_event_name":"Stop","last_assistant_message":"Done"}}"#));
        assert_eq!(log_lines(&path).len(), 3, "a whole Stop says the hook works");
        assert_eq!(crate::claude::hook_problem(&db), None);
    }

    #[test]
    fn prompts_and_session_ends_against_another_schema_are_skipped_quietly() {
        let path = temp_db("hook-skip-schema");
        drop(Db::open(path.clone()).unwrap());
        let tab = tid("tab-1");
        // Failures they could otherwise claim to have cleared.
        hook_log::log_failure(Some(&path), Some(&tab), Some("UserPromptSubmit"), "earlier");
        hook_log::log_failure(Some(&path), Some(&tab), Some("SessionEnd"), "earlier");
        let before = log_lines(&path);
        bump_schema(&path);
        let prompt = format!(r#"{{"session_id":"{ID}","hook_event_name":"UserPromptSubmit","prompt":"Hi"}}"#);
        assert_eq!(hook(prompt.as_bytes(), Some(&tab), Some(&path), Some(42)), Ok(Outcome::Skipped));
        assert_eq!(respond(prompt.as_bytes(), Some(&tab), Some(&path), Some(42)), None);
        let end = format!(r#"{{"session_id":"{ID}","hook_event_name":"SessionEnd"}}"#);
        assert_eq!(hook(end.as_bytes(), Some(&tab), Some(&path), Some(42)), Ok(Outcome::Skipped));
        respond(end.as_bytes(), Some(&tab), Some(&path), Some(42));
        assert_eq!(log_lines(&path), before);
    }

    #[test]
    fn a_session_start_failure_is_cleared_by_a_good_session_start_not_by_a_stop() {
        let path = temp_db("hook-start-cleared");
        let db = Db::open(path.clone()).unwrap();
        let tab = tid("tab-1");
        let start = format!(r#"{{"session_id":"{ID}","hook_event_name":"SessionStart","source":"startup"}}"#);
        respond(start.as_bytes(), Some(&tab), Some(&path), None);
        assert!(crate::claude::hook_problem(&db).unwrap().message.contains(PID_ENV));
        let stop = format!(r#"{{"session_id":"{ID}","hook_event_name":"Stop","last_assistant_message":"Done"}}"#);
        respond(stop.as_bytes(), Some(&tab), Some(&path), Some(42));
        assert!(crate::claude::hook_problem(&db).is_some(), "the tab will still not reopen that session");
        respond(start.as_bytes(), Some(&tab), Some(&path), Some(42));
        assert_eq!(crate::claude::hook_problem(&db), None);
    }

    #[test]
    fn the_hook_reads_its_payload_from_input() {
        let path = temp_db("hook-input");
        drop(Db::open(path.clone()).unwrap());
        let payload = format!(r#"{{"session_id":"{ID}","source":"startup"}}"#);
        hook(payload.as_bytes(), Some(&tid("tab-2")), Some(&path), Some(42)).unwrap();
        let conn = db::connect_existing(&path).unwrap();
        let stored: (String, u32) = conn
            .query_row("SELECT session_id, claude_pid FROM claude_sessions WHERE tab_id = 'tab-2'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(stored, (ID.to_string(), 42));
    }

    #[test]
    fn the_hook_needs_its_environment() {
        let path = temp_db("hook-env");
        let err = hook("{}".as_bytes(), None, Some(&path), None).unwrap_err();
        assert!(err.contains("MISAG_TAB"), "{err}");
        assert!(hook("{}".as_bytes(), Some(&tid("t")), None, None).is_err());
        assert!(!path.exists(), "nothing should be written without a tab");
    }

    #[test]
    fn ignores_malformed_payloads() {
        let path = temp_db("hook-bad");
        assert!(record("not json", &tid("tab-1"), &path, None).is_err());
        assert!(record(r#"{"session_id":"; rm -rf ~"}"#, &tid("tab-1"), &path, None).is_err());
    }

    #[test]
    fn a_long_prompt_is_handed_over_whole() {
        // Claude Code shows only a preview of hook context over 10,000 characters.
        let short: Value = serde_json::from_str(&continuation("Fix it.")).unwrap();
        assert_eq!(short["hookSpecificOutput"]["additionalContext"], "Fix it.");
        let long_prompt = "x".repeat(9_500);
        let long: Value = serde_json::from_str(&continuation(&long_prompt)).unwrap();
        assert_eq!((long["decision"].as_str(), long["reason"].as_str()), (Some("block"), Some(long_prompt.as_str())));
    }
}
