//! Tauri commands over `session::Sessions` and `db::Db`. The ones that touch the
//! database or the filesystem run off the main thread (`#[tauri::command(async)]`):
//! the database may wait up to its busy timeout for the hook's write lock.

use crate::claude::{self, HookProblem, SessionId};
use crate::db::{Db, FlowSummary, StoreStatus};
use crate::flows::{self, Graph};
use crate::layout::{Layout, TabId};
use crate::session::{self, Launch, SpawnError, Sessions, SpawnRequest};
use crate::settings::{skips_permissions, Settings};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tauri::ipc::{Channel, Response};
use tauri::State;

/// misag's own binary, resolved once at startup (see `session::live_path`); also
/// given to `Sessions` as its trampoline. Its path is written into shell command
/// lines, so holding an `Ok` one proves it is valid UTF-8.
pub struct AppExe(Result<PathBuf, String>);

impl AppExe {
    pub fn new(exe: Result<PathBuf, String>) -> Self {
        Self(exe.and_then(|exe| match exe.to_str() {
            Some(_) => Ok(exe),
            None => Err(format!("misag's path {exe:?} is not valid UTF-8")),
        }))
    }

    pub fn path(&self) -> Result<&Path, &str> {
        self.0.as_deref().map_err(String::as_str)
    }

    /// The path as text; `new` checked that it is UTF-8.
    pub fn path_str(&self) -> Result<&str, &str> {
        self.path().map(|p| p.to_str().unwrap_or_default())
    }
}

/// What a tab runs once started: unlike `layout::TabKind`, a Claude tab always has
/// its session. Mirrored by `RunningKind` in src/main.ts.
#[derive(Deserialize, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum RunningKind {
    /// `claude_session`: the Claude session to resume at the shell prompt, for a
    /// restored tab that had it open.
    Terminal {
        command: Option<String>,
        #[serde(default)]
        claude_session: Option<SessionId>,
    },
    Claude { claude_session: SessionId },
}

/// What the frontend asks a tab to run. Mirrored by `TabLaunch` in src/main.ts.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TabLaunch {
    pub tab_id: TabId,
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
    /// Start in `$HOME` instead of failing when `cwd` no longer exists.
    #[serde(default)]
    pub fallback_to_home: bool,
    #[serde(flatten)]
    pub kind: RunningKind,
}

/// A started tab. Mirrored by `Spawned` in src/main.ts.
#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Spawned {
    id: u32,
    /// The requested directory was gone, so the tab started in `$HOME`.
    fell_back_to_home: bool,
    /// A Claude tab whose session hook reports to the database.
    tracked: bool,
    /// When the tab started (unix seconds), for `claude_reported`.
    started_at: u64,
    /// A Claude tab started with `--dangerously-skip-permissions`, so the UI can mark it.
    skips_permissions: bool,
    /// For a terminal tab started with a saved Claude session: what became of it.
    resume: Option<ResumeReport>,
}

/// Serialised as `code` in `CommandError`, so failures can be told apart without
/// parsing `message`. The frontend currently shows `message` only.
#[derive(Serialize, Debug, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The requested directory no longer exists.
    CwdMissing,
    Failed,
}

/// Commands that can fail fail with this shape (`{ code, message }`).
#[derive(Serialize, Debug, PartialEq)]
pub struct CommandError {
    code: ErrorCode,
    message: String,
}

impl From<SpawnError> for CommandError {
    fn from(e: SpawnError) -> Self {
        let code = match e {
            SpawnError::CwdMissing(_) => ErrorCode::CwdMissing,
            SpawnError::Failed(_) => ErrorCode::Failed,
        };
        CommandError { code, message: e.to_string() }
    }
}

impl From<String> for CommandError {
    fn from(message: String) -> Self {
        CommandError { code: ErrorCode::Failed, message }
    }
}

/// Where a Claude tab's session hook runs from and reports to.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct HookTarget<'a> {
    pub exe: &'a Path,
    pub db: &'a Path,
}

/// What tabs find first on their PATH, installed at startup: the `claude` wrapper
/// of terminal tabs (see `claude::install_wrapper`) and the `misag` shim (see
/// `claude::install_cli`). Each is its directory, or why it could not be written.
pub struct Wrapper {
    pub claude: Result<PathBuf, String>,
    pub cli: Result<PathBuf, String>,
}

/// What a tab's PATH is made from (see `tab_path`).
#[derive(Default)]
pub struct TabDirs<'a> {
    /// The `claude` wrapper's directory, for terminal tabs.
    pub wrapper: Option<&'a Path>,
    /// The `misag` shim's directory.
    pub cli: Option<&'a Path>,
    /// The PATH tabs start from: misag's own, or on macOS the login shell's.
    pub path: Option<std::ffi::OsString>,
    /// `path` is not misag's own, so tabs must be given it even when unchanged.
    pub replaced: bool,
    /// The wrapper and shim directories of a misag, or a raterm, misag was started from.
    pub inherited: Vec<std::ffi::OsString>,
}

impl<'a> TabDirs<'a> {
    /// `wrapper`'s directories, with misag's own environment.
    pub fn of(wrapper: &'a Wrapper) -> Self {
        let inherited = [claude::WRAPPER_ENV, claude::RATERM_WRAPPER_ENV, claude::CLI_ENV];
        #[cfg(unix)]
        let login = crate::login_path::path();
        #[cfg(not(unix))]
        let login: Option<String> = None;
        TabDirs {
            wrapper: wrapper.claude.as_deref().ok(),
            cli: wrapper.cli.as_deref().ok(),
            replaced: login.is_some(),
            path: login.map(Into::into).or_else(|| std::env::var_os("PATH")),
            inherited: inherited.into_iter().filter_map(std::env::var_os).collect(),
        }
    }
}

/// Where a terminal tab's `claude` reports to: the tab and the database.
#[derive(Debug, PartialEq)]
pub struct TerminalHook<'a> {
    pub tab_id: &'a TabId,
    pub db: &'a Path,
}

/// What becomes of the Claude session a restored terminal tab had open.
#[derive(Debug, PartialEq)]
pub enum Resume {
    /// Typed at the shell's prompt (see `resume_input`).
    Session(SessionId),
    /// Claude saved no conversation for it, so there is nothing to reopen.
    NoConversation,
    /// Whether it has a conversation could not be told; the error says why.
    Unknown(String),
}

/// How a tab starts, decided before anything runs.
#[derive(Debug, PartialEq)]
pub enum LaunchPlan<'a> {
    Terminal {
        command: Option<&'a str>,
        /// None when there is no database to report to.
        hook: Option<TerminalHook<'a>>,
        /// For a login shell that had a Claude session open when it was saved.
        resume: Option<Resume>,
    },
    Claude {
        tab_id: &'a TabId,
        session: SessionId,
        /// None when there is no database to report to: the session is then not tracked.
        hook: Option<HookTarget<'a>>,
        /// Resume the session's conversation rather than start a new one under its id.
        resume: bool,
        /// The settings as of this launch.
        settings: Settings,
    },
}

type Env = Vec<(String, String)>;

/// Decides how `tab` starts: for a Claude tab, which session (the one its hook last
/// reported, so a restarted tab returns to where `/clear` or `/resume` took it),
/// whether to resume it, and whether its hook can report anywhere. A terminal tab
/// running a login shell (no command) resumes its saved Claude session if Claude
/// has a conversation for it.
pub fn launch_plan<'a>(tab: &'a TabLaunch, exe: &'a AppExe, db: &'a Db) -> Result<LaunchPlan<'a>, CommandError> {
    let launched = match &tab.kind {
        RunningKind::Terminal { command, claude_session } => {
            let hook = db.hook_path().map(|db| TerminalHook { tab_id: &tab.tab_id, db });
            let resume = claude_session.as_ref().filter(|_| command.is_none()).map(|session| {
                let check = db.transcript_for(session).and_then(|reported| {
                    claude::should_resume(session, reported.as_deref(), claude::projects_dir().as_deref())
                });
                match check {
                    Ok(true) => Resume::Session(session.clone()),
                    Ok(false) => Resume::NoConversation,
                    Err(e) => Resume::Unknown(format!("cannot tell whether it has a conversation to resume: {e}")),
                }
            });
            return Ok(LaunchPlan::Terminal { command: command.as_deref(), hook, resume });
        }
        RunningKind::Claude { claude_session } => claude_session,
    };
    let exe = exe.path().map_err(|e| format!("cannot start Claude: {e}"))?;
    let session = db.reported_session(&tab.tab_id)?.unwrap_or_else(|| launched.clone());
    let reported = db.transcript_for(&session)?;
    let projects = claude::projects_dir();
    let resume = claude::should_resume(&session, reported.as_deref(), projects.as_deref())
        .map_err(|e| format!("cannot tell whether Claude session {session} has a conversation to resume: {e}"))?;
    let hook = db.hook_path().map(|db| HookTarget { exe, db });
    // An unreadable setting never blocks a tab; it starts with the cautious settings,
    // and the startup notice and the Settings tab show the error.
    let settings = db.load_settings().unwrap_or_else(|e| {
        eprintln!("misag: cannot read the settings ({e}); starting Claude with permission prompts and auto memory off");
        Settings::cautious()
    });
    Ok(LaunchPlan::Claude { tab_id: &tab.tab_id, session, hook, resume, settings })
}

/// The hook environment: the tab the hook reports for and the database it reports
/// to. `Db` only keeps a UTF-8 path, so nothing is lost converting it.
fn hook_env(tab_id: &TabId, db: &Path) -> Env {
    vec![
        (claude::TAB_ENV.to_string(), tab_id.to_string()),
        (claude::DB_ENV.to_string(), db.to_string_lossy().into_owned()),
        // A flow run sends one prompt after another from the Stop hook; Claude Code
        // otherwise stops honouring the hook after 8 in a row. runs::MAX_RUN_STEPS
        // is misag's own limit.
        (claude::STOP_CAP_ENV.to_string(), claude::STOP_CAP.to_string()),
    ]
}

/// Terminal tabs run their command (a blank one is a login shell). Claude tabs run
/// Claude Code pinned to their session, preceded by the arguments the settings add.
/// With a database, both get the hook environment that lets `misag --claude-hook`
/// record session changes for the tab (`plan_spawn` adds the wrapper's PATH).
pub fn launch_for(plan: &LaunchPlan) -> (Launch, Env) {
    match plan {
        LaunchPlan::Terminal { command, hook, .. } => {
            let launch = match command {
                Some(c) => Launch::Command((*c).to_owned()),
                None => Launch::LoginShell,
            };
            (launch, hook.as_ref().map_or_else(Vec::new, |h| hook_env(h.tab_id, h.db)))
        }
        LaunchPlan::Claude { tab_id, session, hook, resume, settings } => {
            let mut args = settings.claude_args();
            args.extend(claude::args(session, *resume, hook.map(|h| h.exe), settings.claude_settings()));
            let env = hook.map_or_else(Vec::new, |h| hook_env(tab_id, h.db));
            (Launch::Program { program: "claude".into(), args }, env)
        }
    }
}

/// What separates directories in PATH.
pub const PATH_SEPARATOR: char = if cfg!(windows) { ';' } else { ':' };

/// The PATH for a tab: misag's own `path` without `inherited` (the wrapper and
/// shim directories of a misag, or a raterm, this one was started from, whose hook
/// would report to that app's database) and with `first` first, in order. None when
/// that is `path` unchanged, or when `path` is not UTF-8, which is then left alone
/// rather than mangled (the wrapper and shim are not used).
pub fn tab_path(path: Option<&OsStr>, inherited: &[&OsStr], first: &[&Path]) -> Option<String> {
    let path = path.unwrap_or_default().to_str()?;
    let first: Vec<&str> = first.iter().filter_map(|d| d.to_str()).collect();
    let skip = |dir: &str| {
        let dir = Path::new(dir);
        inherited.iter().any(|d| Path::new(d) == dir) || first.iter().any(|f| Path::new(f) == dir)
    };
    let kept = path.split(PATH_SEPARATOR).filter(|dir| !path.is_empty() && !skip(dir));
    let joined: Vec<&str> = first.iter().copied().chain(kept).collect();
    let joined = joined.join(&PATH_SEPARATOR.to_string());
    (joined != path).then_some(joined)
}

/// What a terminal tab's shell is given to type to reopen `session`.
pub fn resume_input(session: &SessionId) -> String {
    // A `SessionId` is a canonical UUID, so it cannot carry shell syntax or keys.
    format!("claude --resume {session}\r")
}

/// Everything `pty_spawn` decides, without running anything: what to start, in
/// which directory, what to type at its prompt, and what to tell the frontend.
pub struct SpawnPlan {
    pub request: SpawnRequest,
    pub fell_back_to_home: bool,
    pub tracked: bool,
    pub skips_permissions: bool,
    /// The saved Claude session of a terminal tab, typed at its prompt once it
    /// starts (the shell reads it as if the user had typed it), or why not.
    pub resume: Option<Resume>,
}

/// Plans `tab`'s start, with its PATH made from `dirs`: when the tab's Claude can
/// report somewhere, a terminal tab finds the `claude` wrapper first, and every tab
/// then finds the `misag` shim.
pub fn plan_spawn(tab: &TabLaunch, exe: &AppExe, db: &Db, dirs: &TabDirs) -> Result<SpawnPlan, CommandError> {
    let plan = launch_plan(tab, exe, db)?;
    let tracked = matches!(plan, LaunchPlan::Claude { hook: Some(_), .. });
    let skips_permissions = matches!(&plan, LaunchPlan::Claude { settings, .. } if settings.skip_permissions);
    let reports = matches!(plan, LaunchPlan::Terminal { hook: Some(_), .. } | LaunchPlan::Claude { hook: Some(_), .. });
    let cli = dirs.cli.filter(|_| reports);
    let wrapper = dirs.wrapper.filter(|_| matches!(plan, LaunchPlan::Terminal { hook: Some(_), .. }));
    let first: Vec<&Path> = wrapper.into_iter().chain(cli).collect();
    let (launch, mut env) = launch_for(&plan);
    let inherited: Vec<&OsStr> = dirs.inherited.iter().map(|d| d.as_os_str()).collect();
    let given = || dirs.path.as_ref().filter(|_| dirs.replaced).and_then(|p| p.to_str()).map(str::to_owned);
    if let Some(path) = tab_path(dirs.path.as_deref(), &inherited, &first).or_else(given) {
        // Only claimed once they are really on PATH, so a misag started here knows what to drop.
        let on_path = |dir: Option<&Path>| dir.and_then(Path::to_str).filter(|d| path.split(PATH_SEPARATOR).any(|p| p == *d)).map(str::to_owned);
        if let Some(dir) = on_path(wrapper) {
            env.push((claude::WRAPPER_ENV.to_string(), dir));
        }
        if let Some(dir) = on_path(cli) {
            env.push((claude::CLI_ENV.to_string(), dir));
        }
        env.push(("PATH".to_string(), path));
    }
    let resume = match plan {
        LaunchPlan::Terminal { resume, .. } => resume,
        LaunchPlan::Claude { .. } => None,
    };
    let (cwd, fell_back_to_home) = match tab.fallback_to_home {
        true => session::cwd_or_home(tab.cwd.clone())?,
        false => (tab.cwd.clone(), false),
    };
    let request = SpawnRequest { cols: tab.cols, rows: tab.rows, cwd, launch, env };
    Ok(SpawnPlan { request, fell_back_to_home, tracked, skips_permissions, resume })
}

/// What became of a terminal tab's saved Claude session. Mirrored by
/// `ResumeReport` in src/main.ts.
#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum ResumeReport {
    /// `claude --resume` was typed at the prompt.
    Typed,
    /// Claude saved no conversation for it.
    NoConversation,
    /// It could not be resumed now; it may still be later.
    Failed { message: String },
}

/// Starts a tab's process; see `plan_spawn`.
#[tauri::command(async)]
pub fn pty_spawn(
    state: State<'_, Sessions>,
    db: State<'_, Db>,
    exe: State<'_, AppExe>,
    wrapper: State<'_, Wrapper>,
    request: TabLaunch,
    on_data: Channel<Response>,
    on_exit: Channel<i32>,
) -> Result<Spawned, CommandError> {
    let plan = plan_spawn(&request, &exe, &db, &TabDirs::of(&wrapper))?;
    let started_at = crate::db::unix_secs(SystemTime::now());
    let id = state.spawn(
        plan.request,
        move |bytes| on_data.send(Response::new(bytes.to_vec())).is_ok(),
        move |code| {
            if on_exit.send(code).is_err() {
                eprintln!("misag: exit notification not delivered");
            }
        },
    )?;
    let SpawnPlan { fell_back_to_home, tracked, skips_permissions, resume, .. } = plan;
    let resume = resume.map(|resume| match resume {
        Resume::Session(session) => match state.write(id, resume_input(&session).into_bytes()) {
            Ok(()) => ResumeReport::Typed,
            Err(e) => ResumeReport::Failed { message: format!("could not type the command: {e}") },
        },
        Resume::NoConversation => ResumeReport::NoConversation,
        Resume::Unknown(message) => ResumeReport::Failed { message },
    });
    Ok(Spawned { id, fell_back_to_home, tracked, skips_permissions, started_at, resume })
}

#[tauri::command]
pub fn pty_write(state: State<'_, Sessions>, id: u32, data: Vec<u8>) -> Result<(), CommandError> {
    Ok(state.write(id, data)?)
}

#[tauri::command]
pub fn pty_resize(state: State<'_, Sessions>, id: u32, cols: u16, rows: u16) -> Result<(), CommandError> {
    Ok(state.resize(id, cols, rows)?)
}

#[tauri::command]
pub fn pty_kill(state: State<'_, Sessions>, id: u32) {
    state.kill(id);
}

/// Kills sessions left over from a previous page load (e.g. a dev reload).
#[tauri::command]
pub fn pty_reset(state: State<'_, Sessions>) {
    state.kill_all();
}

/// A live session and its tab, for `claude_open_sessions`. The frontend sends
/// these as `{ id, tabId }` objects.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TabSession {
    id: u32,
    tab_id: TabId,
}

/// Whether a terminal tab has a Claude session open, and whether that Claude runs
/// tools without asking (see `settings::skips_permissions`). Mirrored by
/// `ClaudeState` in src/main.ts.
#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ClaudeState {
    /// The Claude process that last reported for the tab is in the foreground.
    Open { session: SessionId, skips_permissions: bool },
    /// Something else is: another program, or just the shell.
    Closed,
    /// A `claude` that never reported for the tab is: it bypassed the wrapper.
    Untracked { pid: u32, skips_permissions: bool },
    /// It could not be told, so whatever was known before still stands.
    Unknown,
}

/// The state of each tab, keyed by PTY id: `foreground` gives the programs that may
/// run in a session's foreground, most likely first (one on Unix; on Windows every
/// `claude` or `node` below the shell, see `proc_tree`), `run_by` the session a Claude
/// process reported for a tab, `name` a process's name, and `skips` whether a Claude
/// process skips permission prompts (None when its command line cannot be read).
pub fn claude_states(
    tabs: Vec<TabSession>,
    foreground: impl Fn(u32) -> Result<Vec<u32>, String>,
    run_by: impl Fn(&TabId, u32) -> Result<Option<SessionId>, String>,
    name: impl Fn(u32) -> Option<String>,
    skips: impl Fn(u32) -> Option<bool>,
) -> HashMap<u32, ClaudeState> {
    let state = |t: &TabSession| {
        // A session that is gone or cannot be asked is not known to be idle.
        let Ok(candidates) = foreground(t.id) else { return ClaudeState::Unknown };
        let mut untracked = None;
        for pid in candidates {
            match run_by(&t.tab_id, pid) {
                // A Claude that may skip permission prompts is not known to be safe, so
                // without its command line the earlier warning stands.
                Ok(Some(session)) => {
                    return match skips(pid) {
                        Some(skips_permissions) => ClaudeState::Open { session, skips_permissions },
                        None => ClaudeState::Unknown,
                    };
                }
                Ok(None) => {
                    if untracked.is_none() && name(pid).as_deref() == Some("claude") {
                        untracked = Some(pid);
                    }
                }
                Err(e) => {
                    eprintln!("misag: could not look up the Claude session in tab {}: {e}", t.tab_id);
                    return ClaudeState::Unknown;
                }
            }
        }
        match untracked {
            Some(pid) => match skips(pid) {
                Some(skips_permissions) => ClaudeState::Untracked { pid, skips_permissions },
                None => ClaudeState::Unknown,
            },
            None => ClaudeState::Closed,
        }
    };
    tabs.iter().map(|t| (t.id, state(t))).collect()
}

/// Whether each given terminal tab has a Claude session open; see `claude_states`.
#[tauri::command(async)]
pub fn claude_open_sessions(
    state: State<'_, Sessions>,
    db: State<'_, Db>,
    tabs: Vec<TabSession>,
) -> HashMap<u32, ClaudeState> {
    let skips = |pid| Sessions::process_args(pid).map(|args| skips_permissions(&args));
    claude_states(tabs, |id| state.foreground(id), |tab, pid| db.session_run_by(tab, pid), Sessions::process_name, skips)
}

/// When (unix ms) each tab's Claude ended its last turn waiting on background
/// tasks, by tab id; tabs waiting on none are left out.
#[tauri::command(async)]
pub fn background_tabs(db: State<'_, Db>) -> Result<HashMap<String, i64>, CommandError> {
    Ok(db.background_tabs()?)
}

/// Current working directory of each live session, keyed by id.
#[tauri::command(async)]
pub fn pty_cwds(state: State<'_, Sessions>, ids: Vec<u32>) -> HashMap<u32, String> {
    ids.into_iter().filter_map(|id| Some((id, state.cwd(id)?))).collect()
}

#[tauri::command(async)]
pub fn settings_load(db: State<'_, Db>) -> Result<Settings, CommandError> {
    Ok(db.load_settings()?)
}

/// Checks the settings against the stored ones, stores them and returns them as stored.
#[tauri::command(async)]
pub fn settings_save(db: State<'_, Db>, settings: Settings) -> Result<Settings, CommandError> {
    let stored = db.load_settings()?;
    Ok(db.save_settings(settings.validated(&stored)?)?)
}

#[tauri::command(async)]
pub fn flows_list(db: State<'_, Db>) -> Result<Vec<FlowSummary>, CommandError> {
    Ok(db.list_flows()?)
}

#[tauri::command(async)]
pub fn flow_create(db: State<'_, Db>, name: String) -> Result<FlowSummary, CommandError> {
    Ok(db.create_flow(&name)?)
}

#[tauri::command(async)]
pub fn flow_rename(db: State<'_, Db>, id: i64, name: String) -> Result<(), CommandError> {
    Ok(db.rename_flow(id, &name)?)
}

/// The person has seen a flow a program in a tab added, and lets it run.
#[tauri::command(async)]
pub fn flow_confirm(db: State<'_, Db>, id: i64) -> Result<(), CommandError> {
    Ok(db.confirm_flow(id)?)
}

#[tauri::command(async)]
pub fn flow_set_archived(db: State<'_, Db>, id: i64, archived: bool) -> Result<(), CommandError> {
    Ok(db.set_flow_archived(id, archived)?)
}

#[tauri::command(async)]
pub fn flows_reorder(db: State<'_, Db>, ids: Vec<i64>) -> Result<(), CommandError> {
    Ok(db.reorder_flows(&ids)?)
}

/// A flow with its steps, for its editor tab. Mirrored by `FlowDetail` in src/flowEditor.ts.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FlowDetail {
    #[serde(flatten)]
    flow: FlowSummary,
    graph: Graph,
    /// What keeps it from running; see `flows::problems`.
    problems: Vec<String>,
}

#[tauri::command(async)]
pub fn flow_load(db: State<'_, Db>, id: i64) -> Result<FlowDetail, CommandError> {
    let (flow, graph) = db.load_flow(id)?;
    let problems = flows::problems(&graph);
    Ok(FlowDetail { flow, graph, problems })
}

/// The primary selection's text (what is selected anywhere on the desktop), for a
/// middle-click paste in a terminal; None if there is none. WebKitGTK 2.54 crashes
/// the app pasting it itself, so the page asks for it here: through GTK, on the main
/// thread, where GTK's clipboard lives.
#[tauri::command(async)]
pub fn primary_selection(app: tauri::AppHandle) -> Result<Option<String>, CommandError> {
    #[cfg(target_os = "linux")]
    return Ok(gtk_text(&app, Selection::Primary)?);
    // Only X11 and Wayland desktops have one.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        Ok(None)
    }
}

/// The clipboard's text, for a right-click paste in a terminal; None if there is none.
/// WebKit's menu over a terminal has no Paste, and reading it through GTK spares the
/// page the webview's clipboard permission.
#[tauri::command(async)]
pub fn clipboard_text(app: tauri::AppHandle) -> Result<Option<String>, CommandError> {
    #[cfg(target_os = "linux")]
    return Ok(gtk_text(&app, Selection::Clipboard)?);
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        Ok(pasteboard_text()?)
    }
    #[cfg(windows)]
    {
        let _ = app;
        Ok(crate::win::clipboard_text()?)
    }
}

/// The general pasteboard's text, through `pbpaste`, which every macOS has.
#[cfg(target_os = "macos")]
fn pasteboard_text() -> Result<Option<String>, String> {
    let mut pbpaste = std::process::Command::new("/usr/bin/pbpaste");
    // Started from Finder, misag has no locale, and pbpaste then mangles non-ASCII text.
    if std::env::var_os("LANG").is_none() {
        pbpaste.env("LANG", "en_US.UTF-8");
    }
    let out = pbpaste.output().map_err(|e| format!("cannot run pbpaste: {e}"))?;
    if !out.status.success() {
        return Err(format!("pbpaste failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok((!text.is_empty()).then_some(text))
}

/// Where `gtk_text` reads text from.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum Selection {
    /// What is selected anywhere on the desktop.
    Primary,
    /// What was copied.
    Clipboard,
}

#[cfg(target_os = "linux")]
impl Selection {
    fn name(self) -> &'static str {
        match self {
            Selection::Primary => "the primary selection",
            Selection::Clipboard => "the clipboard",
        }
    }
}

/// The text of `selection`, read through GTK on the main thread.
#[cfg(target_os = "linux")]
fn gtk_text(app: &tauri::AppHandle, selection: Selection) -> Result<Option<String>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.run_on_main_thread(move || {
        // An Atom cannot cross threads, so it is picked here.
        let atom = match selection {
            Selection::Primary => gtk::gdk::SELECTION_PRIMARY,
            Selection::Clipboard => gtk::gdk::SELECTION_CLIPBOARD,
        };
        gtk::Clipboard::get(&atom).request_text(move |_, text| {
            let _ = tx.send(text.map(str::to_owned));
        });
    })
    .map_err(|e| format!("cannot reach the main thread: {e}"))?;
    rx.recv_timeout(std::time::Duration::from_secs(2)).map_err(|e| match e {
        // The selection's owner may be another app that never answers.
        std::sync::mpsc::RecvTimeoutError::Timeout => format!("{} did not arrive within 2 seconds", selection.name()),
        std::sync::mpsc::RecvTimeoutError::Disconnected => format!("GTK dropped the request for {}", selection.name()),
    })
}

const FLOW_FILES: (&str, &[&str]) = ("Flow", &["json"]);

/// A file name for a flow called `name`: without the characters some file systems
/// refuse or read as folders, nor trailing dots and spaces.
pub fn export_file_name(name: &str) -> String {
    let safe: String = name.chars().map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '-' } else { c }).collect();
    let safe = safe.trim_end_matches(['.', ' ']).trim_start_matches('.');
    format!("{}.json", if safe.is_empty() { "flow" } else { safe })
}

/// Adds the flow in the file at `path` (see `flow_file`) after the others.
pub fn import_from(db: &Db, path: &Path) -> Result<FlowSummary, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let (name, graph) = crate::flow_file::parse(&text)?;
    db.import_flow(&name, &graph)
}

/// Writes flow `id` to `path` as a file `import_from` and `misag flow import` read.
pub fn export_to(db: &Db, id: i64, path: &Path) -> Result<(), String> {
    let (flow, graph) = db.load_flow(id)?;
    let file = crate::flow_file::write(&flow.name, &graph) + "\n";
    std::fs::write(path, file).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn picked(path: Option<tauri_plugin_dialog::FilePath>) -> Result<Option<PathBuf>, String> {
    path.map(|p| p.into_path().map_err(|e| format!("the file picked cannot be used: {e}"))).transpose()
}

/// Asks the person for a flow file and adds it; None if they cancel. The dialog is
/// opened here, so the webview never names a file to read.
#[tauri::command(async)]
pub fn flow_import_file(window: tauri::Window, db: State<'_, Db>) -> Result<Option<FlowSummary>, CommandError> {
    use tauri_plugin_dialog::DialogExt;
    let (filter, extensions) = FLOW_FILES;
    let path = window.dialog().file().set_parent(&window).set_title("Import a flow").add_filter(filter, extensions).blocking_pick_file();
    let Some(path) = picked(path)? else { return Ok(None) };
    Ok(Some(import_from(&db, &path)?))
}

/// Asks the person where to save flow `id`, and saves it there; false if they cancel.
#[tauri::command(async)]
pub fn flow_export_file(window: tauri::Window, db: State<'_, Db>, id: i64) -> Result<bool, CommandError> {
    use tauri_plugin_dialog::DialogExt;
    let (flow, _) = db.load_flow(id)?;
    let (filter, extensions) = FLOW_FILES;
    let path = window
        .dialog()
        .file()
        .set_parent(&window)
        .set_title(format!("Export {}", flow.name))
        .set_file_name(export_file_name(&flow.name))
        .add_filter(filter, extensions)
        .blocking_save_file();
    let Some(path) = picked(path)? else { return Ok(false) };
    export_to(&db, id, &path)?;
    Ok(true)
}

/// Stores a flow's steps and returns what keeps them from running.
#[tauri::command(async)]
pub fn flow_save_graph(db: State<'_, Db>, id: i64, graph: Graph) -> Result<Vec<String>, CommandError> {
    db.save_flow_graph(id, &graph)?;
    Ok(flows::problems(&graph))
}

#[tauri::command(async)]
pub fn flow_start(db: State<'_, Db>, flow_id: i64, tab_id: TabId) -> Result<crate::runs::Started, CommandError> {
    Ok(db.start_run(flow_id, &tab_id)?)
}

/// How a run that cleared at its start is getting on (see `runs::cleared`).
#[tauri::command(async)]
pub fn flow_cleared(db: State<'_, Db>, run_id: i64) -> Result<crate::runs::Cleared, CommandError> {
    Ok(db.cleared_run(run_id)?)
}

/// The text to put into Claude's input for the run's manual prompt, if the app owes it (see `runs::prefill_text`).
#[tauri::command(async)]
pub fn flow_prefill_text(db: State<'_, Db>, run_id: i64, step_id: String) -> Result<Option<crate::runs::Prefill>, CommandError> {
    Ok(db.prefill_text(run_id, &step_id)?)
}

/// The app put the run's manual prompt into Claude's input (see `runs::prefilled`).
#[tauri::command(async)]
pub fn flow_prefilled(db: State<'_, Db>, run_id: i64, step_id: String, token: i64) -> Result<(), CommandError> {
    Ok(db.prefilled(run_id, &step_id, token)?)
}

#[tauri::command(async)]
pub fn flow_continue(db: State<'_, Db>, run_id: i64, step_id: String, output: String) -> Result<Option<String>, CommandError> {
    Ok(db.proceed_run(run_id, &step_id, &output)?)
}

/// Has Claude go on with the run's step that needs attention; returns the text to send.
#[tauri::command(async)]
pub fn flow_resume(db: State<'_, Db>, run_id: i64, step_id: String) -> Result<String, CommandError> {
    Ok(db.resume_run(run_id, &step_id)?)
}

#[tauri::command(async)]
pub fn flow_stop(db: State<'_, Db>, run_id: i64) -> Result<(), CommandError> {
    Ok(db.stop_run(run_id)?)
}

/// Ends the tab's run, if any, as the tab is closing.
#[tauri::command(async)]
pub fn flow_stop_tab(db: State<'_, Db>, tab_id: TabId) -> Result<(), CommandError> {
    Ok(db.stop_tab_run(&tab_id)?)
}

#[tauri::command(async)]
pub fn flow_runs(db: State<'_, Db>) -> Result<Vec<crate::runs::Run>, CommandError> {
    Ok(db.active_runs()?)
}

#[tauri::command(async)]
pub fn flow_delete(db: State<'_, Db>, id: i64) -> Result<(), CommandError> {
    Ok(db.delete_flow(id)?)
}

/// One level of the sidebar's file tree.
#[tauri::command(async)]
pub fn fs_list(dir: String) -> Result<crate::files::Listing, CommandError> {
    Ok(crate::files::list(&dir)?)
}

/// The pinned conversations Claude saved, then the newest `limit` others, for the
/// Session history box. Pins whose transcript is gone are dropped.
#[tauri::command(async)]
pub fn claude_history(
    cache: State<'_, crate::history::Cache>,
    db: State<'_, Db>,
    limit: usize,
) -> Result<crate::history::Page, CommandError> {
    let Some(projects) = claude::projects_dir() else {
        return Err("Claude's config folder is unknown: neither CLAUDE_CONFIG_DIR nor your home folder is set".to_string().into());
    };
    let page = cache.list(&projects, limit, &db.pinned_sessions()?)?;
    for id in &page.gone_pins {
        // The listing stands without it; the next one tries again.
        if let Err(e) = db.set_session_pinned(id, false) {
            db.log_once(format!("could not unpin {}, whose transcript is gone: {e}", id.as_str()));
        }
    }
    Ok(page)
}

/// Pins a Claude session to the top of the Session history box, or unpins it.
#[tauri::command(async)]
pub fn claude_session_pin(db: State<'_, Db>, id: SessionId, pinned: bool) -> Result<(), CommandError> {
    Ok(db.set_session_pinned(&id, pinned)?)
}

#[tauri::command(async)]
pub fn layout_load(db: State<'_, Db>) -> Result<Layout, CommandError> {
    Ok(db.load_layout()?)
}

#[tauri::command(async)]
pub fn layout_save(db: State<'_, Db>, layout: Layout) -> Result<(), CommandError> {
    Ok(db.save_layout(layout)?)
}

/// Saving state for the UI. Mirrored by `StoreReport` in src/main.ts.
#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StoreReport {
    store: StoreStatus,
    /// The last failure the Claude session hook logged during this run, unless it
    /// has worked since.
    hook_problem: Option<HookProblem>,
    /// Why the `claude` wrapper could not be installed at startup, if it could not.
    wrapper_problem: Option<String>,
    /// Why the `misag` shim could not be installed at startup, if it could not.
    cli_problem: Option<String>,
}

#[tauri::command(async)]
pub fn store_status(db: State<'_, Db>, wrapper: State<'_, Wrapper>) -> StoreReport {
    // Without a database there is nothing to report to anyway, which `store` says.
    let problem = |installed: &Result<PathBuf, String>| installed.as_ref().err().filter(|_| db.hook_path().is_some()).cloned();
    StoreReport {
        store: db.status(),
        hook_problem: claude::hook_problem(&db),
        wrapper_problem: problem(&wrapper.claude),
        cli_problem: problem(&wrapper.cli),
    }
}

/// Whether a Claude tab's session hook has reported since the tab started
/// (`since`, from `Spawned::started_at`), so the UI can warn when session changes
/// are not being tracked.
#[tauri::command(async)]
pub fn claude_reported(db: State<'_, Db>, tab_id: TabId, since: u64) -> Result<bool, CommandError> {
    Ok(db.claude_reported(&tab_id, since)?)
}

/// The Claude session each tab's hook last reported, by tab id, so the Session
/// history box can tell which sessions are open in a tab.
#[tauri::command(async)]
pub fn claude_tab_sessions(db: State<'_, Db>) -> Result<HashMap<String, SessionId>, CommandError> {
    Ok(db.reported_sessions()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{temp_db, tid};
    use crate::db::{connect_existing, record_claude_session, SessionReport};

    const ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const LATER: &str = "1b4e28ba-2fa1-41d2-883f-0016d3cca427";
    const EXE: &str = "/bin/misag";

    #[test]
    fn a_loaded_flow_is_one_flat_object() {
        let flow = FlowSummary { id: 3, name: "Deploy".into(), archived: false, clear: false, unconfirmed: false };
        let detail = FlowDetail { flow, graph: Graph::default(), problems: vec!["x".into()] };
        let json = serde_json::to_value(detail).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["archived", "graph", "id", "name", "problems"]);
    }

    fn session() -> SessionId {
        SessionId::parse(ID).unwrap()
    }

    fn tab(kind: RunningKind) -> TabLaunch {
        TabLaunch { tab_id: tid("tab-1"), cols: 80, rows: 24, cwd: None, fallback_to_home: false, kind }
    }

    fn claude_tab() -> TabLaunch {
        tab(RunningKind::Claude { claude_session: session() })
    }

    fn terminal_tab() -> TabLaunch {
        tab(RunningKind::Terminal { command: None, claude_session: None })
    }

    fn exe() -> AppExe {
        AppExe::new(Ok(EXE.into()))
    }

    fn report<'a>(tab_id: &'a TabId, session_id: &'a SessionId, transcript_path: Option<&'a str>) -> SessionReport<'a> {
        SessionReport { transcript_path, ..crate::db::tests::report(tab_id, session_id) }
    }

    #[test]
    fn terminal_tabs_run_their_command_or_a_login_shell() {
        let run = |command: Option<&str>| launch_for(&LaunchPlan::Terminal { command, hook: None, resume: None });
        assert_eq!(run(Some("htop")), (Launch::Command("htop".into()), vec![]));
        assert_eq!(run(None), (Launch::LoginShell, vec![]));
    }

    #[test]
    fn tabs_get_a_path_other_than_misags_own_even_when_unchanged() {
        let memory = Db::open_or_recover(Err("no data dir".into()));
        let (request, exe) = (terminal_tab(), exe());
        let env = |dirs: &TabDirs| plan_spawn(&request, &exe, &memory, dirs).unwrap().request.env;
        let own = TabDirs { path: Some("/usr/bin".into()), ..Default::default() };
        assert!(!env(&own).iter().any(|(k, _)| k == "PATH"), "inherited as is");
        let login = TabDirs { path: Some("/login/bin".into()), replaced: true, ..Default::default() };
        assert!(env(&login).contains(&("PATH".to_string(), "/login/bin".to_string())));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_tabs_find_the_claude_wrapper_first_on_their_path() {
        let path = temp_db("terminal-hook");
        let db = Db::open(path.clone()).unwrap();
        let (request, exe) = (terminal_tab(), exe());
        let (wrapper, cli) = (claude::wrapper_dir(&path).unwrap(), claude::cli_dir(&path).unwrap());
        let dirs = TabDirs { wrapper: Some(&wrapper), cli: Some(&cli), path: Some("/usr/bin".into()), ..Default::default() };
        let plan = plan_spawn(&request, &exe, &db, &dirs).unwrap();
        assert_eq!(plan.request.launch, Launch::LoginShell);
        let env: HashMap<_, _> = plan.request.env.into_iter().collect();
        assert_eq!(env["MISAG_TAB"], "tab-1");
        assert_eq!(env["MISAG_DB"], path.to_str().unwrap());
        assert_eq!(env["MISAG_WRAPPER"], wrapper.to_str().unwrap());
        // Then `misag` itself, for Claude and the person (see claude::install_cli).
        assert_eq!(env["MISAG_CLI"], cli.to_str().unwrap());
        assert!(env["PATH"].starts_with(&format!("{}:{}:", wrapper.display(), cli.display())), "{}", env["PATH"]);

        // Without an installed wrapper the hook environment is still set, but PATH is not changed for it.
        let plan = plan_spawn(&request, &exe, &db, &TabDirs::default()).unwrap();
        let env: HashMap<_, _> = plan.request.env.into_iter().collect();
        assert!(env.contains_key("MISAG_TAB") && !env.contains_key("MISAG_WRAPPER"));
        // Without the wrapper, `misag` is still found.
        let plan = plan_spawn(&request, &exe, &db, &TabDirs { wrapper: None, ..dirs }).unwrap();
        let env: HashMap<_, _> = plan.request.env.into_iter().collect();
        assert_eq!(env["PATH"], format!("{}:/usr/bin", cli.display()));
        assert!(!env.contains_key("MISAG_WRAPPER"));
        let dirs = TabDirs { wrapper: Some(&wrapper), cli: Some(&cli), path: Some("/usr/bin".into()), ..Default::default() };
        let memory = Db::open_or_recover(Err("no data dir".into()));
        let plan = plan_spawn(&request, &exe, &memory, &dirs).unwrap();
        assert!(!plan.request.env.iter().any(|(k, _)| k.starts_with("MISAG")), "nothing to report to");
    }

    #[cfg(unix)]
    #[test]
    fn claude_tabs_find_this_misag_first_on_their_path() {
        let path = temp_db("claude-cli");
        let db = Db::open(path.clone()).unwrap();
        let (wrapper, cli) = (claude::wrapper_dir(&path).unwrap(), claude::cli_dir(&path).unwrap());
        // Started from another misag's tab, whose shim is on PATH: it goes.
        let dirs = TabDirs {
            wrapper: Some(&wrapper),
            cli: Some(&cli),
            path: Some("/o/cli:/usr/bin".into()),
            inherited: vec!["/o/cli".into()],
            replaced: false,
        };
        let plan = plan_spawn(&claude_tab(), &exe(), &db, &dirs).unwrap();
        let env: HashMap<_, _> = plan.request.env.into_iter().collect();
        assert_eq!(env["MISAG_CLI"], cli.to_str().unwrap());
        assert!(!env.contains_key("MISAG_WRAPPER"), "Claude tabs start claude themselves");
        assert_eq!(env["PATH"], format!("{}:/usr/bin", cli.display()));
    }

    #[test]
    fn tab_paths_use_the_platforms_separator() {
        let sep = PATH_SEPARATOR;
        let first = [Path::new("/d/bin")];
        let path = format!("/a{sep}/b");
        assert_eq!(tab_path(Some(std::ffi::OsStr::new(&path)), &[], &first), Some(format!("/d/bin{sep}/a{sep}/b")));
        let already = format!("/d/bin{sep}/a");
        assert_eq!(tab_path(Some(std::ffi::OsStr::new(&already)), &[], &first), None);
    }

    #[cfg(unix)]
    #[test]
    fn tab_paths_put_the_given_directories_first_in_order() {
        let first = [Path::new("/d/bin"), Path::new("/d/cli")];
        assert_eq!(tab_path(Some(std::ffi::OsStr::new("/d/cli:/usr/bin")), &[], &first).as_deref(), Some("/d/bin:/d/cli:/usr/bin"));
        assert_eq!(tab_path(Some(std::ffi::OsStr::new("/d/bin:/d/cli:/usr/bin")), &[], &first), None, "already first");
    }

    #[cfg(unix)]
    #[test]
    fn tab_paths_put_the_wrapper_first_and_drop_an_outer_misag_wrapper() {
        let os = |s: &str| std::ffi::OsString::from(s);
        let w = Some(Path::new("/d/bin"));
        let path = |p: &str, inherited: Option<&str>, wrapper: Option<&Path>| {
            let inherited: Vec<_> = inherited.map(os).into_iter().collect();
            let inherited: Vec<&std::ffi::OsStr> = inherited.iter().map(|d| d.as_os_str()).collect();
            tab_path(Some(os(p).as_os_str()), &inherited, wrapper.as_slice())
        };
        assert_eq!(path("/usr/bin:/bin", None, w).as_deref(), Some("/d/bin:/usr/bin:/bin"));
        assert_eq!(path("/usr/bin", None, None), None, "unchanged");
        // Started from another misag's tab: its wrapper goes, wherever it is and however it is spelt.
        assert_eq!(path("/o/bin/:/usr/bin", Some("/o/bin"), w).as_deref(), Some("/d/bin:/usr/bin"));
        assert_eq!(path("/o/bin:/usr/bin", Some("/o/bin"), None).as_deref(), Some("/usr/bin"));
        // Started from a raterm tab inside a misag tab: both wrappers go.
        let both = [os("/o/bin"), os("/r/bin")];
        let both: Vec<&std::ffi::OsStr> = both.iter().map(|d| d.as_os_str()).collect();
        assert_eq!(tab_path(Some(os("/r/bin:/o/bin:/usr/bin").as_os_str()), &both, w.as_slice()).as_deref(), Some("/d/bin:/usr/bin"));
        // Not listed twice, and empty entries (the current directory) are kept.
        assert_eq!(path("/d/bin:/usr/bin::", None, w), None, "already first");
        assert_eq!(path("/usr/bin:/d/bin", None, w).as_deref(), Some("/d/bin:/usr/bin"));
        assert_eq!(path("", None, w).as_deref(), Some("/d/bin"));
        assert_eq!(tab_path(None, &[], w.as_slice()).as_deref(), Some("/d/bin"));
        // A PATH that is not UTF-8 is left alone rather than mangled.
        #[cfg(unix)]
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(tab_path(Some(std::ffi::OsStr::from_bytes(b"/\xff:/bin")), &[], w.as_slice()), None);
    }

    #[test]
    fn a_flow_file_name_is_one_any_file_system_takes() {
        assert_eq!(export_file_name("Deploy: v2"), "Deploy- v2.json");
        assert_eq!(export_file_name("a/b\\c\u{1b}d"), "a-b-c-d.json");
        assert_eq!(export_file_name("Fix it. "), "Fix it.json");
        assert_eq!(export_file_name(".."), "flow.json");
    }

    #[test]
    fn a_flow_exported_to_a_file_imports_as_the_same_flow() {
        let db = Db::open(temp_db("export-file")).unwrap();
        let flow = db.import_flow("Deploy", &crate::flows::tests::runnable()).unwrap();
        let file = temp_db("export-file").with_file_name("deploy.json");
        export_to(&db, flow.id, &file).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().ends_with("}\n"));
        let again = import_from(&db, &file).unwrap();
        assert_eq!((again.name.as_str(), db.load_flow(again.id).unwrap().1), ("Deploy", db.load_flow(flow.id).unwrap().1));
        let missing = file.with_file_name("missing.json");
        assert!(import_from(&db, &missing).unwrap_err().starts_with("cannot read "), "says which file");
    }

    #[test]
    fn a_terminal_tab_resumes_its_claude_session_at_the_prompt() {
        let path = temp_db("terminal-resume");
        let db = Db::open(path.clone()).unwrap();
        let shell = tab(RunningKind::Terminal { command: None, claude_session: Some(session()) });
        let resume = |t: &TabLaunch| plan_spawn(t, &exe(), &db, &TabDirs::default()).unwrap().resume;
        assert_eq!(resume(&shell), Some(Resume::NoConversation));

        let transcript = path.with_extension("jsonl");
        std::fs::write(&transcript, "{}").unwrap();
        record_claude_session(&connect_existing(&path).unwrap(), &report(&shell.tab_id, &session(), transcript.to_str()))
            .unwrap();
        assert_eq!(resume(&shell), Some(Resume::Session(session())));
        assert_eq!(resume_input(&session()), format!("claude --resume {ID}\r"));
        let plan = plan_spawn(&shell, &exe(), &db, &TabDirs::default()).unwrap();
        assert_eq!(plan.request.launch, Launch::LoginShell);
        assert!(!plan.tracked, "the hook watch is for Claude tabs");

        // A tab running a command has no prompt to type at, and a tab without a session has nothing to resume.
        let command = tab(RunningKind::Terminal { command: Some("htop".into()), claude_session: Some(session()) });
        assert_eq!(resume(&command), None);
        assert_eq!(resume(&terminal_tab()), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_resume_that_cannot_be_checked_is_unknown_not_dropped() {
        if crate::test_support::running_as_root() {
            return;
        }
        let path = temp_db("terminal-resume-unknown");
        let db = Db::open(path.clone()).unwrap();
        let shell = tab(RunningKind::Terminal { command: None, claude_session: Some(session()) });
        // A reported transcript in a directory that cannot be searched.
        let locked = path.parent().unwrap().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let transcript = locked.join("t.jsonl");
        record_claude_session(&connect_existing(&path).unwrap(), &report(&shell.tab_id, &session(), transcript.to_str()))
            .unwrap();
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let resume = plan_spawn(&shell, &exe(), &db, &TabDirs::default()).unwrap().resume;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(resume, Some(Resume::Unknown(m)) if m.contains("cannot check")));
    }

    #[test]
    fn claude_states_tell_open_closed_untracked_and_unknown_apart() {
        let tabs = |ids: &[u32]| ids.iter().map(|&id| TabSession { id, tab_id: tid(&format!("t{id}")) }).collect();
        // Tab 1 runs the Claude that reported (pid 10); tab 2 an idle shell; tab 3 vim
        // (pid 30); tab 4 a claude that never reported (pid 40); tab 5 cannot be asked;
        // tab 6's lookup fails; tab 7 runs the Claude that reported (pid 70), whose
        // command line cannot be read.
        // Tab 8 is a Windows tab running an npm Claude (node, pid 81, reported) under
        // the wrapper (claude.exe, pid 80); tab 9 the wrapper over a node that never
        // reported, which counts as an untracked claude.
        let foreground = |id: u32| match id {
            1 => Ok(vec![10]),
            7 => Ok(vec![70]),
            2 => Ok(vec![]),
            3 => Ok(vec![30]),
            4 => Ok(vec![40]),
            6 => Ok(vec![60]),
            8 => Ok(vec![81, 80]),
            9 => Ok(vec![91, 90]),
            _ => Err("no session".to_string()),
        };
        let run_by = |tab: &TabId, pid: u32| match (tab.as_str(), pid) {
            ("t1", 10) | ("t7", 70) | ("t8", 81) => Ok(Some(session())),
            ("t6", _) => Err("database is locked".to_string()),
            _ => Ok(None),
        };
        let name = |pid: u32| {
            Some(match pid {
                30 => "vim",
                81 | 91 => "node",
                _ => "claude",
            }
            .to_string())
        };
        // Only the untracked claude was started with --dangerously-skip-permissions.
        let skips = |pid: u32| if pid == 70 { None } else { Some(pid == 40) };
        let states = claude_states(tabs(&[1, 2, 3, 4, 5, 6, 7, 8, 9]), foreground, run_by, name, skips);
        assert_eq!(states[&8], ClaudeState::Open { session: session(), skips_permissions: false });
        assert_eq!(states[&9], ClaudeState::Untracked { pid: 90, skips_permissions: false });
        assert_eq!(states[&1], ClaudeState::Open { session: session(), skips_permissions: false });
        assert_eq!(states[&2], ClaudeState::Closed);
        assert_eq!(states[&3], ClaudeState::Closed);
        assert_eq!(states[&4], ClaudeState::Untracked { pid: 40, skips_permissions: true });
        assert_eq!(states[&5], ClaudeState::Unknown);
        assert_eq!(states[&6], ClaudeState::Unknown);
        assert_eq!(states[&7], ClaudeState::Unknown, "it might skip permissions: not known to be safe");
        let json = serde_json::to_value(&states[&1]).unwrap();
        assert_eq!(json, serde_json::json!({ "state": "open", "session": ID, "skipsPermissions": false }));
        let json = serde_json::to_value(&states[&4]).unwrap();
        assert_eq!(json, serde_json::json!({ "state": "untracked", "pid": 40, "skipsPermissions": true }));
    }

    #[test]
    fn resume_reports_have_one_shape() {
        assert_eq!(serde_json::to_value(ResumeReport::Typed).unwrap(), serde_json::json!({ "state": "typed" }));
        let failed = ResumeReport::Failed { message: "x".into() };
        assert_eq!(serde_json::to_value(failed).unwrap(), serde_json::json!({ "state": "failed", "message": "x" }));
    }

    #[test]
    fn claude_tabs_run_claude_with_the_hook_environment() {
        let tab_id = tid("tab-1");
        let hook = Some(HookTarget { exe: Path::new(EXE), db: Path::new("/data/misag.db") });
        let plan = LaunchPlan::Claude { tab_id: &tab_id, session: session(), hook, resume: false, settings: Settings::default() };
        let (launch, env) = launch_for(&plan);
        let Launch::Program { program, args } = launch else { panic!("expected a program") };
        assert_eq!(program, "claude");
        assert_eq!(args, claude::args(&session(), false, Some(Path::new(EXE)), Default::default()));
        assert_eq!(
            env,
            [
                ("MISAG_TAB".into(), "tab-1".into()),
                ("MISAG_DB".into(), "/data/misag.db".into()),
                // A flow sends prompt after prompt from the Stop hook; Claude Code's own
                // cap on hook continuations (8) would cut it short.
                ("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP".into(), "51".into()),
            ]
        );
    }

    #[test]
    fn without_a_database_claude_runs_without_the_hook() {
        let tab_id = tid("tab-1");
        let (launch, env) = launch_for(&LaunchPlan::Claude { tab_id: &tab_id, session: session(), hook: None, resume: false, settings: Settings::default() });
        let Launch::Program { args, .. } = launch else { panic!("expected a program") };
        assert_eq!(args, ["--session-id", ID]);
        assert!(env.is_empty());
    }

    #[test]
    fn the_plan_resumes_reported_sessions_and_knows_the_hook_target() {
        let path = temp_db("plan");
        let db = Db::open(path.clone()).unwrap();
        let exe = exe();
        let request = claude_tab();
        let hook = Some(HookTarget { exe: Path::new(EXE), db: path.as_path() });
        assert_eq!(
            launch_plan(&request, &exe, &db).unwrap(),
            LaunchPlan::Claude { tab_id: &request.tab_id, session: session(), hook, resume: false, settings: Settings::default() }
        );

        let transcript = path.with_extension("jsonl");
        std::fs::write(&transcript, "{}").unwrap();
        record_claude_session(&connect_existing(&path).unwrap(), &report(&request.tab_id, &session(), transcript.to_str()))
            .unwrap();
        assert!(matches!(launch_plan(&request, &exe, &db).unwrap(), LaunchPlan::Claude { resume: true, .. }));
    }

    #[test]
    fn claude_tabs_get_the_arguments_the_settings_add() {
        let db = Db::open(temp_db("plan-args")).unwrap();
        let request = claude_tab();
        let exe = exe();
        assert!(!plan_spawn(&request, &exe, &db, &TabDirs::default()).unwrap().skips_permissions);

        db.save_settings(Settings { skip_permissions: true, ..Settings::default() }).unwrap();
        let plan = launch_plan(&request, &exe, &db).unwrap();
        let (Launch::Program { args, .. }, _) = launch_for(&plan) else { panic!("expected a program") };
        let mut expected = vec!["--dangerously-skip-permissions".to_string()];
        expected.extend(claude::args(&session(), false, db.hook_path().map(|_| Path::new(EXE)), Default::default()));
        assert_eq!(args, expected);
        assert!(plan_spawn(&request, &exe, &db, &TabDirs::default()).unwrap().skips_permissions);
        assert!(!plan_spawn(&terminal_tab(), &exe, &db, &TabDirs::default()).unwrap().skips_permissions, "only Claude tabs");
    }

    #[test]
    fn disabling_auto_memory_reaches_the_settings_json_next_to_the_hook() {
        let db = Db::open(temp_db("plan-auto-memory")).unwrap();
        let request = claude_tab();
        let exe = exe();
        let settings_json = |db: &Db| {
            let (Launch::Program { args, .. }, _) = launch_for(&launch_plan(&request, &exe, db).unwrap()) else {
                panic!("expected a program")
            };
            let i = args.iter().position(|a| a == "--settings").expect("the hook's --settings");
            serde_json::from_str::<serde_json::Value>(&args[i + 1]).unwrap()
        };
        let auto = settings_json(&db);
        assert!(auto.get("autoMemoryEnabled").is_none(), "auto leaves it to the user's config");
        assert!(auto.get("hooks").is_some());

        db.save_settings(Settings { disable_auto_memory: true, ..Settings::default() }).unwrap();
        let disabled = settings_json(&db);
        assert_eq!(disabled["autoMemoryEnabled"], false);
        assert_eq!(disabled["hooks"], auto["hooks"]);
    }

    #[test]
    fn unreadable_settings_start_claude_cautiously() {
        let path = temp_db("plan-bad-settings");
        let db = Db::open(path.clone()).unwrap();
        db.save_settings(Settings { skip_permissions: true, ..Settings::default() }).unwrap();
        connect_existing(&path).unwrap().execute("DELETE FROM settings", []).unwrap();
        let request = claude_tab();
        let LaunchPlan::Claude { settings, .. } = launch_plan(&request, &exe(), &db).unwrap() else { panic!() };
        assert_eq!(settings, Settings::cautious());
        assert!(!settings.skip_permissions && settings.disable_auto_memory);
    }

    #[test]
    fn a_restarted_tab_goes_back_to_the_session_its_hook_reported() {
        let path = temp_db("plan-restart");
        let db = Db::open(path.clone()).unwrap();
        let request = claude_tab();
        let later = SessionId::parse(LATER).unwrap();
        // The tab ran /clear: the frontend still holds the launch-time id.
        record_claude_session(&connect_existing(&path).unwrap(), &report(&request.tab_id, &later, None)).unwrap();
        let LaunchPlan::Claude { session, .. } = launch_plan(&request, &exe(), &db).unwrap() else { panic!() };
        assert_eq!(session, later);
    }

    #[test]
    fn the_plan_needs_the_binary() {
        let db = Db::open(temp_db("plan-errors")).unwrap();
        let err = launch_plan(&claude_tab(), &AppExe::new(Err("gone".into())), &db).err().unwrap();
        assert!(err.message.contains("cannot start Claude: gone"));
        // A terminal tab does not need misag's binary.
        assert!(launch_plan(&terminal_tab(), &AppExe::new(Err("gone".into())), &db).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn the_binary_path_must_be_utf8() {
        #[cfg(unix)]
        use std::os::unix::ffi::OsStrExt;
        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/opt/\xff/misag"));
        assert!(AppExe::new(Ok(bad)).path().unwrap_err().contains("not valid UTF-8"));
        assert_eq!(exe().path(), Ok(Path::new(EXE)));
    }

    #[test]
    fn spawn_plans_fall_back_to_home_only_when_asked() {
        let db = Db::open(temp_db("spawn-plan")).unwrap();
        let gone = Some("/definitely/not/here".to_string());
        // Without the fallback the directory is kept, and starting there fails visibly.
        let strict = TabLaunch { cwd: gone.clone(), ..terminal_tab() };
        let plan = plan_spawn(&strict, &exe(), &db, &TabDirs::default()).unwrap();
        assert_eq!((plan.request.cwd.as_deref(), plan.fell_back_to_home), (gone.as_deref(), false));
        let err: CommandError = Sessions::default().spawn(plan.request, |_| true, |_| {}).unwrap_err().into();
        assert_eq!(err.code, ErrorCode::CwdMissing);

        let lenient = TabLaunch { cwd: gone, fallback_to_home: true, ..terminal_tab() };
        let plan = plan_spawn(&lenient, &exe(), &db, &TabDirs::default()).unwrap();
        assert!(plan.fell_back_to_home);
        assert_eq!(plan.request.cwd, None);
        assert!(!plan.tracked, "terminal tabs are not tracked");
    }

    #[test]
    fn claude_spawn_plans_are_tracked_only_with_a_database() {
        let db = Db::open(temp_db("spawn-tracked")).unwrap();
        assert!(plan_spawn(&claude_tab(), &exe(), &db, &TabDirs::default()).unwrap().tracked);
        let memory = Db::open_or_recover(Err("no data dir".into()));
        let plan = plan_spawn(&claude_tab(), &exe(), &memory, &TabDirs::default()).unwrap();
        assert!(!plan.tracked);
        assert!(plan.request.env.is_empty());
    }

    #[test]
    fn reads_the_request_the_frontend_sends() {
        let raw = r#"{"kind":"claude","tabId":"t1","cols":120,"rows":40,"cwd":null,
                      "claudeSession":"0f8fad5b-d9cb-469f-a165-70867728950e"}"#;
        let t: TabLaunch = serde_json::from_str(raw).unwrap();
        assert_eq!((t.tab_id.as_str(), t.cols), ("t1", 120));
        assert_eq!(t.kind, RunningKind::Claude { claude_session: session() });
        assert!(!t.fallback_to_home, "off unless asked for");
        let terminal = r#"{"kind":"terminal","tabId":"t2","cols":80,"rows":24,"cwd":"/tmp","command":"htop","fallbackToHome":true}"#;
        let resuming = r#"{"kind":"terminal","tabId":"t3","cols":80,"rows":24,"cwd":null,"command":null,
                           "claudeSession":"0f8fad5b-d9cb-469f-a165-70867728950e"}"#;
        let t: TabLaunch = serde_json::from_str(resuming).unwrap();
        assert_eq!(t.kind, RunningKind::Terminal { command: None, claude_session: Some(session()) });
        let t: TabLaunch = serde_json::from_str(terminal).unwrap();
        assert_eq!(t.kind, RunningKind::Terminal { command: Some("htop".into()), claude_session: None });
        assert!(t.fallback_to_home);
    }

    #[test]
    fn refuses_requests_that_cannot_start() {
        let no_session = r#"{"kind":"claude","tabId":"t1","cols":80,"rows":24,"cwd":null,"claudeSession":null}"#;
        assert!(serde_json::from_str::<TabLaunch>(no_session).is_err(), "a running Claude tab has a session");
        let bad_id = r#"{"kind":"terminal","tabId":"a\nb","cols":80,"rows":24,"cwd":null,"command":null}"#;
        assert!(serde_json::from_str::<TabLaunch>(bad_id).is_err());
    }

    #[test]
    fn errors_have_one_shape_with_a_code() {
        let missing = serde_json::to_value(CommandError::from(SpawnError::CwdMissing("/gone".into()))).unwrap();
        assert_eq!(missing["code"], "cwd_missing");
        assert_eq!(missing["message"], "directory does not exist: /gone");
        let other = serde_json::to_value(CommandError::from("no pty".to_string())).unwrap();
        assert_eq!(other["code"], "failed");
    }

    #[test]
    fn the_store_report_carries_the_status_and_hook_problems() {
        let report = StoreReport {
            store: StoreStatus::NotSaving { reason: "x".into() },
            hook_problem: Some(HookProblem { at: 7, message: "y".into(), log: "/d/hook.log".into() }),
            wrapper_problem: Some("read-only".into()),
            cli_problem: Some("full".into()),
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["store"]["state"], "notSaving");
        assert_eq!(json["store"]["reason"], "x");
        assert_eq!(json["hookProblem"], serde_json::json!({ "at": 7, "message": "y", "log": "/d/hook.log" }));
        assert_eq!(json["wrapperProblem"], "read-only");
        assert_eq!(json["cliProblem"], "full");
    }
}
