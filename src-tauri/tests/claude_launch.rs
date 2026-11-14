#![cfg(unix)]
//! Runs the real `misag` binary in `--claude-launch` mode, as the `claude` wrapper
//! in terminal tabs does, with a stand-in `claude` that prints its arguments.

use misag_lib::settings::Settings;
use misag_lib::{claude, db};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;
use common::temp_dir;

const EXE: &str = env!("CARGO_BIN_EXE_misag");

/// A directory holding a `claude` that prints its arguments, one per line.
fn real_claude(dir: &Path) -> PathBuf {
    let bin = dir.join("real");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("claude"), "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Runs `misag --claude-launch <args>` with `PATH` set to `path`. A `db` of None
/// leaves `MISAG_DB` unset.
fn launch(path: &Path, db: Option<&Path>, args: &[&str]) -> Output {
    let mut command = Command::new(EXE);
    command
        .arg(claude::LAUNCH_FLAG)
        .args(args)
        // Only `path`, so a real `claude` installed elsewhere can never run.
        .env("PATH", path)
        .env(claude::TAB_ENV, "tab-1")
        .env_remove(claude::DB_ENV);
    if let Some(db) = db {
        command.env(claude::DB_ENV, db);
    }
    command.output().unwrap()
}

fn lines(out: &Output) -> Vec<String> {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout.clone()).unwrap().lines().map(str::to_owned).collect()
}

#[test]
fn runs_claude_with_the_current_settings_and_the_hook() {
    let dir = temp_dir("launch");
    let db_path = dir.join(db::FILE);
    let store = db::Db::open(db_path.clone()).unwrap();
    store.save_settings(Settings { skip_permissions: true, disable_auto_memory: true, ..Settings::default() }).unwrap();

    let out = lines(&launch(&real_claude(&dir), Some(&db_path), &["--resume", "it's"]));
    assert_eq!(out[..2], ["--dangerously-skip-permissions", "--settings"]);
    let settings: serde_json::Value = serde_json::from_str(&out[2]).unwrap();
    assert_eq!(settings["autoMemoryEnabled"], false);
    assert_eq!(settings["hooks"]["SessionStart"][0]["hooks"][0]["command"], claude::hook_command(Path::new(EXE)));
    assert_eq!(out[3..], ["--resume", "it's"], "the user's arguments come last, unchanged");
}

#[test]
fn a_settings_change_applies_to_the_next_launch() {
    let dir = temp_dir("launch-change");
    let db_path = dir.join(db::FILE);
    let store = db::Db::open(db_path.clone()).unwrap();
    let bin = real_claude(&dir);
    assert_eq!(lines(&launch(&bin, Some(&db_path), &[]))[0], "--settings", "defaults: no skip flag");
    store.save_settings(Settings { skip_permissions: true, ..Settings::default() }).unwrap();
    assert_eq!(lines(&launch(&bin, Some(&db_path), &[]))[0], "--dangerously-skip-permissions");
}

#[test]
fn without_a_database_claude_starts_cautiously_and_untracked() {
    let dir = temp_dir("launch-nodb");
    let bin = real_claude(&dir);
    let cautious = ["--settings", r#"{"autoMemoryEnabled":false}"#, "-p"];
    assert_eq!(lines(&launch(&bin, None, &["-p"])), cautious);
    let missing = dir.join(db::FILE);
    assert_eq!(lines(&launch(&bin, Some(&missing), &["-p"])), cautious);
    assert!(!missing.exists(), "the launcher never creates the database");
}

#[test]
fn an_unusable_database_starts_claude_cautiously_and_untracked() {
    let dir = temp_dir("launch-baddb");
    let bin = real_claude(&dir);
    let cautious = ["--settings", r#"{"autoMemoryEnabled":false}"#];
    let corrupt = dir.join("corrupt.db");
    std::fs::write(&corrupt, "not a database").unwrap();
    assert_eq!(lines(&launch(&bin, Some(&corrupt), &[])), cautious);
    // A schema this binary does not know, as after an upgrade by another misag.
    let newer = dir.join(db::FILE);
    drop(db::Db::open(newer.clone()).unwrap());
    db::connect_existing(&newer).unwrap().pragma_update(None, "user_version", 999).unwrap();
    assert_eq!(lines(&launch(&bin, Some(&newer), &[])), cautious);
}

#[test]
fn without_a_real_claude_it_fails_like_a_shell() {
    let dir = temp_dir("launch-missing");
    let out = launch(&dir, None, &[]);
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("claude"));
}
