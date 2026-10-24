pub mod claude;
mod commands;
pub mod db;
pub mod files;
pub mod flows;
pub mod runs;
pub mod layout;
pub mod session;
pub mod settings;

use std::path::PathBuf;
use tauri::{Manager, RunEvent};

/// `raterm --exec`: replaces this process with the program and arguments in
/// `RATERM_EXEC` (a JSON array), so non-POSIX login shells can launch Claude tabs
/// without quoting the program or its arguments. `RATERM_EXEC` itself is not passed
/// on. Returns only on failure, with the exit code to use.
///
/// Exit codes follow the shell's: 2 for bad usage, 126 when the program cannot be
/// run, 127 when it is not found.
pub fn run_exec() -> i32 {
    use std::os::unix::process::CommandExt;
    let (flag, var) = (session::EXEC_FLAG, session::EXEC_ENV);
    let raw = match std::env::var(var) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("raterm: {flag} needs {var}: {e}");
            return 2;
        }
    };
    let argv: Vec<String> = match serde_json::from_str(&raw) {
        Ok(argv) => argv,
        Err(e) => {
            eprintln!("raterm: {var} is not a JSON array of strings: {e}");
            return 2;
        }
    };
    let Some((program, args)) = argv.split_first() else {
        eprintln!("raterm: {var} is empty");
        return 2;
    };
    let err = std::process::Command::new(program).args(args).env_remove(var).exec();
    eprintln!("raterm: could not run {program}: {err}");
    if err.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 }
}

/// `raterm --claude-launch [args]`, run by the `claude` wrapper in terminal tabs:
/// replaces this process with the real `claude` (the wrapper took itself off
/// `PATH`), started like a Claude tab with the settings and hook from the database
/// in `RATERM_DB`. Without a usable database it starts untracked with the cautious
/// settings; if only the settings cannot be read, it is still tracked. Returns only
/// on failure, with the exit code a shell uses (127 not found, 126 otherwise).
pub fn run_launch() -> i32 {
    use std::os::unix::process::CommandExt;
    let db_path = std::env::var_os(claude::DB_ENV).map(PathBuf::from);
    let store = db_path.as_deref().map(db::connect_existing).transpose();
    let settings = store.as_ref().map_err(String::clone).and_then(|conn| match conn {
        Some(conn) => db::load_settings(conn),
        None => Err(format!("{} is not set", claude::DB_ENV)),
    });
    let settings = settings.unwrap_or_else(|e| {
        eprintln!("raterm: cannot read the settings ({e}); starting Claude with permission prompts and auto memory off");
        settings::Settings::cautious()
    });
    let exe = own_exe();
    let hook = match (exe.path(), &store) {
        (Ok(exe), Ok(Some(_))) => Some(exe),
        (Err(e), Ok(Some(_))) => {
            eprintln!("raterm: {e}, so this Claude session will not be remembered");
            None
        }
        _ => None,
    };
    let args = claude::launch_args(&settings, hook, std::env::args_os().skip(2));
    let err = std::process::Command::new("claude").args(args).exec();
    eprintln!("raterm: could not run claude: {err}");
    if err.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 }
}

/// raterm's own binary, for Claude's hook, `--claude-launch` and the `--exec` trampoline.
fn own_exe() -> commands::AppExe {
    commands::AppExe::new(
        std::env::current_exe()
            // After an upgrade replaced the binary, run the new one at the same path.
            .map(session::live_path)
            .map_err(|e| format!("cannot locate the raterm binary: {e}")),
    )
}

/// The app's database, ready to use. Problems are logged and shown to the user,
/// never fatal: the app must start even if saved state cannot be read.
fn open_store(dir: Result<PathBuf, String>) -> db::Db {
    let db = db::Db::open_or_recover(dir.map(|d| d.join(db::FILE)));
    if let Err(e) = db.prune_claude_sessions() {
        eprintln!("raterm: could not prune Claude sessions: {e}");
    }
    if let Err(e) = db.resume_runs() {
        eprintln!("raterm: could not resume the flow runs: {e}");
    }
    db
}

/// The `claude` wrapper for terminal tabs. Without it, `claude` in a terminal tab
/// runs untracked, nothing worse; the UI says why (see `store_status`).
fn install_wrapper(store: &db::Db, exe: &commands::AppExe) -> commands::Wrapper {
    let installed = match (store.hook_path(), exe.path()) {
        (None, _) => Err("raterm is running without its database".to_string()),
        (_, Err(e)) => Err(e.to_string()),
        (Some(db), Ok(exe)) => claude::install_wrapper(db, exe),
    };
    if let Err(e) = &installed {
        eprintln!("raterm: no claude wrapper for terminal tabs: {e}");
    }
    commands::Wrapper(installed)
}

pub fn run() {
    let exe = own_exe();
    let sessions = match exe.path_str() {
        Ok(exe) => session::Sessions::default().with_trampoline(exe.to_owned()),
        Err(e) => {
            eprintln!("raterm: {e}; Claude tabs cannot start");
            session::Sessions::default()
        }
    };
    tauri::Builder::default()
        // First, so a second launch only focuses the running window: two instances
        // would overwrite each other's saved tabs.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let Some(window) = app.get_webview_window("main") else {
                eprintln!("raterm: another launch was refused, but there is no window to focus");
                return;
            };
            if let Err(e) = window.unminimize().and_then(|()| window.set_focus()) {
                eprintln!("raterm: could not focus the window for another launch: {e}");
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(sessions)
        .manage(exe)
        .setup(|app| {
            // Debug builds keep their data in a subdirectory, so even a dev build run
            // without the dev config's identifier never touches an installed raterm's.
            let dir = app.path().app_data_dir().map_err(|e| e.to_string());
            let dir = dir.map(|d| if cfg!(debug_assertions) { d.join("dev") } else { d });
            let store = open_store(dir);
            app.manage(install_wrapper(&store, &app.state::<commands::AppExe>()));
            app.manage(store);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::pty_spawn,
            commands::pty_write,
            commands::pty_resize,
            commands::pty_kill,
            commands::pty_reset,
            commands::pty_cwds,
            commands::layout_load,
            commands::layout_save,
            commands::store_status,
            commands::claude_reported,
            commands::claude_open_sessions,
            commands::settings_load,
            commands::settings_save,
            commands::fs_list,
            commands::flows_list,
            commands::flow_create,
            commands::flow_rename,
            commands::flow_set_archived,
            commands::flows_reorder,
            commands::flow_delete,
            commands::flow_load,
            commands::flow_save_graph,
            commands::flow_start,
            commands::flow_continue,
            commands::flow_stop,
            commands::flow_runs,
            commands::flow_stop_tab,
        ])
        .build(tauri::generate_context!())
        .expect("error while building raterm")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                app.state::<session::Sessions>().kill_all_and_wait();
            }
        });
}

/// Helpers shared by the unit tests of every module.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    /// An empty directory for one test, named after it and this test process.
    pub fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("raterm-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Permission tests skip themselves as root, which file permissions do not restrict.
    pub fn running_as_root() -> bool {
        // SAFETY: plain syscall without arguments.
        let root = unsafe { libc::geteuid() } == 0;
        if root {
            eprintln!("skipped: file permissions do not restrict root");
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude::SessionId;
    use layout::{Layout, Tab, TabId, TabKind};

    #[test]
    fn opening_the_store_prunes_reports_for_closed_tabs() {
        let dir = test_support::temp_dir("store");
        let session = SessionId::parse("0f8fad5b-d9cb-469f-a165-70867728950e").unwrap();

        // A previous run left a report for a tab that is no longer open.
        let previous = db::Db::open(dir.join(db::FILE)).unwrap();
        let tab = Tab {
            tab_id: TabId::parse("kept").unwrap(),
            kind: TabKind::Claude { claude_session: None },
            name: "c".into(),
            renamed: false,
            cwd: None,
        };
        previous.save_layout(Layout { active: 0, tabs: vec![tab] }).unwrap();
        let conn = db::connect_existing(&dir.join(db::FILE)).unwrap();
        for tab_id in ["kept", "closed"].map(|id| TabId::parse(id).unwrap()) {
            db::record_claude_session(&conn, &db::tests::report(&tab_id, &session)).unwrap();
        }
        drop(previous);

        let store = open_store(Ok(dir.clone()));
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM claude_sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
        let loaded = store.load_layout().unwrap();
        assert_eq!(loaded.tabs[0].kind, TabKind::Claude { claude_session: Some(session) });
        assert_eq!(store.status(), db::StoreStatus::Saving);
    }

    #[test]
    fn a_store_without_a_directory_still_works() {
        let store = open_store(Err("no data dir".into()));
        assert!(matches!(store.status(), db::StoreStatus::NotSaving { .. }));
    }
}
