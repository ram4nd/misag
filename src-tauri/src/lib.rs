pub mod claude;
pub mod cli;
mod commands;
pub mod db;
pub mod files;
pub mod flow_file;
pub mod flows;
pub mod history;
pub mod runs;
pub mod layout;
#[cfg(unix)]
mod login_path;
pub mod proc_tree;
pub mod session;
pub mod settings;
#[cfg(windows)]
mod win;

use std::path::PathBuf;
use tauri::{Manager, RunEvent};

/// Which binary is running: the app, or its console build (`misag-cli`, Windows),
/// which terminal tabs also run as `claude.exe` and `misag.exe`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Build {
    App,
    Console,
}

/// `misag --exec`: runs, in place of this process, the program and arguments in
/// `MISAG_EXEC` (a JSON array), so non-POSIX login shells can launch Claude tabs
/// without quoting the program or its arguments. `MISAG_EXEC` itself is not passed
/// on. Returns only on failure (on Windows, with the program's exit code), with the
/// exit code to use: 2 for bad usage, 126 when the program cannot be run, 127 when it
/// is not found, as a shell's.
pub fn run_exec() -> i32 {
    let (flag, var) = (session::EXEC_FLAG, session::EXEC_ENV);
    let raw = match std::env::var(var) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("misag: {flag} needs {var}: {e}");
            return 2;
        }
    };
    let argv: Vec<String> = match serde_json::from_str(&raw) {
        Ok(argv) => argv,
        Err(e) => {
            eprintln!("misag: {var} is not a JSON array of strings: {e}");
            return 2;
        }
    };
    let Some((program, args)) = argv.split_first() else {
        eprintln!("misag: {var} is empty");
        return 2;
    };
    let mut command = std::process::Command::new(program);
    command.args(args).env_remove(var);
    replace_with(command, program)
}

/// Runs `command` in place of this process (`program` names it in errors): execs it
/// on Unix; on Windows, which cannot, runs it and exits with its code. Returns the
/// exit code a shell uses when it cannot run: 127 not found, 126 otherwise.
fn replace_with(mut command: std::process::Command, program: &str) -> i32 {
    #[cfg(unix)]
    let err = std::os::unix::process::CommandExt::exec(&mut command);
    #[cfg(windows)]
    let err = {
        win::outlive_ctrl_c();
        match command.status() {
            // A program ended by Windows (no exit code) reports failure.
            Ok(status) => return status.code().unwrap_or(1),
            Err(err) => err,
        }
    };
    eprintln!("misag: could not run {program}: {err}");
    if err.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 }
}

/// `misag --claude-launch [args]`, run by the `claude` wrapper in terminal tabs:
/// runs, in place of this process, the real `claude` (the wrapper took itself off
/// `PATH`), started like a Claude tab with the settings and hook from the database
/// in `MISAG_DB`. Without a usable database it starts untracked with the cautious
/// settings; if only the settings cannot be read, it is still tracked. Returns only
/// on failure (on Windows, with Claude's exit code), with the exit code a shell uses
/// (127 not found, 126 otherwise).
pub fn run_launch(build: Build) -> i32 {
    launch_claude(build, std::env::args_os().skip(2).collect())
}

/// `run_launch` with Claude's arguments `claude_args`.
fn launch_claude(build: Build, claude_args: Vec<std::ffi::OsString>) -> i32 {
    let db_path = std::env::var_os(claude::DB_ENV).map(PathBuf::from);
    let store = db_path.as_deref().map(db::connect_existing).transpose();
    let settings = store.as_ref().map_err(String::clone).and_then(|conn| match conn {
        Some(conn) => db::load_settings(conn),
        None => Err(format!("{} is not set", claude::DB_ENV)),
    });
    let settings = settings.unwrap_or_else(|e| {
        eprintln!("misag: cannot read the settings ({e}); starting Claude with permission prompts and auto memory off");
        settings::Settings::cautious()
    });
    let exe = hook_exe(build);
    let hook = match (exe.path(), &store) {
        (Ok(exe), Ok(Some(_))) => Some(exe),
        (Err(e), Ok(Some(_))) => {
            eprintln!("misag: {e}, so this Claude session will not be remembered");
            None
        }
        _ => None,
    };
    let Some(claude) = real_claude() else {
        eprintln!("misag: claude is not on PATH (looked for {})", CLAUDE_FILES.join(", "));
        return 127;
    };
    let args = claude::launch_args(&settings, hook, claude_args);
    let mut command = std::process::Command::new(claude);
    command.args(args);
    replace_with(command, "claude")
}

/// The files the real `claude` may be: the native install, or the .cmd npm installs
/// on Windows (which `Command` would not find by its name).
#[cfg(windows)]
const CLAUDE_FILES: &[&str] = &["claude.exe", "claude.cmd", "claude.bat"];
#[cfg(unix)]
const CLAUDE_FILES: &[&str] = &["claude"];

/// The `claude` to run: on Unix whichever is on PATH (the wrapper took itself off).
#[cfg(unix)]
fn real_claude() -> Option<std::ffi::OsString> {
    Some("claude".into())
}

/// On Windows the wrapper is this binary, so its directory comes off this process's
/// PATH here (which Claude inherits), and the real `claude` is looked up there.
#[cfg(windows)]
fn real_claude() -> Option<std::ffi::OsString> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let exe = std::env::current_exe().ok();
    let own = exe.as_deref().and_then(std::path::Path::parent);
    let dirs = path_without(std::env::split_paths(&path).collect(), own);
    if let Ok(kept) = std::env::join_paths(&dirs) {
        // SAFETY: single-threaded here, before Claude starts.
        unsafe { std::env::set_var("PATH", kept) };
    }
    find_claude(&dirs, exe.as_deref(), |file| file.is_file()).map(PathBuf::into_os_string)
}

/// Windows compares paths without regard to case.
#[cfg(any(windows, test))]
fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| p.to_string_lossy().trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

/// The PATH directories `dirs` that may hold the real `claude`: absolute ones only (an
/// empty or relative entry would look in the tab's own folder, where a cloned project
/// could put a `claude.cmd`), and not `own`, the wrapper's.
#[cfg(any(windows, test))]
fn path_without(dirs: Vec<PathBuf>, own: Option<&std::path::Path>) -> Vec<PathBuf> {
    dirs.into_iter().filter(|dir| dir.is_absolute() && !own.is_some_and(|own| same_path(dir, own))).collect()
}

/// The first `claude.exe`, `.cmd` or `.bat` in `dirs`, skipping `own_exe` (the
/// wrapper), so the wrapper can never start itself.
#[cfg(any(windows, test))]
fn find_claude(dirs: &[PathBuf], own_exe: Option<&std::path::Path>, is_file: impl Fn(&std::path::Path) -> bool) -> Option<PathBuf> {
    let names = ["claude.exe", "claude.cmd", "claude.bat"];
    dirs.iter()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|file| is_file(file) && !own_exe.is_some_and(|own| same_path(file, own)))
}

/// `claude [args]` on Windows, run by the wrapper, which is this binary linked as
/// `claude.exe`: inside a Claude session (`CLAUDECODE`) the real `claude` unchanged,
/// so a `claude` that Claude itself runs cannot replace the session recorded for the
/// tab; otherwise as `misag --claude-launch`.
#[cfg(windows)]
pub fn run_as_claude_wrapper() -> i32 {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if std::env::var_os("CLAUDECODE").is_some() {
        let Some(claude) = real_claude() else {
            eprintln!("misag: claude is not on PATH (looked for {})", CLAUDE_FILES.join(", "));
            return 127;
        };
        let mut command = std::process::Command::new(claude);
        command.args(args);
        return replace_with(command, "claude");
    }
    launch_claude(Build::Console, args)
}

/// misag's own binary, for Claude's hook, `--claude-launch` and the `--exec` trampoline.
/// On Windows the console build (`CLI_EXE`) next to the app, which terminals wait for
/// and whose output they show, unlike the app's.
fn own_exe(build: Build) -> commands::AppExe {
    let exe = std::env::current_exe()
        // After an upgrade replaced the binary, run the new one at the same path.
        .map(session::live_path)
        .map_err(|e| format!("cannot locate the misag binary: {e}"));
    #[cfg(windows)]
    let exe = exe.and_then(|exe| {
        if build == Build::Console {
            return Ok(exe);
        }
        let cli = exe.with_file_name(CLI_EXE);
        if cli.is_file() { Ok(cli) } else { Err(format!("cannot find {} next to {}", CLI_EXE, exe.display())) }
    });
    #[cfg(unix)]
    let _ = build;
    commands::AppExe::new(exe)
}

/// The binary Claude's hook runs. The wrapper (`claude.exe`) cannot be it, as it
/// would start Claude again, so on Windows a wrapper uses the `misag.exe` shim, the
/// same console build under its own name, from the tab's `MISAG_CLI` directory.
fn hook_exe(build: Build) -> commands::AppExe {
    #[cfg(windows)]
    if build == Build::Console && invoked_as_claude() {
        let shim = std::env::var_os(claude::CLI_ENV).map(|dir| PathBuf::from(dir).join("misag.exe"));
        return commands::AppExe::new(match shim {
            Some(shim) if shim.is_file() => Ok(shim),
            _ => Err(format!("the misag shim is not on this tab's PATH ({} is not set)", claude::CLI_ENV)),
        });
    }
    own_exe(build)
}

/// The console build of misag that Windows installs next to the app.
#[cfg(windows)]
pub const CLI_EXE: &str = "misag-cli.exe";

/// What a command line asks misag to do.
#[derive(Debug, PartialEq)]
enum Mode {
    /// Start the app (no arguments to the app).
    App,
    Hook,
    Launch,
    Exec,
    Flow,
    Help,
    /// The console build as `claude.exe`: every argument is Claude's.
    ClaudeWrapper,
    /// The console build with no arguments, which only commands use.
    NoCommand,
    Unknown(String),
}

/// Reads `first`, the first argument, for `build`; `as_claude` says the binary runs
/// as `claude.exe`, which settles it before any argument, since every argument then
/// belongs to Claude (`claude flow ...` is a prompt, not misag's `flow`).
fn mode(build: Build, first: Option<&std::ffi::OsStr>, as_claude: bool) -> Mode {
    if build == Build::Console && as_claude {
        return Mode::ClaudeWrapper;
    }
    let Some(first) = first else {
        return if build == Build::Console { Mode::NoCommand } else { Mode::App };
    };
    match first.to_str() {
        Some(claude::HOOK_FLAG) => Mode::Hook,
        Some(claude::LAUNCH_FLAG) => Mode::Launch,
        Some(session::EXEC_FLAG) => Mode::Exec,
        Some(cli::FLOW_COMMAND) => Mode::Flow,
        Some("--help" | "-h") => Mode::Help,
        _ => Mode::Unknown(first.to_string_lossy().into_owned()),
    }
}

/// The command-line modes, shared by the app and its console build: the exit code,
/// or None to start the app.
pub fn dispatch(build: Build) -> Option<i32> {
    use std::io::{IsTerminal, Write};
    // `args_os`, so an argument that is not UTF-8 cannot panic before the app starts.
    let first = std::env::args_os().nth(1);
    #[cfg(windows)]
    let as_claude = invoked_as_claude();
    #[cfg(unix)]
    let as_claude = false;
    let code = match mode(build, first.as_deref(), as_claude) {
        Mode::App => return None,
        // Claude Code runs this binary as its hook.
        Mode::Hook => claude::run_hook(),
        // The `claude` wrapper in terminal tabs starts Claude through this binary.
        Mode::Launch => run_launch(build),
        // Non-POSIX login shells launch Claude tabs through this binary.
        Mode::Exec => run_exec(),
        // Flows from the command line, for a person or for Claude.
        Mode::Flow => cli::run(std::env::args_os().skip(2).collect()),
        #[cfg(windows)]
        Mode::ClaudeWrapper => run_as_claude_wrapper(),
        #[cfg(unix)]
        Mode::ClaudeWrapper => unreachable!("only Windows links the console build as claude"),
        // Not print!, which panics on a closed stdout.
        Mode::Help => {
            let _ = std::io::stdout().write_all(cli::APP_HELP.as_bytes());
            0
        }
        Mode::NoCommand => {
            // With input waiting, this is likely Claude Code running misag's hook
            // without its arguments, as one that predates hooks' `args` does.
            let why = if std::io::stdin().is_terminal() {
                String::new()
            } else {
                format!(" (if Claude Code ran this as a hook, update it: misag's hooks need {})", claude::HOOK_ARGS_SINCE)
            };
            let _ = writeln!(std::io::stderr(), "misag: give a command; see misag --help{why}");
            2
        }
        // Anything else is a mistake, such as a mistyped `flow`: say so rather than
        // start the app, which a person or Claude might not notice.
        Mode::Unknown(shown) => {
            let _ = writeln!(std::io::stderr(), "misag: unknown command {shown:?}; see misag --help");
            2
        }
    };
    Some(code)
}

/// Whether this process runs as `claude.exe`, the wrapper's name.
#[cfg(windows)]
fn invoked_as_claude() -> bool {
    let exe = std::env::current_exe().ok();
    exe.as_deref().and_then(std::path::Path::file_stem).is_some_and(|stem| stem.eq_ignore_ascii_case("claude"))
}

/// The log the app writes its diagnostics to when it has no terminal: started from
/// a menu, Finder or the Start menu, its stderr goes nowhere (on Windows it has none).
pub const LOG_FILE: &str = "misag.log";
/// A log larger than this is started over at the next launch.
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Sends stderr to `LOG_FILE` in `dir` unless a terminal shows it.
fn log_to_file(dir: &std::path::Path) {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        return;
    }
    let path = dir.join(LOG_FILE);
    let too_big = std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX_BYTES);
    let file = std::fs::create_dir_all(dir).and_then(|()| {
        std::fs::OpenOptions::new().create(true).append(!too_big).write(true).truncate(too_big).open(&path)
    });
    let file = match file {
        Ok(file) => file,
        Err(e) => {
            eprintln!("misag: cannot write the log {}: {e}", path.display());
            return;
        }
    };
    #[cfg(unix)]
    let redirected = {
        use std::os::fd::AsRawFd;
        // SAFETY: plain syscall; `file` is leaked below, so its descriptor stays open.
        if unsafe { libc::dup2(file.as_raw_fd(), 2) } < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    };
    #[cfg(windows)]
    let redirected = win::redirect_stderr(&file);
    match redirected {
        // Kept open for the rest of the run.
        Ok(()) => std::mem::forget(file),
        Err(e) => eprintln!("misag: cannot log to {}: {e}", path.display()),
    }
}

/// The app's database, ready to use, taken over from raterm's in `old_dir` on the
/// first start. Problems are logged and shown to the user, never fatal: the app
/// must start even if saved state cannot be read.
fn open_store(dir: Result<PathBuf, String>, old_dir: Option<PathBuf>) -> db::Db {
    let db = db::Db::open_or_import(dir.map(|d| d.join(db::FILE)), old_dir.map(|d| d.join(db::RATERM_FILE)));
    if let Err(e) = db.prune_claude_sessions() {
        eprintln!("misag: could not prune Claude sessions: {e}");
    }
    if let Err(e) = db.resume_runs() {
        eprintln!("misag: could not resume the flow runs: {e}");
    }
    db
}

/// Where this build keeps its data in the app data directory `dir`: debug builds in
/// a subdirectory, so even a dev build run without the dev config's identifier never
/// touches an installed misag's.
pub(crate) fn in_build(dir: PathBuf) -> PathBuf {
    if cfg!(debug_assertions) { dir.join("dev") } else { dir }
}

/// Where raterm, misag's old name, kept its data, given misag's app data directory:
/// the same place under raterm's identifier.
fn raterm_data_dir(dir: &std::path::Path) -> Option<PathBuf> {
    let name = dir.file_name()?.to_str()?;
    let suffix = name.strip_prefix("io.github.ram4nd.misag")?;
    Some(dir.with_file_name(format!("com.tag1.raterm{suffix}")))
}

/// The `claude` wrapper for terminal tabs and the `misag` shim for every tab.
/// Without the wrapper, `claude` in a terminal tab runs untracked; without the shim,
/// `misag` in a tab is whichever is on PATH, if any. Nothing worse: the UI says why
/// (see `store_status`).
fn install_wrapper(store: &db::Db, exe: &commands::AppExe) -> commands::Wrapper {
    let install = |what: &str, install: fn(&std::path::Path, &std::path::Path) -> Result<PathBuf, String>| {
        let installed = match (store.hook_path(), exe.path()) {
            (None, _) => Err("Mischief of Agents is running without its database".to_string()),
            (_, Err(e)) => Err(e.to_string()),
            (Some(db), Ok(exe)) => install(db, exe),
        };
        if let Err(e) = &installed {
            eprintln!("misag: no {what}: {e}");
        }
        installed
    };
    commands::Wrapper {
        claude: install("claude wrapper for terminal tabs", claude::install_wrapper),
        cli: install("misag shim for tabs", claude::install_cli),
    }
}

pub fn run() {
    #[cfg(target_os = "macos")]
    login_path::start(session::default_shell());
    let exe = own_exe(Build::App);
    let sessions = match exe.path_str() {
        Ok(exe) => session::Sessions::default().with_trampoline(exe.to_owned()),
        Err(e) => {
            eprintln!("misag: {e}; Claude tabs cannot start");
            session::Sessions::default()
        }
    };
    tauri::Builder::default()
        // First, so a second launch only focuses the running window: two instances
        // would overwrite each other's saved tabs.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let Some(window) = app.get_webview_window("main") else {
                eprintln!("misag: another launch was refused, but there is no window to focus");
                return;
            };
            if let Err(e) = window.unminimize().and_then(|()| window.set_focus()) {
                eprintln!("misag: could not focus the window for another launch: {e}");
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(sessions)
        .manage(exe)
        .manage(history::Cache::default())
        .setup(|app| {
            let dir = app.path().app_data_dir().map_err(|e| e.to_string());
            if let Ok(dir) = &dir {
                log_to_file(&in_build(dir.clone()));
            }
            #[cfg(unix)]
            login_path::report();
            let old_dir = dir.as_deref().ok().and_then(raterm_data_dir);
            let store = open_store(dir.map(in_build), old_dir.map(in_build));
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
            commands::background_tabs,
            commands::layout_load,
            commands::layout_save,
            commands::store_status,
            commands::claude_reported,
            commands::claude_open_sessions,
            commands::settings_load,
            commands::settings_save,
            commands::fs_list,
            commands::claude_history,
            commands::claude_session_pin,
            commands::claude_tab_sessions,
            commands::flows_list,
            commands::flow_create,
            commands::flow_rename,
            commands::flow_set_archived,
            commands::flow_confirm,
            commands::flows_reorder,
            commands::flow_delete,
            commands::flow_load,
            commands::flow_save_graph,
            commands::flow_import_file,
            commands::primary_selection,
            commands::clipboard_text,
            commands::flow_export_file,
            commands::flow_start,
            commands::flow_cleared,
            commands::flow_prefill_text,
            commands::flow_prefilled,
            commands::flow_continue,
            commands::flow_resume,
            commands::flow_unsent,
            commands::flow_stop,
            commands::flow_runs,
            commands::flow_stop_tab,
        ])
        .build(tauri::generate_context!())
        .expect("error while building misag")
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
        let dir = std::env::temp_dir().join(format!("misag-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
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
    use std::path::Path;

    #[test]
    fn modes_follow_the_first_argument() {
        use std::ffi::OsStr;
        let mode_of = |build, first: Option<&str>| mode(build, first.map(OsStr::new), false);
        assert_eq!(mode_of(Build::App, None), Mode::App);
        assert_eq!(mode_of(Build::Console, None), Mode::NoCommand, "the console build only runs commands");
        assert_eq!(mode_of(Build::App, Some(claude::HOOK_FLAG)), Mode::Hook);
        assert_eq!(mode_of(Build::Console, Some(claude::LAUNCH_FLAG)), Mode::Launch);
        assert_eq!(mode_of(Build::App, Some(session::EXEC_FLAG)), Mode::Exec);
        assert_eq!(mode_of(Build::App, Some("flow")), Mode::Flow);
        assert_eq!(mode_of(Build::App, Some("-h")), Mode::Help);
        assert_eq!(mode_of(Build::App, Some("flwo")), Mode::Unknown("flwo".into()));
    }

    #[test]
    fn as_claude_every_argument_is_claudes() {
        use std::ffi::OsStr;
        for first in [None, Some("flow"), Some(claude::HOOK_FLAG), Some("--help")] {
            assert_eq!(mode(Build::Console, first.map(OsStr::new), true), Mode::ClaudeWrapper, "{first:?}");
        }
        assert_eq!(mode(Build::App, Some(OsStr::new("flow")), true), Mode::Flow, "only the console build wraps claude");
    }

    #[test]
    fn the_wrapper_finds_claude_but_never_itself() {
        // Absolute on every platform; `/usr/bin` is not on Windows.
        let base = std::env::temp_dir();
        let at = |p: &str| base.join(p);
        let own = at("data/bin");
        let upper = PathBuf::from(own.to_string_lossy().to_uppercase());
        // Relative and empty entries (`;;`) would look in the tab's own folder.
        let given = vec![at("data/bin/"), PathBuf::new(), PathBuf::from("rel"), at("usr"), upper, at("local")];
        let kept = path_without(given, Some(&own));
        assert_eq!(kept, vec![at("usr"), at("local")]);
        let present = [at("usr").join("claude.cmd"), at("local").join("claude.exe")];
        let is_file = |p: &Path| present.iter().any(|f| f == p);
        assert_eq!(find_claude(&kept, None, is_file), Some(at("usr").join("claude.cmd")));
        let own_exe = PathBuf::from(at("usr").join("Claude.cmd").to_string_lossy().to_uppercase());
        assert_eq!(find_claude(&kept, Some(&own_exe), is_file), Some(at("local").join("claude.exe")));
        assert_eq!(find_claude(&kept, None, |_| false), None, "not found: the caller exits 127");
    }

    /// The values the page reads, one of each shape, as src/ipc.fixtures.json holds
    /// them; src/ipc.test.ts checks the TypeScript mirrors against that file, so a
    /// field renamed, added or dropped on either side fails a test.
    #[test]
    fn the_page_s_mirrors_match_the_values_sent() {
        let session = SessionId::parse("0f8fad5b-d9cb-469f-a165-70867728950e").unwrap();
        let past = history::Past { id: session, title: "Fix it".into(), cwd: "/p".into(), modified: 1, pinned: true };
        let values = serde_json::json!({
            "run": runs::Run {
                id: 1,
                flow_id: 2,
                flow_name: "Deploy".into(),
                tab_id: "tab-1".into(),
                step_id: "p".into(),
                step: "Fix".into(),
                status: runs::Status::Attention,
                note: Some("Why".into()),
                outputs: vec!["done".into()],
                steps: 3,
                done: 1,
                total: 4,
                resumable: true,
                prefill: false,
                step_kind: Some(crate::flows::Kind::Prompt),
                briefing: false,
            },
            "prefill": runs::Prefill { text: "Fix it".into(), token: 7 },
            "started": runs::Started { run_id: 1, clear: true, prompt: Some("Go".into()) },
            "cleared": [runs::Cleared::Pending, runs::Cleared::Ready { prompt: Some("Go".into()) }, runs::Cleared::Gone],
            "flow": db::FlowSummary { id: 1, name: "Deploy".into(), archived: false, clear: true, unconfirmed: true },
            "page": history::Page { sessions: vec![past], more: true, unreadable: 0, gone_pins: Vec::new() },
        });
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/ipc.fixtures.json");
        let text = serde_json::to_string_pretty(&values).unwrap() + "\n";
        if std::env::var_os("UPDATE_IPC_FIXTURES").is_some() {
            std::fs::write(&file, &text).unwrap();
        }
        // A Windows checkout may have turned its line ends into CRLF.
        let stored = std::fs::read_to_string(&file).unwrap_or_default().replace("\r\n", "\n");
        assert!(stored == text, "{} is out of date: run UPDATE_IPC_FIXTURES=1 cargo test, then fix the TypeScript mirrors", file.display());
    }

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

        let store = open_store(Ok(dir.clone()), None);
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM claude_sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
        let loaded = store.load_layout().unwrap();
        assert_eq!(loaded.tabs[0].kind, TabKind::Claude { claude_session: Some(session) });
        assert_eq!(store.status(), db::StoreStatus::Saving);
    }

    #[test]
    fn raterm_kept_its_data_next_to_misags() {
        let old = |dir: &str| raterm_data_dir(std::path::Path::new(dir));
        assert_eq!(old("/d/io.github.ram4nd.misag"), Some(PathBuf::from("/d/com.tag1.raterm")));
        assert_eq!(old("/d/io.github.ram4nd.misag.dev"), Some(PathBuf::from("/d/com.tag1.raterm.dev")));
        assert_eq!(old("/d/somebody.else"), None);
    }

    #[test]
    fn the_raterm_data_dir_follows_the_configured_identifiers() {
        for (file, raterm) in [("tauri.conf.json", "com.tag1.raterm"), ("tauri.dev.conf.json", "com.tag1.raterm.dev")] {
            let config = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file)).unwrap();
            let config: serde_json::Value = serde_json::from_str(&config).unwrap();
            let identifier = config["identifier"].as_str().unwrap();
            let dir = std::path::Path::new("/d").join(identifier);
            assert_eq!(raterm_data_dir(&dir), Some(PathBuf::from("/d").join(raterm)), "{file}");
        }
    }

    #[test]
    fn a_store_without_a_directory_still_works() {
        let store = open_store(Err("no data dir".into()), None);
        assert!(matches!(store.status(), db::StoreStatus::NotSaving { .. }));
    }
}
