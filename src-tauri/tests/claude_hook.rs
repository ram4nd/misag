#![cfg(unix)]
//! Runs the real `misag` binary as Claude's hook, the way Claude invokes it: the
//! hook command through a shell, with the payload on stdin.

use misag_lib::layout::{Layout, Tab, TabId, TabKind};
use misag_lib::{claude, db};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod common;
use common::temp_dir;

const SESSION: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

/// A database the app has already created, as it always has before any hook runs.
fn app_db(dir: &Path) -> (db::Db, PathBuf) {
    let path = dir.join(db::FILE);
    (db::Db::open(path.clone()).unwrap(), path)
}

fn payload(session: &str) -> String {
    format!(r#"{{"session_id":"{session}","source":"clear","hook_event_name":"SessionStart"}}"#)
}

/// Starts the hook command the way Claude does. An empty value in `env` removes
/// that variable instead.
fn start_hook(exe: &Path, env: &[(&str, &str)], payload: &str) -> std::process::Child {
    let mut command = Command::new("sh");
    command
        .args(["-c", &claude::hook_command(exe)])
        // Start clean, so only the variables a case passes are set.
        .env_remove(claude::TAB_ENV)
        .env_remove(claude::DB_ENV)
        // Claude Code always sets it for hooks.
        .env(claude::PID_ENV, "4242");
    for &(key, value) in env {
        if value.is_empty() {
            command.env_remove(key);
        } else {
            command.env(key, value);
        }
    }
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
    child
}

fn run_hook(exe: &Path, env: &[(&str, &str)], payload: &str) -> std::process::ExitStatus {
    start_hook(exe, env, payload).wait().unwrap()
}

fn stored_session(db_path: &Path, tab: &str) -> Option<String> {
    db::connect_existing(db_path)
        .unwrap()
        .query_row("SELECT session_id FROM claude_sessions WHERE tab_id = ?1", [tab], |r| r.get(0))
        .ok()
}

#[test]
fn records_the_session_through_a_path_with_spaces() {
    let dir = temp_dir("hook");
    // Claude runs the hook through a shell, so the binary path must survive quoting.
    let bin_dir = dir.join("my apps");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let exe = bin_dir.join("misag");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_misag"), &exe).unwrap();
    let (_app, db_path) = app_db(&dir);

    let env = [(claude::TAB_ENV, "tab-1"), (claude::DB_ENV, db_path.to_str().unwrap())];
    assert!(run_hook(&exe, &env, &payload(SESSION)).success());
    assert_eq!(stored_session(&db_path, "tab-1").as_deref(), Some(SESSION));
}

#[test]
fn records_the_claude_process_with_the_session() {
    let dir = temp_dir("hook-pid");
    let exe = Path::new(env!("CARGO_BIN_EXE_misag"));
    let (_app, db_path) = app_db(&dir);
    let env = [(claude::TAB_ENV, "t"), (claude::DB_ENV, db_path.to_str().unwrap()), (claude::PID_ENV, "4242")];
    assert!(run_hook(exe, &env, &payload(SESSION)).success());
    let pid: Option<u32> = db::connect_existing(&db_path)
        .unwrap()
        .query_row("SELECT claude_pid FROM claude_sessions WHERE tab_id = 't'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(pid, Some(4242));
}

#[test]
fn a_missing_claude_pid_is_recorded_and_logged() {
    let dir = temp_dir("hook-no-pid");
    let exe = Path::new(env!("CARGO_BIN_EXE_misag"));
    let (_app, db_path) = app_db(&dir);
    let env = [(claude::TAB_ENV, "t"), (claude::DB_ENV, db_path.to_str().unwrap()), (claude::PID_ENV, "")];
    assert!(run_hook(exe, &env, &payload(SESSION)).success());
    assert_eq!(stored_session(&db_path, "t").as_deref(), Some(SESSION));
    let log = std::fs::read_to_string(dir.join(claude::HOOK_LOG)).unwrap();
    assert!(log.contains("CLAUDE_PID is missing"), "{log}");
}

#[test]
fn never_fails_claude_and_logs_why() {
    let dir = temp_dir("hook-bad");
    let exe = Path::new(env!("CARGO_BIN_EXE_misag"));
    let (app, db_path) = app_db(&dir);
    let db_str = db_path.to_str().unwrap();
    assert!(run_hook(exe, &[(claude::TAB_ENV, "t"), (claude::DB_ENV, db_str)], "garbage").success());
    assert!(run_hook(exe, &[(claude::DB_ENV, db_str)], "{}").success());
    assert!(run_hook(exe, &[(claude::TAB_ENV, "not a word"), (claude::DB_ENV, db_str)], "{}").success());
    assert_eq!(stored_session(&db_path, "t"), None);
    let log = std::fs::read_to_string(dir.join(claude::HOOK_LOG)).unwrap();
    let entries: Vec<_> = log.lines().map(|l| l.split_once(' ').unwrap().1).collect();
    assert!(entries[0].starts_with("t could not record") && entries[0].contains("bad hook payload"), "{log}");
    assert!(entries[1].starts_with("- ") && entries[1].contains("MISAG_TAB"), "{log}");
    assert!(entries[2].starts_with("- "), "an invalid tab id is not logged as one: {log}");
    assert!(claude::hook_problem(&app).is_some(), "the app sees the failures");
}

// Debug builds only: release builds ignore the panic switch.
#[cfg(debug_assertions)]
#[test]
fn a_panic_is_logged_and_still_exits_zero() {
    let dir = temp_dir("hook-panic");
    let exe = Path::new(env!("CARGO_BIN_EXE_misag"));
    let (app, db_path) = app_db(&dir);
    let env = [
        (claude::TAB_ENV, "t"),
        (claude::DB_ENV, db_path.to_str().unwrap()),
        (claude::PANIC_TEST_ENV, "1"),
    ];
    assert!(run_hook(exe, &env, &payload(SESSION)).success());
    let log = std::fs::read_to_string(dir.join(claude::HOOK_LOG)).unwrap();
    assert!(log.contains("the hook panicked"), "{log}");
    assert_eq!(log.lines().count(), 1, "a panic is one log entry: {log}");
    let problem = claude::hook_problem(&app).expect("the app sees the panic");
    assert!(problem.message.contains("the hook panicked"), "{problem:?}");
}

#[test]
fn does_not_create_a_database_the_app_never_made() {
    let dir = temp_dir("hook-nodb");
    let exe = Path::new(env!("CARGO_BIN_EXE_misag"));
    let db_path = dir.join(db::FILE);
    let env = [(claude::TAB_ENV, "t"), (claude::DB_ENV, db_path.to_str().unwrap())];
    assert!(run_hook(exe, &env, &payload(SESSION)).success());
    assert!(!db_path.exists());
}

#[test]
fn hooks_and_layout_saves_can_run_at_the_same_time() {
    let dir = temp_dir("hook-concurrent");
    let exe = Path::new(env!("CARGO_BIN_EXE_misag"));
    let (db, db_path) = app_db(&dir);
    let tabs: Vec<Tab> = (0..8)
        .map(|i| Tab {
            tab_id: TabId::parse(&format!("tab-{i}")).unwrap(),
            kind: TabKind::Claude { claude_session: None },
            name: format!("claude {i}"),
            renamed: false,
            cwd: None,
        })
        .collect();
    let layout = Layout { active: 0, tabs };

    let db_str = db_path.to_str().unwrap().to_string();
    let hooks: Vec<_> = (0..8)
        .map(|i| {
            let tab = format!("tab-{i}");
            let env = [(claude::TAB_ENV, tab.as_str()), (claude::DB_ENV, db_str.as_str())];
            start_hook(exe, &env, &payload(SESSION))
        })
        .collect();
    for _ in 0..50 {
        db.save_layout(layout.clone()).unwrap();
        assert_eq!(db.load_layout().unwrap().tabs.len(), 8);
    }
    for mut hook in hooks {
        assert!(hook.wait().unwrap().success());
    }
    for i in 0..8 {
        assert_eq!(stored_session(&db_path, &format!("tab-{i}")).as_deref(), Some(SESSION));
    }
    assert!(!dir.join(claude::HOOK_LOG).exists(), "no hook should have failed");
}

/// Runs the hook for a Stop event and returns what it printed for Claude.
fn run_stop_hook(env: &[(&str, &str)], reply: &str) -> String {
    let payload = serde_json::json!({
        "session_id": SESSION, "hook_event_name": "Stop", "stop_hook_active": false, "last_assistant_message": reply,
    });
    let mut child = start_hook(Path::new(env!("CARGO_BIN_EXE_misag")), env, &payload.to_string());
    let mut out = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
    assert!(child.wait().unwrap().success());
    out
}

#[test]
fn a_stop_moves_the_tabs_flow_run_on_and_sends_the_next_prompt() {
    use misag_lib::{flows, runs};
    let dir = temp_dir("hook-stop");
    let (app, db_path) = app_db(&dir);
    let graph: flows::Graph = serde_json::from_value(serde_json::json!({ "version": 1, "nodes": [
        { "id": "start", "kind": "start", "title": "Start", "outputs": ["next"], "x": 0, "y": 0 },
        { "id": "p", "kind": "prompt", "title": "Fix", "text": "Fix the tests.", "outputs": ["pass", "fail"], "x": 0, "y": 0 }
    ], "edges": [{ "from": "start", "output": "next", "to": "p" }, { "from": "p", "output": "fail", "to": "p" }] }))
    .unwrap();
    let flow = app.create_flow("F").unwrap().id;
    app.save_flow_graph(flow, &graph).unwrap();
    let mut conn = db::connect_existing(&db_path).unwrap();
    runs::start(&mut conn, flow, &TabId::parse("t1").unwrap()).unwrap();
    let env = [(claude::TAB_ENV, "t1"), (claude::DB_ENV, db_path.to_str().unwrap())];

    // Claude's reply to the run's briefing: the hook hands it the first step.
    let out: serde_json::Value = serde_json::from_str(&run_stop_hook(&env, "Ready")).unwrap();
    assert_eq!(out["hookSpecificOutput"]["additionalContext"], flows::prompt_text(&graph.nodes[1]));
    // fail loops back: the hook hands Claude the prompt again, as hook context.
    let out: serde_json::Value = serde_json::from_str(&run_stop_hook(&env, "Still broken.\nFLOW: fail")).unwrap();
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "Stop");
    assert_eq!(out["hookSpecificOutput"]["additionalContext"], flows::prompt_text(&graph.nodes[1]));
    // pass has no next step: the run ends, telling Claude the flow is finished.
    let out: serde_json::Value = serde_json::from_str(&run_stop_hook(&env, "Fixed.\nFLOW: pass")).unwrap();
    assert_eq!(out["hookSpecificOutput"]["additionalContext"], flows::closing_text("F"));
    assert!(runs::active(&conn).unwrap().is_empty());
    // A tab without a run, and a Stop that records no session, are left alone.
    assert_eq!(run_stop_hook(&[(claude::TAB_ENV, "t2"), (claude::DB_ENV, db_path.to_str().unwrap())], "FLOW: pass"), "");
    assert_eq!(stored_session(&db_path, "t1"), None, "a Stop is not a session start");
}

#[test]
fn stop_payloads_it_cannot_use_are_logged_without_output() {
    let dir = temp_dir("hook-stop-bad");
    let (_app, db_path) = app_db(&dir);
    let env = [(claude::TAB_ENV, "t1"), (claude::DB_ENV, db_path.to_str().unwrap())];
    let mut child = start_hook(Path::new(env!("CARGO_BIN_EXE_misag")), &env, r#"{"hook_event_name":"Stop","last_assistant_message":7}"#);
    let mut out = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(out, "");
    assert!(run_hook(Path::new(env!("CARGO_BIN_EXE_misag")), &env, r#"{"hook_event_name":"Notification"}"#).success());
    let log = std::fs::read_to_string(dir.join(claude::HOOK_LOG)).unwrap();
    assert!(log.contains("could not move the flow run on"), "{log}");
    assert!(log.contains("Notification"), "{log}");
}

/// The tabs the app shows waiting on background tasks.
fn waiting(app: &db::Db) -> Vec<String> {
    let mut tabs: Vec<String> = app.background_tabs().unwrap().into_keys().collect();
    tabs.sort();
    tabs
}

#[test]
fn a_stop_records_whether_claude_waits_on_background_tasks() {
    let dir = temp_dir("hook-background");
    let (app, db_path) = app_db(&dir);
    let env = [(claude::TAB_ENV, "t1"), (claude::DB_ENV, db_path.to_str().unwrap())];
    let exe = Path::new(env!("CARGO_BIN_EXE_misag"));
    let stop = |tasks: serde_json::Value| {
        let payload = serde_json::json!({
            "session_id": SESSION, "hook_event_name": "Stop", "last_assistant_message": "Waiting.", "background_tasks": tasks,
        });
        assert!(run_hook(exe, &env, &payload.to_string()).success());
    };
    let shell = serde_json::json!([{ "id": "b1", "type": "shell", "status": "running", "command": "sleep 60" }]);

    // Without a session start there is no Claude to mark.
    stop(shell.clone());
    assert!(waiting(&app).is_empty());

    assert!(run_hook(exe, &env, &payload(SESSION)).success());
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    stop(shell.clone());
    let since = app.background_tabs().unwrap()["t1"];
    assert!(since >= before, "marked with when the turn ended");
    stop(serde_json::json!([]));
    assert!(waiting(&app).is_empty());

    // A new session starts with nothing in flight.
    stop(shell.clone());
    assert!(run_hook(exe, &env, &payload(SESSION)).success());
    assert!(waiting(&app).is_empty());

    // Nor does a Claude that ended its session leave anything waiting.
    stop(shell);
    assert_eq!(waiting(&app), ["t1"]);
    let end = serde_json::json!({ "session_id": SESSION, "hook_event_name": "SessionEnd", "reason": "prompt_input_exit" });
    assert!(run_hook(exe, &env, &end.to_string()).success());
    assert!(waiting(&app).is_empty());
    assert!(!dir.join(claude::HOOK_LOG).exists(), "nothing went wrong");
}

#[test]
fn a_stop_whose_background_tasks_cannot_be_read_still_moves_the_run_on() {
    use misag_lib::{flows, runs};
    let dir = temp_dir("hook-background-odd");
    let (app, db_path) = app_db(&dir);
    let graph: flows::Graph = serde_json::from_value(serde_json::json!({ "version": 1, "nodes": [
        { "id": "start", "kind": "start", "title": "Start", "outputs": ["next"], "x": 0, "y": 0 },
        { "id": "p", "kind": "prompt", "title": "Fix", "text": "Fix the tests.", "outputs": ["pass"], "x": 0, "y": 0 }
    ], "edges": [{ "from": "start", "output": "next", "to": "p" }] }))
    .unwrap();
    let flow = app.create_flow("F").unwrap().id;
    app.save_flow_graph(flow, &graph).unwrap();
    let conn = db::connect_existing(&db_path).unwrap();
    let env = [(claude::TAB_ENV, "t1"), (claude::DB_ENV, db_path.to_str().unwrap())];
    let exe = Path::new(env!("CARGO_BIN_EXE_misag"));
    assert!(run_hook(exe, &env, &payload(SESSION)).success());

    for odd in [serde_json::Value::Null, serde_json::json!({}), serde_json::json!("none")] {
        let mut started = db::connect_existing(&db_path).unwrap();
        runs::start(&mut started, flow, &TabId::parse("t1").unwrap()).unwrap();
        // Claude's reply to the briefing takes the first step.
        runs::on_stop(&mut started, &TabId::parse("t1").unwrap(), Some("Ready")).unwrap();
        let stop = serde_json::json!({
            "session_id": SESSION, "hook_event_name": "Stop", "last_assistant_message": "Fixed.\nFLOW: pass", "background_tasks": odd,
        });
        assert!(run_hook(exe, &env, &stop.to_string()).success());
        assert!(runs::active(&conn).unwrap().is_empty(), "{odd}: the run ended");
        assert!(waiting(&app).is_empty(), "{odd}: nothing waits");
    }
    assert!(!dir.join(claude::HOOK_LOG).exists(), "nothing went wrong");

    // A Stop without a session id cannot be matched to the tab's session, and says so.
    let anonymous = serde_json::json!({ "hook_event_name": "Stop", "last_assistant_message": "Done.", "background_tasks": [] });
    assert!(run_hook(exe, &env, &anonymous.to_string()).success());
    let log = std::fs::read_to_string(dir.join(claude::HOOK_LOG)).unwrap();
    assert!(log.contains("no session id"), "{log}");
}
