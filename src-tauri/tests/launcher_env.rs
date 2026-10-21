//! A tab started from a raterm that Claude Code launched (as with a dev build run
//! from Claude Code) must not inherit that session, but must keep the user's own
//! settings. Its own test binary, because it changes this process's environment.

use raterm_lib::session::{Launch, Sessions, SpawnRequest, CLAUDE_CODE_SHELL_ENV, LAUNCHER_ENV};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn tabs_drop_the_launching_claude_session_but_keep_user_settings() {
    let home = std::env::temp_dir().join(format!("raterm-it-launcher-env-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let kept = [("CLAUDE_CONFIG_DIR", "/cfg"), ("CLAUDE_CODE_USE_BEDROCK", "1")];
    let leaked = LAUNCHER_ENV.iter().chain(CLAUDE_CODE_SHELL_ENV).map(|k| (*k, "leaked"));
    // An empty HOME, so the login shell's ~/.profile cannot add or reset variables.
    let home_var = [("HOME", home.to_str().unwrap())];
    for (key, value) in leaked.chain(kept).chain(home_var) {
        // SAFETY: the only test in this binary, so nothing else reads or writes the
        // environment concurrently; set before `Sessions` starts any thread.
        unsafe { std::env::set_var(key, value) };
    }

    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = output.clone();
    let (tx, exit) = mpsc::channel();
    let request = SpawnRequest { cols: 80, rows: 24, launch: Launch::Command("env".into()), ..Default::default() };
    Sessions::new("/bin/sh", Duration::from_millis(300))
        .spawn(
            request,
            move |bytes| {
                sink.lock().unwrap().extend_from_slice(bytes);
                true
            },
            move |code| {
                let _ = tx.send(code);
            },
        )
        .unwrap();
    let text = || String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
    let code = exit.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|e| panic!("env did not exit ({e}): {:?}", text()));
    assert_eq!(code, 0, "env failed: {:?}", text());

    let env = text();
    let names: Vec<&str> = env.lines().filter_map(|line| line.split_once('=').map(|(name, _)| name)).collect();
    for key in LAUNCHER_ENV.iter().chain(CLAUDE_CODE_SHELL_ENV) {
        assert!(!names.contains(key), "{key} reached the tab: {env:?}");
    }
    for (key, value) in kept {
        assert!(env.lines().any(|line| line.trim_end() == format!("{key}={value}")), "{key} was dropped: {env:?}");
    }
    let _ = std::fs::remove_dir_all(&home);
}
