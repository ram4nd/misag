//! Tauri commands over `session::Sessions` and `db::Db`. The ones that touch the
//! database or the filesystem run off the main thread (`#[tauri::command(async)]`):
//! the database may wait up to its busy timeout for the hook's write lock.

use crate::claude::{self, HookProblem, SessionId};
use crate::db::{Db, StoreStatus};
use crate::layout::{Layout, TabId};
use crate::session::{self, Launch, SpawnError, Sessions, SpawnRequest};
use crate::settings::Settings;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tauri::ipc::{Channel, Response};
use tauri::State;

/// raterm's own binary, resolved once at startup (see `session::live_path`); also
/// given to `Sessions` as its trampoline. Its path is written into shell command
/// lines, so holding an `Ok` one proves it is valid UTF-8.
pub struct AppExe(Result<PathBuf, String>);

impl AppExe {
    pub fn new(exe: Result<PathBuf, String>) -> Self {
        Self(exe.and_then(|exe| match exe.to_str() {
            Some(_) => Ok(exe),
            None => Err(format!("raterm's path {exe:?} is not valid UTF-8")),
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

/// The `claude` wrapper terminal tabs find first on their PATH: its directory, or
/// why it could not be written at startup (see `claude::install_wrapper`).
pub struct Wrapper(pub Result<PathBuf, String>);

impl Wrapper {
    pub fn dir(&self) -> Option<&Path> {
        self.0.as_deref().ok()
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
    // The defaults are the cautious choice (Claude asks before running tools), so an
    // unreadable setting never blocks a tab; the Settings tab shows the error.
    let settings = db.load_settings().unwrap_or_else(|e| {
        eprintln!("raterm: {e}; starting Claude with the default settings");
        Settings::default()
    });
    Ok(LaunchPlan::Claude { tab_id: &tab.tab_id, session, hook, resume, settings })
}

/// The hook environment: the tab the hook reports for and the database it reports
/// to. `Db` only keeps a UTF-8 path, so nothing is lost converting it.
fn hook_env(tab_id: &TabId, db: &Path) -> Env {
    vec![
        (claude::TAB_ENV.to_string(), tab_id.to_string()),
        (claude::DB_ENV.to_string(), db.to_string_lossy().into_owned()),
    ]
}

/// Terminal tabs run their command (a blank one is a login shell). Claude tabs run
/// Claude Code pinned to their session, preceded by the arguments the settings add.
/// With a database, both get the hook environment that lets `raterm --claude-hook`
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
            args.extend(claude::args(session, *resume, hook.map(|h| h.exe)));
            let env = hook.map_or_else(Vec::new, |h| hook_env(tab_id, h.db));
            (Launch::Program { program: "claude".into(), args }, env)
        }
    }
}

/// The PATH for a tab: raterm's own `path` without `inherited` (the wrapper
/// directory of a raterm this one was started from, whose hook would report to
/// that raterm's database) and with `wrapper` first. None when that is `path`
/// unchanged, or when `path` is not UTF-8, which is then left alone rather than
/// mangled (the wrapper is not used).
pub fn tab_path(path: Option<&OsStr>, inherited: Option<&OsStr>, wrapper: Option<&Path>) -> Option<String> {
    let path = path.unwrap_or_default().to_str()?;
    let wrapper = wrapper.and_then(Path::to_str);
    let skip = |dir: &str| [inherited.map(Path::new), wrapper.map(Path::new)].contains(&Some(Path::new(dir)));
    let kept = path.split(':').filter(|dir| !path.is_empty() && !skip(dir));
    let joined: Vec<&str> = wrapper.into_iter().chain(kept).collect();
    let joined = joined.join(":");
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

/// Plans `tab`'s start. `wrapper` is the installed `claude` wrapper's directory,
/// put first on a terminal tab's PATH when its Claude can report somewhere.
pub fn plan_spawn(tab: &TabLaunch, exe: &AppExe, db: &Db, wrapper: Option<&Path>) -> Result<SpawnPlan, CommandError> {
    let plan = launch_plan(tab, exe, db)?;
    let tracked = matches!(plan, LaunchPlan::Claude { hook: Some(_), .. });
    let skips_permissions = matches!(&plan, LaunchPlan::Claude { settings, .. } if settings.skip_permissions);
    let wrapper = wrapper.filter(|_| matches!(plan, LaunchPlan::Terminal { hook: Some(_), .. }));
    let (launch, mut env) = launch_for(&plan);
    let inherited = std::env::var_os(claude::WRAPPER_ENV);
    if let Some(path) = tab_path(std::env::var_os("PATH").as_deref(), inherited.as_deref(), wrapper) {
        // Only claimed once it is really on PATH, so a raterm started here knows what to drop.
        if let Some(dir) = wrapper.and_then(Path::to_str).filter(|dir| path.starts_with(dir)) {
            env.push((claude::WRAPPER_ENV.to_string(), dir.to_owned()));
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
    let plan = plan_spawn(&request, &exe, &db, wrapper.dir())?;
    let started_at = crate::db::unix_secs(SystemTime::now());
    let id = state.spawn(
        plan.request,
        move |bytes| on_data.send(Response::new(bytes.to_vec())).is_ok(),
        move |code| {
            if on_exit.send(code).is_err() {
                eprintln!("raterm: exit notification not delivered");
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

/// Whether a terminal tab has a Claude session open. Mirrored by `ClaudeState` in
/// src/main.ts.
#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ClaudeState {
    /// The Claude process that last reported for the tab is in the foreground.
    Open { session: SessionId },
    /// Something else is: another program, or just the shell.
    Closed,
    /// A `claude` that never reported for the tab is: it bypassed the wrapper.
    Untracked { pid: u32 },
    /// It could not be told, so whatever was known before still stands.
    Unknown,
}

/// The state of each tab, keyed by PTY id: `foreground` gives a session's
/// foreground program, `run_by` the session a Claude process reported for a tab,
/// and `name` a process's name.
pub fn claude_states(
    tabs: Vec<TabSession>,
    foreground: impl Fn(u32) -> Result<Option<u32>, String>,
    run_by: impl Fn(&TabId, u32) -> Result<Option<SessionId>, String>,
    name: impl Fn(u32) -> Option<String>,
) -> HashMap<u32, ClaudeState> {
    let state = |t: &TabSession| {
        // A session that is gone or cannot be asked is not known to be idle.
        let Ok(foreground) = foreground(t.id) else { return ClaudeState::Unknown };
        let Some(pid) = foreground else { return ClaudeState::Closed };
        match run_by(&t.tab_id, pid) {
            Ok(Some(session)) => ClaudeState::Open { session },
            Ok(None) if name(pid).as_deref() == Some("claude") => ClaudeState::Untracked { pid },
            Ok(None) => ClaudeState::Closed,
            Err(e) => {
                eprintln!("raterm: could not look up the Claude session in tab {}: {e}", t.tab_id);
                ClaudeState::Unknown
            }
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
    claude_states(tabs, |id| state.foreground(id), |tab, pid| db.session_run_by(tab, pid), Sessions::process_name)
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

/// One level of the sidebar's file tree.
#[tauri::command(async)]
pub fn fs_list(dir: String) -> Result<crate::files::Listing, CommandError> {
    Ok(crate::files::list(&dir)?)
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
}

#[tauri::command(async)]
pub fn store_status(db: State<'_, Db>, wrapper: State<'_, Wrapper>) -> StoreReport {
    // Without a database there is nothing to report to anyway, which `store` says.
    let wrapper_problem = wrapper.0.as_ref().err().filter(|_| db.hook_path().is_some()).cloned();
    StoreReport { store: db.status(), hook_problem: claude::hook_problem(&db), wrapper_problem }
}

/// Whether a Claude tab's session hook has reported since the tab started
/// (`since`, from `Spawned::started_at`), so the UI can warn when session changes
/// are not being tracked.
#[tauri::command(async)]
pub fn claude_reported(db: State<'_, Db>, tab_id: TabId, since: u64) -> Result<bool, CommandError> {
    Ok(db.claude_reported(&tab_id, since)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{temp_db, tid};
    use crate::db::{connect_existing, record_claude_session, SessionReport};

    const ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const LATER: &str = "1b4e28ba-2fa1-41d2-883f-0016d3cca427";
    const EXE: &str = "/bin/raterm";

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
    fn terminal_tabs_find_the_claude_wrapper_first_on_their_path() {
        let path = temp_db("terminal-hook");
        let db = Db::open(path.clone()).unwrap();
        let (request, exe) = (terminal_tab(), exe());
        let wrapper = claude::wrapper_dir(&path).unwrap();
        let plan = plan_spawn(&request, &exe, &db, Some(&wrapper)).unwrap();
        assert_eq!(plan.request.launch, Launch::LoginShell);
        let env: HashMap<_, _> = plan.request.env.into_iter().collect();
        assert_eq!(env["RATERM_TAB"], "tab-1");
        assert_eq!(env["RATERM_DB"], path.to_str().unwrap());
        assert_eq!(env["RATERM_WRAPPER"], wrapper.to_str().unwrap());
        assert!(env["PATH"].starts_with(&format!("{}:", wrapper.display())), "{}", env["PATH"]);

        // Without an installed wrapper the hook environment is still set, but PATH is not changed for it.
        let plan = plan_spawn(&request, &exe, &db, None).unwrap();
        let env: HashMap<_, _> = plan.request.env.into_iter().collect();
        assert!(env.contains_key("RATERM_TAB") && !env.contains_key("RATERM_WRAPPER"));
        let memory = Db::open_or_recover(Err("no data dir".into()));
        let plan = plan_spawn(&request, &exe, &memory, Some(&wrapper)).unwrap();
        assert!(!plan.request.env.iter().any(|(k, _)| k.starts_with("RATERM")), "nothing to report to");
    }

    #[test]
    fn tab_paths_put_the_wrapper_first_and_drop_an_outer_raterm_wrapper() {
        let os = |s: &str| std::ffi::OsString::from(s);
        let w = Some(Path::new("/d/bin"));
        let path = |p: &str, inherited: Option<&str>, wrapper| {
            tab_path(Some(os(p).as_os_str()), inherited.map(os).as_deref(), wrapper)
        };
        assert_eq!(path("/usr/bin:/bin", None, w).as_deref(), Some("/d/bin:/usr/bin:/bin"));
        assert_eq!(path("/usr/bin", None, None), None, "unchanged");
        // Started from another raterm's tab: its wrapper goes, wherever it is and however it is spelt.
        assert_eq!(path("/o/bin/:/usr/bin", Some("/o/bin"), w).as_deref(), Some("/d/bin:/usr/bin"));
        assert_eq!(path("/o/bin:/usr/bin", Some("/o/bin"), None).as_deref(), Some("/usr/bin"));
        // Not listed twice, and empty entries (the current directory) are kept.
        assert_eq!(path("/d/bin:/usr/bin::", None, w), None, "already first");
        assert_eq!(path("/usr/bin:/d/bin", None, w).as_deref(), Some("/d/bin:/usr/bin"));
        assert_eq!(path("", None, w).as_deref(), Some("/d/bin"));
        assert_eq!(tab_path(None, None, w).as_deref(), Some("/d/bin"));
        // A PATH that is not UTF-8 is left alone rather than mangled.
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(tab_path(Some(std::ffi::OsStr::from_bytes(b"/\xff:/bin")), None, w), None);
    }

    #[test]
    fn a_terminal_tab_resumes_its_claude_session_at_the_prompt() {
        let path = temp_db("terminal-resume");
        let db = Db::open(path.clone()).unwrap();
        let shell = tab(RunningKind::Terminal { command: None, claude_session: Some(session()) });
        let resume = |t: &TabLaunch| plan_spawn(t, &exe(), &db, None).unwrap().resume;
        assert_eq!(resume(&shell), Some(Resume::NoConversation));

        let transcript = path.with_extension("jsonl");
        std::fs::write(&transcript, "{}").unwrap();
        record_claude_session(&connect_existing(&path).unwrap(), &report(&shell.tab_id, &session(), transcript.to_str()))
            .unwrap();
        assert_eq!(resume(&shell), Some(Resume::Session(session())));
        assert_eq!(resume_input(&session()), format!("claude --resume {ID}\r"));
        let plan = plan_spawn(&shell, &exe(), &db, None).unwrap();
        assert_eq!(plan.request.launch, Launch::LoginShell);
        assert!(!plan.tracked, "the hook watch is for Claude tabs");

        // A tab running a command has no prompt to type at, and a tab without a session has nothing to resume.
        let command = tab(RunningKind::Terminal { command: Some("htop".into()), claude_session: Some(session()) });
        assert_eq!(resume(&command), None);
        assert_eq!(resume(&terminal_tab()), None);
    }

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
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let resume = plan_spawn(&shell, &exe(), &db, None).unwrap().resume;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(resume, Some(Resume::Unknown(m)) if m.contains("cannot check")));
    }

    #[test]
    fn claude_states_tell_open_closed_untracked_and_unknown_apart() {
        let tabs = |ids: &[u32]| ids.iter().map(|&id| TabSession { id, tab_id: tid(&format!("t{id}")) }).collect();
        // Tab 1 runs the Claude that reported (pid 10); tab 2 an idle shell; tab 3 vim
        // (pid 30); tab 4 a claude that never reported (pid 40); tab 5 cannot be asked;
        // tab 6's lookup fails.
        let foreground = |id: u32| match id {
            1 => Ok(Some(10)),
            2 => Ok(None),
            3 => Ok(Some(30)),
            4 => Ok(Some(40)),
            6 => Ok(Some(60)),
            _ => Err("no session".to_string()),
        };
        let run_by = |tab: &TabId, pid: u32| match (tab.as_str(), pid) {
            ("t1", 10) => Ok(Some(session())),
            ("t6", _) => Err("database is locked".to_string()),
            _ => Ok(None),
        };
        let name = |pid: u32| Some(if pid == 30 { "vim" } else { "claude" }.to_string());
        let states = claude_states(tabs(&[1, 2, 3, 4, 5, 6]), foreground, run_by, name);
        assert_eq!(states[&1], ClaudeState::Open { session: session() });
        assert_eq!(states[&2], ClaudeState::Closed);
        assert_eq!(states[&3], ClaudeState::Closed);
        assert_eq!(states[&4], ClaudeState::Untracked { pid: 40 });
        assert_eq!(states[&5], ClaudeState::Unknown);
        assert_eq!(states[&6], ClaudeState::Unknown);
        let json = serde_json::to_value(&states[&1]).unwrap();
        assert_eq!(json, serde_json::json!({ "state": "open", "session": ID }));
        assert_eq!(serde_json::to_value(&states[&4]).unwrap(), serde_json::json!({ "state": "untracked", "pid": 40 }));
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
        let hook = Some(HookTarget { exe: Path::new(EXE), db: Path::new("/data/raterm.db") });
        let plan = LaunchPlan::Claude { tab_id: &tab_id, session: session(), hook, resume: false, settings: Settings::default() };
        let (launch, env) = launch_for(&plan);
        let Launch::Program { program, args } = launch else { panic!("expected a program") };
        assert_eq!(program, "claude");
        assert_eq!(args, claude::args(&session(), false, Some(Path::new(EXE))));
        assert_eq!(
            env,
            [("RATERM_TAB".into(), "tab-1".into()), ("RATERM_DB".into(), "/data/raterm.db".into())]
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
        assert!(!plan_spawn(&request, &exe, &db, None).unwrap().skips_permissions);

        db.save_settings(Settings { skip_permissions: true, ..Settings::default() }).unwrap();
        let plan = launch_plan(&request, &exe, &db).unwrap();
        let (Launch::Program { args, .. }, _) = launch_for(&plan) else { panic!("expected a program") };
        let mut expected = vec!["--dangerously-skip-permissions".to_string()];
        expected.extend(claude::args(&session(), false, db.hook_path().map(|_| Path::new(EXE))));
        assert_eq!(args, expected);
        assert!(plan_spawn(&request, &exe, &db, None).unwrap().skips_permissions);
        assert!(!plan_spawn(&terminal_tab(), &exe, &db, None).unwrap().skips_permissions, "only Claude tabs");
    }

    #[test]
    fn unreadable_settings_start_claude_with_the_defaults() {
        let path = temp_db("plan-bad-settings");
        let db = Db::open(path.clone()).unwrap();
        db.save_settings(Settings { skip_permissions: true, ..Settings::default() }).unwrap();
        connect_existing(&path).unwrap().execute("DELETE FROM settings", []).unwrap();
        let request = claude_tab();
        let LaunchPlan::Claude { settings, .. } = launch_plan(&request, &exe(), &db).unwrap() else { panic!() };
        assert_eq!(settings, Settings::default());
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
        // A terminal tab does not need raterm's binary.
        assert!(launch_plan(&terminal_tab(), &AppExe::new(Err("gone".into())), &db).is_ok());
    }

    #[test]
    fn the_binary_path_must_be_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/opt/\xff/raterm"));
        assert!(AppExe::new(Ok(bad)).path().unwrap_err().contains("not valid UTF-8"));
        assert_eq!(exe().path(), Ok(Path::new(EXE)));
    }

    #[test]
    fn spawn_plans_fall_back_to_home_only_when_asked() {
        let db = Db::open(temp_db("spawn-plan")).unwrap();
        let gone = Some("/definitely/not/here".to_string());
        // Without the fallback the directory is kept, and starting there fails visibly.
        let strict = TabLaunch { cwd: gone.clone(), ..terminal_tab() };
        let plan = plan_spawn(&strict, &exe(), &db, None).unwrap();
        assert_eq!((plan.request.cwd.as_deref(), plan.fell_back_to_home), (gone.as_deref(), false));
        let err: CommandError = Sessions::default().spawn(plan.request, |_| true, |_| {}).unwrap_err().into();
        assert_eq!(err.code, ErrorCode::CwdMissing);

        let lenient = TabLaunch { cwd: gone, fallback_to_home: true, ..terminal_tab() };
        let plan = plan_spawn(&lenient, &exe(), &db, None).unwrap();
        assert!(plan.fell_back_to_home);
        assert_eq!(plan.request.cwd, None);
        assert!(!plan.tracked, "terminal tabs are not tracked");
    }

    #[test]
    fn claude_spawn_plans_are_tracked_only_with_a_database() {
        let db = Db::open(temp_db("spawn-tracked")).unwrap();
        assert!(plan_spawn(&claude_tab(), &exe(), &db, None).unwrap().tracked);
        let memory = Db::open_or_recover(Err("no data dir".into()));
        let plan = plan_spawn(&claude_tab(), &exe(), &memory, None).unwrap();
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
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["store"]["state"], "notSaving");
        assert_eq!(json["store"]["reason"], "x");
        assert_eq!(json["hookProblem"], serde_json::json!({ "at": 7, "message": "y", "log": "/d/hook.log" }));
        assert_eq!(json["wrapperProblem"], "read-only");
    }
}
