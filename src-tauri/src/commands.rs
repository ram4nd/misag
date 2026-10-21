//! Tauri commands over `session::Sessions` and `db::Db`. The ones that touch the
//! database or the filesystem run off the main thread (`#[tauri::command(async)]`):
//! the database may wait up to its busy timeout for the hook's write lock.

use crate::claude::{self, HookProblem, SessionId};
use crate::db::{Db, StoreStatus};
use crate::layout::{Layout, TabId};
use crate::session::{self, Launch, SpawnError, Sessions, SpawnRequest};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    Terminal { command: Option<String> },
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

/// How a tab starts, decided before anything runs.
#[derive(Debug, PartialEq)]
pub enum LaunchPlan<'a> {
    Terminal { command: Option<&'a str> },
    Claude {
        tab_id: &'a TabId,
        session: SessionId,
        /// None when there is no database to report to: the session is then not tracked.
        hook: Option<HookTarget<'a>>,
        /// Resume the session's conversation rather than start a new one under its id.
        resume: bool,
    },
}

type Env = Vec<(String, String)>;

/// Decides how `tab` starts: for a Claude tab, which session (the one its hook last
/// reported, so a restarted tab returns to where `/clear` or `/resume` took it),
/// whether to resume it, and whether its hook can report anywhere.
pub fn launch_plan<'a>(tab: &'a TabLaunch, exe: &'a AppExe, db: &'a Db) -> Result<LaunchPlan<'a>, CommandError> {
    let launched = match &tab.kind {
        RunningKind::Terminal { command } => return Ok(LaunchPlan::Terminal { command: command.as_deref() }),
        RunningKind::Claude { claude_session } => claude_session,
    };
    let exe = exe.path().map_err(|e| format!("cannot start Claude: {e}"))?;
    let session = db.reported_session(&tab.tab_id)?.unwrap_or_else(|| launched.clone());
    let reported = db.transcript_for(&session)?;
    let projects = claude::projects_dir();
    let resume = claude::should_resume(&session, reported.as_deref(), projects.as_deref())
        .map_err(|e| format!("cannot tell whether Claude session {session} has a conversation to resume: {e}"))?;
    let hook = db.hook_path().map(|db| HookTarget { exe, db });
    Ok(LaunchPlan::Claude { tab_id: &tab.tab_id, session, hook, resume })
}

/// Terminal tabs run their command (a blank one is a login shell). Claude tabs run
/// Claude Code pinned to their session, with the hook environment that lets
/// `raterm --claude-hook` record session changes for this tab.
pub fn launch_for(plan: &LaunchPlan) -> (Launch, Env) {
    match plan {
        LaunchPlan::Terminal { command: Some(c) } => (Launch::Command((*c).to_owned()), vec![]),
        LaunchPlan::Terminal { command: None } => (Launch::LoginShell, vec![]),
        LaunchPlan::Claude { tab_id, session, hook, resume } => {
            let args = claude::args(session, *resume, hook.map(|h| h.exe));
            let env = match hook {
                // `Db` only keeps a UTF-8 path, so nothing is lost here.
                Some(HookTarget { db, .. }) => vec![
                    (claude::TAB_ENV.to_string(), tab_id.to_string()),
                    (claude::DB_ENV.to_string(), db.to_string_lossy().into_owned()),
                ],
                None => vec![],
            };
            (Launch::Program { program: "claude".into(), args }, env)
        }
    }
}

/// Everything `pty_spawn` decides, without running anything: what to start, in
/// which directory, and what to tell the frontend about it.
pub struct SpawnPlan {
    pub request: SpawnRequest,
    pub fell_back_to_home: bool,
    pub tracked: bool,
}

pub fn plan_spawn(tab: &TabLaunch, exe: &AppExe, db: &Db) -> Result<SpawnPlan, CommandError> {
    let plan = launch_plan(tab, exe, db)?;
    let tracked = matches!(plan, LaunchPlan::Claude { hook: Some(_), .. });
    let (launch, env) = launch_for(&plan);
    let (cwd, fell_back_to_home) = match tab.fallback_to_home {
        true => session::cwd_or_home(tab.cwd.clone())?,
        false => (tab.cwd.clone(), false),
    };
    let request = SpawnRequest { cols: tab.cols, rows: tab.rows, cwd, launch, env };
    Ok(SpawnPlan { request, fell_back_to_home, tracked })
}

/// Starts a tab's process; see `plan_spawn`.
#[tauri::command(async)]
pub fn pty_spawn(
    state: State<'_, Sessions>,
    db: State<'_, Db>,
    exe: State<'_, AppExe>,
    request: TabLaunch,
    on_data: Channel<Response>,
    on_exit: Channel<i32>,
) -> Result<Spawned, CommandError> {
    let plan = plan_spawn(&request, &exe, &db)?;
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
    Ok(Spawned { id, fell_back_to_home: plan.fell_back_to_home, tracked: plan.tracked, started_at })
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

/// Current working directory of each live session, keyed by id.
#[tauri::command(async)]
pub fn pty_cwds(state: State<'_, Sessions>, ids: Vec<u32>) -> HashMap<u32, String> {
    ids.into_iter().filter_map(|id| Some((id, state.cwd(id)?))).collect()
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
}

#[tauri::command(async)]
pub fn store_status(db: State<'_, Db>) -> StoreReport {
    StoreReport { store: db.status(), hook_problem: claude::hook_problem(&db) }
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
        tab(RunningKind::Terminal { command: None })
    }

    fn exe() -> AppExe {
        AppExe::new(Ok(EXE.into()))
    }

    fn report<'a>(tab_id: &'a TabId, session_id: &'a SessionId, transcript_path: Option<&'a str>) -> SessionReport<'a> {
        SessionReport { transcript_path, ..crate::db::tests::report(tab_id, session_id) }
    }

    #[test]
    fn terminal_tabs_run_their_command_or_a_login_shell() {
        let run = |command: Option<&str>| launch_for(&LaunchPlan::Terminal { command });
        assert_eq!(run(Some("htop")), (Launch::Command("htop".into()), vec![]));
        assert_eq!(run(None), (Launch::LoginShell, vec![]));
    }

    #[test]
    fn claude_tabs_run_claude_with_the_hook_environment() {
        let tab_id = tid("tab-1");
        let hook = Some(HookTarget { exe: Path::new(EXE), db: Path::new("/data/raterm.db") });
        let plan = LaunchPlan::Claude { tab_id: &tab_id, session: session(), hook, resume: false };
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
        let (launch, env) = launch_for(&LaunchPlan::Claude { tab_id: &tab_id, session: session(), hook: None, resume: false });
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
            LaunchPlan::Claude { tab_id: &request.tab_id, session: session(), hook, resume: false }
        );

        let transcript = path.with_extension("jsonl");
        std::fs::write(&transcript, "{}").unwrap();
        record_claude_session(&connect_existing(&path).unwrap(), &report(&request.tab_id, &session(), transcript.to_str()))
            .unwrap();
        assert!(matches!(launch_plan(&request, &exe, &db).unwrap(), LaunchPlan::Claude { resume: true, .. }));
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
        let plan = plan_spawn(&strict, &exe(), &db).unwrap();
        assert_eq!((plan.request.cwd.as_deref(), plan.fell_back_to_home), (gone.as_deref(), false));
        let err: CommandError = Sessions::default().spawn(plan.request, |_| true, |_| {}).unwrap_err().into();
        assert_eq!(err.code, ErrorCode::CwdMissing);

        let lenient = TabLaunch { cwd: gone, fallback_to_home: true, ..terminal_tab() };
        let plan = plan_spawn(&lenient, &exe(), &db).unwrap();
        assert!(plan.fell_back_to_home);
        assert_eq!(plan.request.cwd, None);
        assert!(!plan.tracked, "terminal tabs are not tracked");
    }

    #[test]
    fn claude_spawn_plans_are_tracked_only_with_a_database() {
        let db = Db::open(temp_db("spawn-tracked")).unwrap();
        assert!(plan_spawn(&claude_tab(), &exe(), &db).unwrap().tracked);
        let memory = Db::open_or_recover(Err("no data dir".into()));
        let plan = plan_spawn(&claude_tab(), &exe(), &memory).unwrap();
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
        let t: TabLaunch = serde_json::from_str(terminal).unwrap();
        assert_eq!(t.kind, RunningKind::Terminal { command: Some("htop".into()) });
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
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["store"]["state"], "notSaving");
        assert_eq!(json["store"]["reason"], "x");
        assert_eq!(json["hookProblem"], serde_json::json!({ "at": 7, "message": "y", "log": "/d/hook.log" }));
    }
}
