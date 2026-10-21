//! Runs the real `raterm` binary as Claude's hook, the way Claude invokes it: the
//! hook command through a shell, with the payload on stdin.

use raterm_lib::layout::{Layout, Tab, TabId, TabKind};
use raterm_lib::{claude, db};
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

/// Starts the hook command the way Claude does.
fn start_hook(exe: &Path, env: &[(&str, &str)], payload: &str) -> std::process::Child {
    let mut child = Command::new("sh")
        .args(["-c", &claude::hook_command(exe)])
        // Start clean, so only the variables a case passes are set.
        .env_remove(claude::TAB_ENV)
        .env_remove(claude::DB_ENV)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
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
    let exe = bin_dir.join("raterm");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_raterm"), &exe).unwrap();
    let (_app, db_path) = app_db(&dir);

    let env = [(claude::TAB_ENV, "tab-1"), (claude::DB_ENV, db_path.to_str().unwrap())];
    assert!(run_hook(&exe, &env, &payload(SESSION)).success());
    assert_eq!(stored_session(&db_path, "tab-1").as_deref(), Some(SESSION));
}

#[test]
fn never_fails_claude_and_logs_why() {
    let dir = temp_dir("hook-bad");
    let exe = Path::new(env!("CARGO_BIN_EXE_raterm"));
    let (app, db_path) = app_db(&dir);
    let db_str = db_path.to_str().unwrap();
    assert!(run_hook(exe, &[(claude::TAB_ENV, "t"), (claude::DB_ENV, db_str)], "garbage").success());
    assert!(run_hook(exe, &[(claude::DB_ENV, db_str)], "{}").success());
    assert!(run_hook(exe, &[(claude::TAB_ENV, "not a word"), (claude::DB_ENV, db_str)], "{}").success());
    assert_eq!(stored_session(&db_path, "t"), None);
    let log = std::fs::read_to_string(dir.join(claude::HOOK_LOG)).unwrap();
    let entries: Vec<_> = log.lines().map(|l| l.split_once(' ').unwrap().1).collect();
    assert!(entries[0].starts_with("t could not record") && entries[0].contains("bad hook payload"), "{log}");
    assert!(entries[1].starts_with("- ") && entries[1].contains("RATERM_TAB"), "{log}");
    assert!(entries[2].starts_with("- "), "an invalid tab id is not logged as one: {log}");
    assert!(claude::hook_problem(&app).is_some(), "the app sees the failures");
}

// Debug builds only: release builds ignore the panic switch.
#[cfg(debug_assertions)]
#[test]
fn a_panic_is_logged_and_still_exits_zero() {
    let dir = temp_dir("hook-panic");
    let exe = Path::new(env!("CARGO_BIN_EXE_raterm"));
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
    let exe = Path::new(env!("CARGO_BIN_EXE_raterm"));
    let db_path = dir.join(db::FILE);
    let env = [(claude::TAB_ENV, "t"), (claude::DB_ENV, db_path.to_str().unwrap())];
    assert!(run_hook(exe, &env, &payload(SESSION)).success());
    assert!(!db_path.exists());
}

#[test]
fn hooks_and_layout_saves_can_run_at_the_same_time() {
    let dir = temp_dir("hook-concurrent");
    let exe = Path::new(env!("CARGO_BIN_EXE_raterm"));
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
