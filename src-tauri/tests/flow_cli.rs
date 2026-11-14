#![cfg(unix)]
//! Runs the real `misag` binary's `flow` command, as a person or Claude would in a
//! terminal, against a database of its own.

use misag_lib::{claude, db};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod common;
use common::temp_dir;

const EXE: &str = env!("CARGO_BIN_EXE_misag");

const FLOW: &str = r#"{
  "misagFlow": 1,
  "name": "Fix it",
  "graph": {
    "nodes": [
      {"id": "start", "kind": "start"},
      {"id": "fix", "kind": "prompt", "title": "Fix", "text": "", "outputs": ["done"]},
      {"id": "check", "kind": "manual", "title": "Check", "outputs": ["ok"]}
    ],
    "edges": [{"from": "start", "output": "next", "to": "fix"}]
  }
}"#;

/// A database as the app leaves it, in a directory of its own.
fn database(name: &str) -> PathBuf {
    let path = temp_dir(name).join(db::FILE);
    db::Db::open(path.clone()).unwrap();
    path
}

/// Runs `misag <args>` with `MISAG_DB` set to `db` (unset with None), feeding it
/// `input`, as from a terminal outside misag's tabs.
fn misag(db: Option<&Path>, args: &[&str], input: &str) -> Output {
    misag_in(None, db, args, input)
}

/// `misag`, run in misag's tab `tab` when given (`MISAG_TAB` set).
fn misag_in(tab: Option<&str>, db: Option<&Path>, args: &[&str], input: &str) -> Output {
    let mut command = Command::new(EXE);
    command.args(args).env_remove(claude::DB_ENV).env_remove(claude::TAB_ENV).env("HOME", temp_dir("flow-cli-home"));
    if let Some(tab) = tab {
        command.env(claude::TAB_ENV, tab);
    }
    command.env_remove("XDG_DATA_HOME");
    if let Some(db) = db {
        command.env(claude::DB_ENV, db);
    }
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn help_describes_the_command_and_the_file_format() {
    for args in [&["flow", "help"][..], &["flow"], &["flow", "--help"]] {
        let o = misag(None, args, "");
        assert!(o.status.success(), "{args:?}: {}", err(&o));
        let help = out(&o);
        for needed in ["misag flow import", "misag flow export", "misag flow list", "\"misagFlow\": 1", "\"kind\": \"prompt\"", "maxRuns"] {
            assert!(help.contains(needed), "{args:?} help lacks {needed:?}");
        }
    }
}

#[test]
fn the_app_s_help_points_at_the_flow_command() {
    for flag in ["--help", "-h"] {
        let o = misag(None, &[flag], "");
        assert!(o.status.success());
        assert!(out(&o).contains("misag flow help"), "{}", out(&o));
    }
}

#[test]
fn a_mistyped_command_says_so_instead_of_starting_the_app() {
    for args in [&["flwo", "list"][..], &["--version"], &["list"]] {
        let o = misag(None, args, "");
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(err(&o).contains(&format!("misag: unknown command \"{}\"; see misag --help", args[0])), "{}", err(&o));
    }
}

#[test]
fn a_flow_from_stdin_is_added_and_listed_with_what_keeps_it_from_running() {
    let db = database("flow-cli-import");
    let o = misag(Some(&db), &["flow", "import", "-"], FLOW);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(out(&o), "Added \"Fix it\" as flow 1.\nBefore it can run:\n- \"Fix\" has no prompt.\n");
    let o = misag(Some(&db), &["flow", "list"], "");
    assert_eq!(out(&o), "1\tFix it\n");
}

#[test]
fn a_flow_added_from_a_tab_must_be_confirmed_in_the_app_before_it_runs() {
    let db = database("flow-cli-import-tab");
    let o = misag_in(Some("tab-1"), Some(&db), &["flow", "import", "-"], FLOW);
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains("confirm it in the app"), "{}", out(&o));
    let o = misag(Some(&db), &["flow", "list"], "");
    assert_eq!(out(&o), "1\tFix it\t(not yet confirmed)\n");
    let app = db::Db::open(db).unwrap();
    assert!(app.list_flows().unwrap()[0].unconfirmed);
}

#[test]
fn a_flow_file_can_be_added_under_another_name() {
    let db = database("flow-cli-import-file");
    let file = db.with_file_name("fix.json");
    std::fs::write(&file, FLOW).unwrap();
    let o = misag(None, &["flow", "import", file.to_str().unwrap(), "--name", "Mine", "--db", db.to_str().unwrap()], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).starts_with("Added \"Mine\" as flow 1.\n"), "{}", out(&o));
}

#[test]
fn an_exported_flow_imports_as_the_same_flow() {
    let db = database("flow-cli-export");
    misag(Some(&db), &["flow", "import", "-"], FLOW);
    let by_name = misag(Some(&db), &["flow", "export", "Fix it"], "");
    assert!(by_name.status.success(), "{}", err(&by_name));
    let file = db.with_file_name("out.json");
    let by_id = misag(Some(&db), &["flow", "export", "1", file.to_str().unwrap()], "");
    assert!(by_id.status.success(), "{}", err(&by_id));
    assert_eq!(out(&by_id), "");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), out(&by_name));
    let again = misag(Some(&db), &["flow", "import", file.to_str().unwrap()], "");
    assert!(out(&again).starts_with("Added \"Fix it\" as flow 2."), "{}", out(&again));
    let first: serde_json::Value = serde_json::from_str(&out(&by_name)).unwrap();
    let second: serde_json::Value = serde_json::from_str(&out(&misag(Some(&db), &["flow", "export", "2"], ""))).unwrap();
    assert_eq!(first, second);
}

#[test]
fn a_flow_that_is_not_there_or_not_one_says_so() {
    let db = database("flow-cli-missing");
    misag(Some(&db), &["flow", "import", "-"], FLOW);
    misag(Some(&db), &["flow", "import", "-"], FLOW);
    let o = misag(Some(&db), &["flow", "export", "Nope"], "");
    assert_eq!((o.status.code(), err(&o).as_str()), (Some(1), "misag: there is no flow \"Nope\"; see misag flow list\n"));
    let o = misag(Some(&db), &["flow", "export", "Fix it"], "");
    assert_eq!(err(&o), "misag: more than one flow is named \"Fix it\"; give its id (see misag flow list)\n");
    let o = misag(Some(&db), &["flow", "import", "-"], "{}");
    assert_eq!((o.status.code(), err(&o).as_str()), (Some(1), "misag: not a flow file: it has no \"misagFlow\": 1\n"));
    let o = misag(Some(&db), &["flow", "import", "/no/such.json"], "");
    assert!(err(&o).starts_with("misag: cannot read /no/such.json: "), "{}", err(&o));
}

#[test]
fn without_the_app_s_database_nothing_is_created() {
    let dir = temp_dir("flow-cli-no-db");
    let db = dir.join(db::FILE);
    let o = misag(Some(&db), &["flow", "list"], "");
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("no Mischief of Agents database at"), "{}", err(&o));
    assert!(!db.exists());
    // Without MISAG_DB, the app's own data directory under HOME: a dev build's, for a dev build.
    let o = misag(None, &["flow", "list"], "");
    let id = if cfg!(debug_assertions) { "io.github.ram4nd.misag.dev" } else { "io.github.ram4nd.misag" };
    let db = if cfg!(debug_assertions) { "dev/misag.db" } else { "misag.db" };
    let data = if cfg!(target_os = "macos") { "Library/Application Support" } else { ".local/share" };
    assert!(err(&o).contains(&format!("{data}/{id}/{db}")), "{}", err(&o));
}

#[test]
fn a_database_of_another_version_is_left_alone() {
    // Such as after an upgrade, with the old app still running: only the app migrates.
    let db = database("flow-cli-old-schema");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let known: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    conn.pragma_update(None, "user_version", known - 1).unwrap();
    drop(conn);
    let o = misag(Some(&db), &["flow", "list"], "");
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains(&format!("schema {} in", known - 1)), "{}", err(&o));
    assert!(err(&o).contains("restart"), "says what to do: {}", err(&o));
    let conn = rusqlite::Connection::open(&db).unwrap();
    assert_eq!(conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0)).unwrap(), known - 1);
}

// macOS's file system refuses names that are not UTF-8.
#[cfg(target_os = "linux")]
#[test]
fn a_file_name_that_is_not_utf8_is_read_as_given() {
    use std::os::unix::ffi::OsStrExt;
    let db = database("flow-cli-bytes");
    let file = db.with_file_name(std::ffi::OsStr::from_bytes(b"fl\xffow.json"));
    std::fs::write(&file, FLOW).unwrap();
    let o = Command::new(EXE).args(["flow", "import"]).arg(&file).env(claude::DB_ENV, &db).output().unwrap();
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).starts_with("Added \"Fix it\" as flow 1."), "{}", out(&o));
}

#[test]
fn a_name_with_control_or_hidden_characters_is_refused_and_never_printed_raw() {
    let db = database("flow-cli-escapes");
    let o = misag(Some(&db), &["flow", "import", "-"], &FLOW.replace("Fix it", "Fix\\u001b]0;pwned\\u0007"));
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(err(&o), "misag: a flow name cannot hold control or hidden characters (U+001B)\n");
    // One stored before names were checked.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO flows (name, position, created_at, updated_at) VALUES ('a' || char(27) || '[2Jb', 0, 0, 0)",
        [],
    )
    .unwrap();
    assert_eq!(out(&misag(Some(&db), &["flow", "list"], "")), "1\ta\u{FFFD}[2Jb\n");
}

#[test]
fn help_is_shown_wherever_it_is_asked_for() {
    for args in [&["flow", "import", "--help"][..], &["flow", "export", "1", "-h"], &["flow", "help", "import"]] {
        let o = misag(None, args, "");
        assert!(o.status.success(), "{args:?}: {}", err(&o));
        assert!(out(&o).contains("misag flow import"), "{args:?}");
    }
}

#[test]
fn options_belong_to_their_command() {
    let db = database("flow-cli-options");
    let o = misag(Some(&db), &["flow", "list", "--name", "x"], "");
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("--name is only for import"), "{}", err(&o));
    let o = misag(Some(&db), &["flow", "list", "--db"], "");
    assert_eq!(o.status.code(), Some(2));
    let o = misag(None, &["flow", "list", "--db", "a", "--db", "b"], "");
    assert!(err(&o).contains("--db is given twice"), "{}", err(&o));
}

#[test]
fn export_does_not_replace_a_file_unless_told_to() {
    let db = database("flow-cli-overwrite");
    misag(Some(&db), &["flow", "import", "-"], FLOW);
    let file = db.with_file_name("taken.json");
    std::fs::write(&file, "mine").unwrap();
    let o = misag(Some(&db), &["flow", "export", "1", file.to_str().unwrap()], "");
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("already exists; add --force to replace it"), "{}", err(&o));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine");
    let o = misag(Some(&db), &["flow", "export", "1", file.to_str().unwrap(), "--force"], "");
    assert!(o.status.success(), "{}", err(&o));
    assert!(std::fs::read_to_string(&file).unwrap().contains("\"misagFlow\": 1"));
}

#[test]
fn a_wrong_command_line_shows_the_usage() {
    for args in [&["flow", "frobnicate"][..], &["flow", "export"], &["flow", "import"], &["flow", "list", "extra"]] {
        let o = misag(None, args, "");
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(err(&o).contains("misag flow help"), "{args:?}: {}", err(&o));
    }
}
