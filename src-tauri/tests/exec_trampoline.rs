#![cfg(unix)]
//! Claude tabs under a non-POSIX login shell (fish, csh, ...) go through
//! `misag --exec`. A shell named `fish` that is really `sh` stands in for one
//! here, since it is enough to route the launch through the trampoline.

mod common;

use misag_lib::session::{Launch, Sessions, SpawnRequest};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

#[test]
fn programs_run_through_the_trampoline_with_arguments_intact() {
    let dir = common::temp_dir("exec");
    let fake_fish = dir.join("fish");
    std::os::unix::fs::symlink("/bin/sh", &fake_fish).unwrap();

    let sessions = Sessions::new(fake_fish.to_str().unwrap(), Duration::from_millis(300))
        .with_trampoline(env!("CARGO_BIN_EXE_misag").into());
    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = output.clone();
    let (tx, exited) = mpsc::channel();
    let args = ["%s|", "a b", "it's", "$HOME", "\"q\""].map(String::from).to_vec();
    let req = SpawnRequest { cols: 80, rows: 24, launch: Launch::Program { program: "printf".into(), args }, ..Default::default() };
    sessions
        .spawn(
            req,
            move |b| {
                sink.lock().unwrap().extend_from_slice(b);
                true
            },
            move |c| tx.send(c).unwrap(),
        )
        .unwrap();
    assert_eq!(exited.recv_timeout(Duration::from_secs(10)).unwrap(), 0);
    let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
    assert!(text.contains(r#"a b|it's|$HOME|"q"|"#), "output: {text:?}");
}

fn exec(program_list: Option<&str>) -> std::process::Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_misag"));
    cmd.arg("--exec").stderr(std::process::Stdio::null());
    match program_list {
        Some(list) => cmd.env("MISAG_EXEC", list),
        None => cmd.env_remove("MISAG_EXEC"),
    };
    cmd.output().unwrap()
}

#[test]
fn exec_mode_reports_bad_program_lists_and_missing_programs() {
    assert_eq!(exec(None).status.code(), Some(2), "no list");
    assert_eq!(exec(Some("not json")).status.code(), Some(2), "not JSON");
    assert_eq!(exec(Some("[]")).status.code(), Some(2), "empty");
    assert_eq!(exec(Some(r#"["/no/such/program"]"#)).status.code(), Some(127), "not found");
    // A file that exists but is not executable (macOS has no /etc/hostname).
    let file = std::env::temp_dir().join(format!("misag-not-runnable-{}", std::process::id()));
    std::fs::write(&file, "data").unwrap();
    let list = serde_json::to_string(&[file.to_str().unwrap()]).unwrap();
    assert_eq!(exec(Some(&list)).status.code(), Some(126), "not runnable");
    let _ = std::fs::remove_file(&file);
}

#[test]
fn exec_mode_does_not_pass_its_variable_on() {
    let out = exec(Some(r#"["sh","-c","echo ${MISAG_EXEC-unset}"]"#));
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "unset");
}
