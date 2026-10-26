//! Launching Claude Code in Claude tabs, and the hook that reports which session
//! each tab is in and moves the tab's flow run on.
//!
//! Each Claude tab runs `claude` with a hook passed through `--settings`, so the
//! user's own Claude configuration is untouched. The hook is misag's own binary
//! in `--claude-hook` mode, for the tab named by `MISAG_TAB`. Claude runs it
//! whenever a session starts (launch, `--resume`, `/resume`, `/clear`, compaction),
//! and it records the session in the database; and whenever Claude ends a turn
//! (Stop), and it moves the tab's flow run on (see `runs`), printing the next
//! prompt for Claude when there is one.
//!
//! Terminal tabs find a `claude` wrapper first on their `PATH` (see
//! `install_wrapper`), which hands `claude` to `misag --claude-launch`: that
//! starts the real `claude` with the same hook and the current settings, so a
//! Claude session started in a terminal tab is recorded for that tab and set up
//! like a Claude tab's. Tabs also find a `misag` shim on their `PATH` (see
//! `install_cli`), so `misag flow` there works on this app's flows.

use crate::db::{self, unix_secs, Db, SessionReport};
use crate::layout::TabId;
use crate::settings::Settings;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::ffi::OsString;
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Set on Claude and terminal tabs: the tab the hook reports for.
pub const TAB_ENV: &str = "MISAG_TAB";
/// Set on Claude and terminal tabs: the database the hook reports to.
pub const DB_ENV: &str = "MISAG_DB";
/// Set on terminal tabs: the wrapper directory misag put first on their PATH, so
/// a misag started from such a tab can take it off again.
pub const WRAPPER_ENV: &str = "MISAG_WRAPPER";
/// The same, set by raterm (misag's old name) in its tabs.
pub const RATERM_WRAPPER_ENV: &str = "RATERM_WRAPPER";
/// Set on Claude and terminal tabs: the directory of the `misag` shim misag put on
/// their PATH, so a misag started from such a tab can take it off again.
pub const CLI_ENV: &str = "MISAG_CLI";
/// Claude Code sets this for the commands and hooks it runs (seen in 2.1): its
/// own process, recorded with the session.
pub const PID_ENV: &str = "CLAUDE_PID";
/// The subdirectory of the database's directory that holds the `claude` wrapper.
pub const WRAPPER_DIR: &str = "bin";
/// The one that holds the `misag` shim. Not the wrapper's, which the wrapper takes
/// off PATH before it starts Claude.
pub const CLI_DIR: &str = "cli";
/// Claude Code's cap on Stop-hook continuations in a row (8 by default). Tabs get
/// it raised to just above misag's own limit (`runs::MAX_RUN_STEPS`), so a flow
/// run is not cut short; it applies to the user's own Stop hooks in those tabs too.
pub const STOP_CAP_ENV: &str = "CLAUDE_CODE_STOP_HOOK_BLOCK_CAP";
pub const STOP_CAP: u32 = 51;
/// Hook context longer than this reaches Claude only as a preview, so a longer
/// prompt is handed over as a "block" reason instead.
const MAX_CONTEXT: usize = 9_000;
/// Command-line flag that runs the binary as the hook.
pub const HOOK_FLAG: &str = "--claude-hook";
/// Command-line flag the wrapper runs the binary with to start the real `claude`.
pub const LAUNCH_FLAG: &str = "--claude-launch";
/// Hook failures are appended here, next to the database, since Claude does not
/// show a successful hook's stderr.
pub const HOOK_LOG: &str = "hook.log";
const HOOK_LOG_LIMIT: u64 = 256 * 1024;
/// Messages are cut to this many characters, so every entry fits in the tail
/// `last_hook_failure` reads.
const HOOK_MESSAGE_LIMIT: usize = 1000;
/// Longer than any entry (up to 4 bytes per character, plus the time, a tab id of
/// at most 128 bytes and separators), so the newest entry is always read whole.
const HOOK_LOG_TAIL: u64 = 4 * HOOK_MESSAGE_LIMIT as u64 + 256;
/// Written in place of the tab id when the hook does not know its tab.
const NO_TAB: &str = "-";
/// Makes the hook panic on purpose, so tests can check that a panic is still
/// logged and exits 0. Only honoured in debug builds; never set by misag itself.
pub const PANIC_TEST_ENV: &str = "MISAG_HOOK_TEST_PANIC";

/// A Claude Code session id. Only canonical lowercase UUIDs are accepted, since
/// the id ends up in a command line and a file path; holding one proves that.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(id: &str) -> Result<Self, String> {
        // Only the canonical form: the id must print back exactly as given.
        match uuid::Uuid::try_parse(id) {
            Ok(uuid) if uuid.hyphenated().to_string() == id => Ok(Self(id.to_owned())),
            _ => Err(format!("invalid Claude session id {id:?}")),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SessionId {
    type Error = String;
    fn try_from(id: String) -> Result<Self, String> {
        Self::parse(&id)
    }
}

impl From<SessionId> for String {
    fn from(id: SessionId) -> String {
        id.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Claude's projects directory: `$CLAUDE_CONFIG_DIR/projects`, defaulting to
/// `~/.claude/projects`.
pub fn projects_dir() -> Option<PathBuf> {
    projects_dir_from(std::env::var_os("CLAUDE_CONFIG_DIR"), std::env::var_os("HOME"))
}

fn projects_dir_from(config_dir: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match config_dir {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home?).join(".claude"),
    };
    Some(base.join("projects"))
}

/// Whether `path` is a file. Only a path that is not there (or runs through a
/// non-directory) counts as absent; any other error, such as a permission
/// problem, is reported rather than read as "no file".
fn file_exists(path: &Path) -> Result<bool, String> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.is_file()),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Ok(false),
        Err(e) => Err(format!("cannot check {}: {e}", path.display())),
    }
}

/// Whether Claude Code saved a transcript for `id` in any project. A missing
/// projects directory means none; any other read error is reported, not guessed.
fn transcript_in_projects(projects: &Path, id: &SessionId) -> Result<bool, String> {
    let cannot_read = |e: std::io::Error| format!("cannot read {}: {e}", projects.display());
    let entries = match std::fs::read_dir(projects) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(cannot_read(e)),
    };
    let file = format!("{id}.jsonl");
    for entry in entries {
        if file_exists(&entry.map_err(cannot_read)?.path().join(&file))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `session` has a conversation to resume. The transcript path the hook
/// reported is checked first; Claude's projects directory is scanned when no
/// reported transcript exists on disk. An error means the answer is unknown, and
/// guessing would either lose the conversation or fail to start.
pub fn should_resume(session: &SessionId, reported: Option<&Path>, projects: Option<&Path>) -> Result<bool, String> {
    if let Some(reported) = reported
        && file_exists(reported)?
    {
        return Ok(true);
    }
    match projects {
        Some(projects) => transcript_in_projects(projects, session),
        None => Ok(false),
    }
}

/// Single-quotes `s` for a POSIX shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The hook command line. Claude runs hook commands through a POSIX shell, so the
/// path is single-quoted.
pub fn hook_command(exe: &Path) -> String {
    format!("{} {HOOK_FLAG}", shell_quote(&exe.to_string_lossy()))
}

/// The `--settings` JSON: `keys` (from misag's own settings) plus, with `hook_exe`,
/// misag's hook for SessionStart (to record the tab's session) and Stop (to move
/// the tab's flow run on). None when there are no keys and no hook, so no
/// `--settings` is passed.
fn settings_json(hook_exe: Option<&Path>, mut keys: Map<String, Value>) -> Option<String> {
    if let Some(exe) = hook_exe {
        debug_assert!(!keys.contains_key("hooks"), "a setting would replace misag's hook");
        let command = hook_command(exe);
        let hook = serde_json::json!([{ "hooks": [{ "type": "command", "command": command }] }]);
        keys.insert("hooks".into(), serde_json::json!({ "SessionStart": hook, "Stop": hook }));
    }
    (!keys.is_empty()).then(|| Value::Object(keys).to_string())
}

/// The directory of the `claude` wrapper for the database at `db`.
pub fn wrapper_dir(db: &Path) -> Option<PathBuf> {
    Some(db.parent()?.join(WRAPPER_DIR))
}

/// The directory of the `misag` shim for the database at `db`.
pub fn cli_dir(db: &Path) -> Option<PathBuf> {
    Some(db.parent()?.join(CLI_DIR))
}

/// The `misag` shim: runs `exe`, the misag that runs the tab, so `misag flow` in a
/// tab (by the person or by Claude) works on that app's database whichever misag,
/// if any, is installed on PATH.
fn cli_script(exe: &Path) -> String {
    let exe = shell_quote(&exe.to_string_lossy());
    format!(
        r#"#!/bin/sh
# Written by misag at every start; changes are lost. misag's tabs have this
# directory on PATH, so `misag` there is the misag that runs them.
exec {exe} "$@"
"#
    )
}

/// Writes `text` as the executable `name` in `dir`, renamed into place, so a tab
/// running it meanwhile never runs half a script. `what` names it in errors.
fn install_script(dir: &Path, what: &str, name: &str, text: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let failed = |e: std::io::Error| format!("cannot write {what} in {}: {e}", dir.display());
    std::fs::create_dir_all(dir).map_err(failed)?;
    let partial = dir.join(format!(".{name}.partial"));
    let written = std::fs::write(&partial, text)
        .and_then(|()| std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::rename(&partial, dir.join(name)));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&partial);
        return Err(failed(e));
    }
    Ok(())
}

/// The `claude` wrapper: takes its own directory off `PATH` and has `exe` start the
/// real `claude` (see `LAUNCH_FLAG`), or runs `claude` unchanged with a warning if
/// `exe` has gone (another misag build rewrote the shared wrapper, say). Inside a
/// Claude session (`CLAUDECODE`, which
/// terminal tabs never inherit) it runs `claude` unchanged, so a `claude` that
/// Claude itself runs cannot replace the session recorded for the tab.
fn wrapper_script(dir: &str, exe: &Path) -> String {
    let (dir, exe) = (shell_quote(dir), shell_quote(&exe.to_string_lossy()));
    format!(
        r#"#!/bin/sh
# Written by misag at every start; changes are lost. Terminal tabs have this
# directory first on PATH, so `claude` there starts with misag's settings and
# reports its session to misag.
self={dir}
rest=
set -f
IFS=:
for d in $PATH; do
  [ "$d" = "$self" ] || [ "$d" = "$self/" ] || rest=${{rest:+$rest:}}$d
done
unset IFS
set +f
PATH=$rest
export PATH
if [ -n "$CLAUDECODE" ]; then exec claude "$@"; fi
if [ ! -x {exe} ]; then
  echo "misag: "{exe}" is gone, so claude starts without misag's settings and session hook" >&2
  exec claude "$@"
fi
exec {exe} {LAUNCH_FLAG} "$@"
"#
    )
}

/// Writes the `claude` wrapper (see `wrapper_script`) into `wrapper_dir(db)`,
/// replacing any earlier one, so it always runs this misag. Returns its directory.
pub fn install_wrapper(db: &Path, exe: &Path) -> Result<PathBuf, String> {
    let dir = wrapper_dir(db).ok_or_else(|| format!("{} has no directory", db.display()))?;
    let text = dir.to_str().ok_or_else(|| format!("{} is not valid UTF-8", dir.display()))?;
    install_script(&dir, "the claude wrapper", "claude", &wrapper_script(text, exe))?;
    Ok(dir)
}

/// Writes the `misag` shim (see `cli_script`) into `cli_dir(db)`, replacing any
/// earlier one, so it always runs this misag. Returns its directory. Installed apart
/// from the wrapper, so neither failing keeps the other from tabs.
pub fn install_cli(db: &Path, exe: &Path) -> Result<PathBuf, String> {
    let dir = cli_dir(db).ok_or_else(|| format!("{} has no directory", db.display()))?;
    install_script(&dir, "the misag shim", "misag", &cli_script(exe))?;
    Ok(dir)
}

/// Arguments for `claude`: resume `session`, or start it under that id. With
/// `hook_exe`, also the hook that reports session starts and changes; without one
/// (nowhere to record them) the session is simply not tracked. `settings` (see
/// `Settings::claude_settings`) share that one `--settings` argument.
pub fn args(session: &SessionId, resume: bool, hook_exe: Option<&Path>, settings: Map<String, Value>) -> Vec<String> {
    let flag = if resume { "--resume" } else { "--session-id" };
    let mut args = vec![flag.to_string(), session.to_string()];
    if let Some(json) = settings_json(hook_exe, settings) {
        args.extend(["--settings".to_string(), json]);
    }
    args
}

/// Arguments for a `claude` started in a terminal tab: what `settings` add to a
/// Claude tab, the hook (with `hook_exe`), then the user's own `args` unchanged.
pub fn launch_args(settings: &Settings, hook_exe: Option<&Path>, args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut all: Vec<OsString> = settings.claude_args().into_iter().map(OsString::from).collect();
    if let Some(json) = settings_json(hook_exe, settings.claude_settings()) {
        all.extend(["--settings".into(), json.into()]);
    }
    all.extend(args);
    all
}

/// The part of Claude's SessionStart payload misag records.
#[derive(Deserialize)]
struct SessionStart {
    session_id: SessionId,
    source: Option<String>,
    cwd: Option<String>,
    transcript_path: Option<String>,
}

fn record(input: &str, tab_id: &TabId, db_path: &Path, claude_pid: Option<u32>) -> Result<(), String> {
    let event: SessionStart = serde_json::from_str(input).map_err(|e| format!("bad hook payload: {e}"))?;
    let mut conn = db::connect_existing(db_path)?;
    if event.source.as_deref() == Some("clear") {
        // Claude no longer has the running step's prompt, so it will not answer it.
        crate::runs::pause_tab(
            &mut conn,
            tab_id,
            "The conversation was cleared, so Claude no longer has this step's prompt. Pick an output to go on.",
        )?;
    }
    db::record_claude_session(
        &conn,
        &SessionReport {
            tab_id,
            session_id: &event.session_id,
            source: event.source.as_deref(),
            cwd: event.cwd.as_deref(),
            transcript_path: event.transcript_path.as_deref(),
            claude_pid,
        },
    )
}

/// Which hook event a payload is.
#[derive(Deserialize)]
struct Event {
    hook_event_name: Option<String>,
}

/// The part of Claude's Stop payload the flow runner reads.
#[derive(Deserialize)]
struct Stop {
    /// Claude's final reply; older Claude Code versions do not report it, and the
    /// run then waits for the person, saying so.
    last_assistant_message: Option<String>,
}

/// What the hook did.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// Recorded the session a tab is now in.
    Recorded,
    /// Handled a Stop (the tab's flow run, if any, moved on), with the hook output
    /// that hands Claude the next prompt, if there is one.
    Stopped(Option<String>),
}

/// The hook output that has Claude go on with `prompt`. Context rather than a
/// "block" decision, so Claude Code does not show it as a hook error (the same
/// limit on continuations applies); but context past `MAX_CONTEXT` would reach
/// Claude only as a preview, so a longer prompt is a block reason.
fn continuation(prompt: &str) -> String {
    let output = if prompt.chars().count() > MAX_CONTEXT {
        serde_json::json!({ "decision": "block", "reason": prompt })
    } else {
        serde_json::json!({ "hookSpecificOutput": { "hookEventName": "Stop", "additionalContext": prompt } })
    };
    output.to_string()
}

/// Moves the tab's flow run on after Claude ended a turn, returning the hook output
/// that hands Claude the next prompt, if there is one.
fn stopped(input: &str, tab_id: &TabId, db_path: &Path) -> Result<Option<String>, String> {
    let event: Stop = serde_json::from_str(input).map_err(|e| format!("bad hook payload: {e}"))?;
    // Across an upgrade (the app not yet restarted) runs cannot be read; every turn
    // of every tab ends here, so that is not logged each time.
    let Some(mut conn) = db::connect_if_current(db_path)? else { return Ok(None) };
    match crate::runs::on_stop(&mut conn, tab_id, event.last_assistant_message.as_deref()) {
        Ok(prompt) => Ok(prompt.as_deref().map(continuation)),
        Err(e) => {
            // Best effort: show the run as waiting rather than running forever.
            let why = format!("The Stop hook could not move the run on ({e}). Pick an output to go on.");
            if let Err(also) = crate::runs::pause_tab(&mut conn, tab_id, &why) {
                return Err(format!("{e}; and could not mark the run: {also}"));
            }
            Err(e)
        }
    }
}

/// Reads one hook payload from `input`: a SessionStart is recorded for `tab_id`,
/// with the Claude process that sent it, and a Stop moves the tab's flow run on.
fn hook(mut input: impl Read, tab_id: Option<&TabId>, db_path: Option<&Path>, claude_pid: Option<u32>) -> Result<Outcome, String> {
    let (Some(tab_id), Some(db_path)) = (tab_id, db_path) else {
        return Err(format!("{HOOK_FLAG} needs a valid {TAB_ENV} and {DB_ENV}"));
    };
    let mut payload = String::new();
    input.read_to_string(&mut payload).map_err(|e| format!("could not read the payload: {e}"))?;
    // A payload without a readable event name goes to the session recorder, which
    // says what is wrong with it.
    let event = serde_json::from_str::<Event>(&payload).ok().and_then(|e| e.hook_event_name);
    match event.as_deref() {
        Some("Stop") => stopped(&payload, tab_id, db_path)
            .map(Outcome::Stopped)
            .map_err(|e| format!("could not move the flow run on: {e}")),
        Some("SessionStart") | None => record(&payload, tab_id, db_path, claude_pid)
            .map(|()| Outcome::Recorded)
            .map_err(|e| format!("could not record the Claude session: {e}")),
        Some(other) => Err(format!("misag's hook was run for {other}, which it does not handle")),
    }
}

/// Appends a failure to the hook log next to the database, starting the log over
/// once it grows past `HOOK_LOG_LIMIT`. Best effort: logging must not fail the hook,
/// so a log that cannot be written is only reported on stderr. Each entry is one
/// line, `<unix time> <tab id or -> <message>`, parsed by `last_hook_failure`.
fn log_failure(db_path: Option<&Path>, tab_id: Option<&TabId>, message: &str) {
    use std::io::Write;
    let mut message = message.replace(['\r', '\n'], " | ");
    if let Some((cut, _)) = message.char_indices().nth(HOOK_MESSAGE_LIMIT) {
        message.truncate(cut);
        message.push('…');
    }
    let _ = writeln!(std::io::stderr(), "misag: {message}");
    let Some(log) = db_path.and_then(Path::parent).map(|dir| dir.join(HOOK_LOG)) else { return };
    let too_big = std::fs::metadata(&log).is_ok_and(|m| m.len() > HOOK_LOG_LIMIT);
    let file = std::fs::OpenOptions::new().create(true).append(!too_big).write(true).truncate(too_big).open(&log);
    let tab = tab_id.map_or(NO_TAB, TabId::as_str);
    let written = file.and_then(|mut file| writeln!(file, "{} {tab} {message}", unix_secs(SystemTime::now())));
    if let Err(e) = written {
        let _ = writeln!(std::io::stderr(), "misag: could not write {}: {e}", log.display());
    }
}

/// A failure the hook logged.
#[derive(Debug, PartialEq)]
struct HookFailure {
    at: u64,
    tab_id: Option<TabId>,
    message: String,
}

/// The newest entry in the hook log at `log`; None if there is no log. Only the
/// end of the file is read, and when that cuts into an entry, the partial first
/// line is dropped.
fn last_hook_failure(log: &Path) -> std::io::Result<Option<HookFailure>> {
    let mut file = match std::fs::File::open(log) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let start = file.metadata()?.len().saturating_sub(HOOK_LOG_TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let complete = if start > 0 { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
    Ok(complete.lines().rev().find_map(|line| {
        let (time, rest) = line.split_once(' ')?;
        let (tab, message) = rest.split_once(' ')?;
        let tab_id = match tab {
            NO_TAB => None,
            tab => Some(TabId::parse(tab).ok()?),
        };
        Some(HookFailure { at: time.parse().ok()?, tab_id, message: message.to_owned() })
    }))
}

/// The last failure the session hook logged. Mirrored by `HookProblem` in src/main.ts.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HookProblem {
    /// Identifies this failure for the UI, so a dismissed one stays dismissed: when
    /// it was logged (unix seconds), or for an unreadable log when this run started.
    pub at: u64,
    pub message: String,
    /// The log to look at.
    pub log: String,
}

/// The last failure the session hook logged during this run, unless the tab it
/// failed for (or, when it did not know its tab, any tab) has reported a session
/// since, which means the hook works again.
pub fn hook_problem(db: &Db) -> Option<HookProblem> {
    let log = db.hook_path()?.parent()?.join(HOOK_LOG);
    let shown = log.display().to_string();
    let opened = unix_secs(db.opened());
    let failure = match last_hook_failure(&log) {
        Ok(failure) => failure?,
        Err(e) => return Some(HookProblem { at: opened, message: format!("cannot read the log: {e}"), log: shown }),
    };
    if failure.at < opened {
        return None;
    }
    match db.latest_report_at(failure.tab_id.as_ref()) {
        Ok(Some(reported)) if reported > failure.at => return None,
        Ok(_) => {}
        Err(e) => eprintln!("misag: could not check the latest session report: {e}"),
    }
    let message = match &failure.tab_id {
        Some(tab) => format!("tab {tab}: {}", failure.message),
        None => failure.message,
    };
    Some(HookProblem { at: failure.at, message, log: shown })
}

/// Entry point for `misag --claude-hook`. Always exits 0: a failure here must
/// never disturb the Claude session, so it is only logged. Panics are handled by a
/// panic hook that logs and exits 0 before any unwinding, which also works with
/// the release profile's `panic = "abort"` (see Cargo.toml), where `catch_unwind`
/// could not.
pub fn run_hook() -> i32 {
    let db_path: Option<PathBuf> = std::env::var_os(DB_ENV).map(PathBuf::from);
    let tab_id = std::env::var(TAB_ENV).ok().and_then(|id| TabId::parse(&id).ok());
    let (panic_log, panic_tab) = (db_path.clone(), tab_id.clone());
    std::panic::set_hook(Box::new(move |info| {
        log_failure(panic_log.as_deref(), panic_tab.as_ref(), &format!("the hook panicked: {info}"));
        std::process::exit(0);
    }));
    if cfg!(debug_assertions) && std::env::var_os(PANIC_TEST_ENV).is_some() {
        panic!("panic requested through {PANIC_TEST_ENV}");
    }
    let claude_pid = std::env::var(PID_ENV).ok().and_then(|pid| pid.parse().ok());
    match hook(std::io::stdin(), tab_id.as_ref(), db_path.as_deref(), claude_pid) {
        Err(message) => log_failure(db_path.as_deref(), tab_id.as_ref(), &message),
        // Recorded, but a terminal tab cannot tell whether this session is still open.
        Ok(Outcome::Recorded) if claude_pid.is_none() => log_failure(
            db_path.as_deref(),
            tab_id.as_ref(),
            &format!("{PID_ENV} is missing or invalid, so a terminal tab will not reopen this Claude session"),
        ),
        Ok(Outcome::Recorded | Outcome::Stopped(None)) => {}
        Ok(Outcome::Stopped(Some(output))) => println!("{output}"),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{temp_db, tid};
    use crate::layout::{Layout, Tab, TabKind};
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const EXE: &str = "/opt/my apps/misag";

    fn id() -> SessionId {
        SessionId::parse(ID).unwrap()
    }

    #[test]
    fn validates_session_ids() {
        assert!(SessionId::parse(ID).is_ok());
        for bad in [
            "0F8FAD5B-D9CB-469F-A165-70867728950E",
            "../../etc/passwd-aaaaaaaaaaaaaaaaaaaaaa",
            "0f8fad5b; rm -rf ~; 469f-a165-7086772895",
            "0f8fad5b-d9cb-469f-a165-70867728950g",
            "0f8fad5bd-9cb-469f-a165-70867728950e",
            "",
        ] {
            assert!(SessionId::parse(bad).is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<SessionId>(r#""nope""#).is_err());
        assert_eq!(serde_json::to_string(&id()).unwrap(), format!("\"{ID}\""));
    }

    #[test]
    fn starts_a_new_session_with_the_hook() {
        let args = args(&id(), false, Some(Path::new(EXE)), Map::new());
        assert_eq!(args[..3], ["--session-id", ID, "--settings"]);
        let settings: serde_json::Value = serde_json::from_str(&args[3]).unwrap();
        let hook = &settings["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(hook["command"], "'/opt/my apps/misag' --claude-hook");
        // The same command runs when Claude ends a turn, to move a flow run on.
        assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0], *hook);
        assert_eq!(settings.as_object().unwrap().len(), 1, "only the hook");
    }

    #[test]
    fn resumes_with_the_resume_flag() {
        assert_eq!(args(&id(), true, Some(Path::new(EXE)), Map::new())[..2], ["--resume", ID]);
    }

    #[test]
    fn without_a_hook_binary_there_is_no_hook() {
        assert_eq!(args(&id(), false, None, Map::new()), ["--session-id", ID]);
    }

    fn no_auto_memory() -> Map<String, Value> {
        crate::settings::Settings { disable_auto_memory: true, ..Default::default() }.claude_settings()
    }

    #[test]
    fn settings_keys_share_the_hooks_settings_json() {
        let args = args(&id(), false, Some(Path::new(EXE)), no_auto_memory());
        assert_eq!(args.iter().filter(|a| *a == "--settings").count(), 1);
        let settings: serde_json::Value = serde_json::from_str(&args[3]).unwrap();
        assert_eq!(settings["autoMemoryEnabled"], false);
        assert_eq!(settings["hooks"]["SessionStart"][0]["hooks"][0]["command"], "'/opt/my apps/misag' --claude-hook");
    }

    #[test]
    fn without_a_hook_the_settings_keys_are_passed_alone() {
        let args = args(&id(), false, None, no_auto_memory());
        assert_eq!(args[..3], ["--session-id", ID, "--settings"]);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&args[3]).unwrap(), serde_json::json!({ "autoMemoryEnabled": false }));
    }

    /// Writes an executable stand-in at `path` that prints `name`, its arguments
    /// and `PATH`, one per line.
    fn stand_in(path: &Path, name: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("#!/bin/sh\nprintf '%s\\n' {name} \"$@\" \"PATH=$PATH\"\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Runs the installed wrapper with stand-ins for misag and the real `claude`.
    fn run_wrapper(base: &Path, extra_env: &[(&str, &str)]) -> Vec<String> {
        let exe = base.join("my apps/misag");
        stand_in(&exe, "misag");
        let wrapper = install_wrapper(&base.join("data/misag.db"), &exe).unwrap();
        let real = base.join("real");
        stand_in(&real.join("claude"), "claude");
        let path = format!("{}:{}/:{}:/usr/bin:/bin", wrapper.display(), wrapper.display(), real.display());
        let out = std::process::Command::new("claude")
            .args(["--resume", "it's"])
            .env("PATH", &path)
            .env_remove("CLAUDECODE")
            .envs(extra_env.iter().copied())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().lines().map(str::to_owned).collect()
    }

    #[test]
    fn the_wrapper_hands_claude_to_misag() {
        let base = crate::test_support::temp_dir("wrapper");
        let lines = run_wrapper(&base, &[]);
        assert_eq!(lines[..4], ["misag", LAUNCH_FLAG, "--resume", "it's"]);
        assert_eq!(lines[4], format!("PATH={}:/usr/bin:/bin", base.join("real").display()), "its own directory is gone");
        // Installing again replaces it.
        assert_eq!(run_wrapper(&base, &[]), lines);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_misag_shim_runs_this_misag_and_stays_on_claude_s_path() {
        let base = crate::test_support::temp_dir("wrapper-cli");
        let exe = base.join("my apps/misag");
        stand_in(&exe, "misag");
        let db = base.join("data/misag.db");
        let wrapper = install_wrapper(&db, &exe).unwrap();
        let cli = install_cli(&db, &exe).unwrap();
        assert_eq!(cli, cli_dir(&db).unwrap());
        let out = std::process::Command::new(cli.join("misag")).args(["flow", "it's"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8(out.stdout).unwrap().lines().take(3).collect::<Vec<_>>(), ["misag", "flow", "it's"]);
        // The claude wrapper takes only itself off PATH, so Claude finds this misag.
        let real = base.join("real");
        stand_in(&real.join("claude"), "claude");
        let path = format!("{}:{}:{}:/usr/bin:/bin", wrapper.display(), cli.display(), real.display());
        let out = std::process::Command::new("claude").env("PATH", &path).env_remove("CLAUDECODE").output().unwrap();
        let lines: Vec<String> = String::from_utf8(out.stdout).unwrap().lines().map(str::to_owned).collect();
        assert_eq!(lines.last().unwrap(), &format!("PATH={}:{}:/usr/bin:/bin", cli.display(), real.display()));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn without_a_real_claude_the_wrapper_fails_instead_of_calling_itself() {
        let base = crate::test_support::temp_dir("wrapper-missing");
        let wrapper = install_wrapper(&base.join("data/misag.db"), Path::new(EXE)).unwrap();
        // Listed twice and with a trailing slash: every spelling must be taken off.
        let path = format!("{w}:{w}/:{w}:{empty}", w = wrapper.display(), empty = base.display());
        // Nested in Claude, the wrapper runs `claude` itself rather than through misag.
        let out = std::process::Command::new(wrapper.join("claude")).env("PATH", path).env("CLAUDECODE", "1").output().unwrap();
        assert_eq!(out.status.code(), Some(127), "{}", String::from_utf8_lossy(&out.stderr));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_wrapper_whose_misag_is_gone_still_runs_claude() {
        let base = crate::test_support::temp_dir("wrapper-stale");
        let wrapper = install_wrapper(&base.join("data/misag.db"), &base.join("gone/misag")).unwrap();
        let real = base.join("real");
        stand_in(&real.join("claude"), "claude");
        let out = std::process::Command::new("claude")
            .arg("-c")
            .env("PATH", format!("{}:{}:/usr/bin:/bin", wrapper.display(), real.display()))
            .env_remove("CLAUDECODE")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let lines: Vec<String> = String::from_utf8(out.stdout).unwrap().lines().map(str::to_owned).collect();
        assert_eq!(lines[..2], ["claude", "-c"]);
        assert!(String::from_utf8_lossy(&out.stderr).contains("gone/misag"), "says why the settings are missing");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_failed_install_leaves_no_partial_wrapper() {
        if crate::test_support::running_as_root() {
            return;
        }
        let base = crate::test_support::temp_dir("wrapper-readonly");
        let dir = wrapper_dir(&base.join("misag.db")).unwrap();
        fs::create_dir(&dir).unwrap();
        // The rename fails: `claude` is a directory that is not empty.
        fs::create_dir_all(dir.join("claude/x")).unwrap();
        let err = install_wrapper(&base.join("misag.db"), Path::new(EXE)).unwrap_err();
        assert!(err.contains("cannot write the claude wrapper"), "{err}");
        assert!(!dir.join(".claude.partial").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_shim_that_cannot_be_written_says_so_and_leaves_the_wrapper_alone() {
        if crate::test_support::running_as_root() {
            return;
        }
        let base = crate::test_support::temp_dir("cli-blocked");
        let db = base.join("misag.db");
        // A file where the shim's directory goes.
        fs::write(cli_dir(&db).unwrap(), "").unwrap();
        let err = install_cli(&db, Path::new(EXE)).unwrap_err();
        assert!(err.starts_with("cannot write the misag shim in"), "{err}");
        assert!(install_wrapper(&db, Path::new(EXE)).is_ok());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn inside_a_claude_session_the_wrapper_adds_no_hook() {
        let base = crate::test_support::temp_dir("wrapper-nested");
        let lines = run_wrapper(&base, &[("CLAUDECODE", "1")]);
        assert_eq!(lines[..3], ["claude", "--resume", "it's"]);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_launch_from_a_terminal_gets_the_settings_and_the_hook_before_the_users_arguments() {
        let s = crate::settings::Settings { skip_permissions: true, disable_auto_memory: true, ..Default::default() };
        let args = launch_args(&s, Some(Path::new(EXE)), ["--resume".into(), "it's".into()]);
        let json = settings_json(Some(Path::new(EXE)), s.claude_settings()).unwrap();
        let expected = ["--dangerously-skip-permissions", "--settings", &json, "--resume", "it's"];
        assert_eq!(args, expected.map(OsString::from));
    }

    #[test]
    fn a_launch_without_settings_or_a_hook_passes_the_users_arguments_alone() {
        assert_eq!(launch_args(&Default::default(), None, ["-p".into()]), [OsString::from("-p")]);
    }

    #[test]
    fn decides_to_resume_from_the_reported_transcript_or_the_projects_dir() {
        let base = crate::test_support::temp_dir("resume");
        let project = base.join("projects").join("-home-me-src");
        fs::create_dir_all(&project).unwrap();
        let reported = base.join("elsewhere.jsonl");
        fs::write(&reported, "{}").unwrap();

        assert!(should_resume(&id(), Some(&reported), None).unwrap(), "reported transcript exists");
        assert!(!should_resume(&id(), Some(&base.join("gone.jsonl")), Some(&base.join("projects"))).unwrap());
        fs::write(project.join(format!("{ID}.jsonl")), "{}").unwrap();
        assert!(should_resume(&id(), None, Some(&base.join("projects"))).unwrap(), "found by scanning");
        assert!(!should_resume(&id(), None, Some(&base.join("missing"))).unwrap());
        assert!(!should_resume(&id(), None, None).unwrap());

        // An unreadable projects directory is an error, not "no conversation".
        let not_a_dir = base.join("file");
        fs::write(&not_a_dir, "").unwrap();
        assert!(should_resume(&id(), None, Some(&not_a_dir)).is_err());
        // A stray file among the projects is not a project.
        fs::write(base.join("projects").join("notes.txt"), "").unwrap();
        fs::remove_file(project.join(format!("{ID}.jsonl"))).unwrap();
        assert!(!should_resume(&id(), None, Some(&base.join("projects"))).unwrap());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_transcript_that_cannot_be_checked_is_an_error_not_absent() {
        if crate::test_support::running_as_root() {
            return;
        }
        let base = crate::test_support::temp_dir("resume-locked");
        let project = base.join("projects").join("-home-me-src");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(format!("{ID}.jsonl")), "{}").unwrap();
        // Listable but not searchable: the transcript's metadata cannot be read.
        fs::set_permissions(&project, fs::Permissions::from_mode(0o644)).unwrap();
        let scanned = should_resume(&id(), None, Some(&base.join("projects")));
        let reported = should_resume(&id(), Some(&project.join(format!("{ID}.jsonl"))), None);
        fs::set_permissions(&project, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(scanned.unwrap_err().contains("cannot check"));
        assert!(reported.unwrap_err().contains("cannot check"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn finds_the_projects_dir_from_the_config_dir_or_home() {
        let config = projects_dir_from(Some("/cfg".into()), Some("/home/me".into()));
        assert_eq!(config, Some(PathBuf::from("/cfg/projects")));
        assert_eq!(projects_dir_from(None, Some("/home/me".into())), Some(PathBuf::from("/home/me/.claude/projects")));
        assert_eq!(projects_dir_from(None, None), None);
    }

    #[test]
    fn quotes_paths_for_the_shell() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    fn log_of(db_path: &Path) -> PathBuf {
        db_path.parent().unwrap().join(HOOK_LOG)
    }

    #[test]
    fn failures_are_logged_next_to_the_database_with_their_tab() {
        let path = temp_db("hook-log");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        log_failure(Some(&path), Some(&tid("tab-1")), "first problem");
        log_failure(Some(&path), None, "second problem");
        let log = fs::read_to_string(log_of(&path)).unwrap();
        let lines: Vec<_> = log.lines().map(|l| l.split_once(' ').unwrap().1).collect();
        assert_eq!(lines, ["tab-1 first problem", "- second problem"]);
        log_failure(None, None, "nowhere to log");
    }

    #[test]
    fn multi_line_messages_are_logged_as_one_entry() {
        let path = temp_db("hook-log-lines");
        let db = Db::open(path.clone()).unwrap();
        log_failure(Some(&path), Some(&tid("t")), "the hook panicked: panicked at x.rs:1:1:\nboom");
        let log = fs::read_to_string(log_of(&path)).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(log.contains("panicked at x.rs:1:1: | boom"), "{log}");
        let problem = hook_problem(&db).unwrap();
        assert_eq!(problem.message, "tab t: the hook panicked: panicked at x.rs:1:1: | boom");
        assert_eq!(problem.log, log_of(&path).display().to_string());
    }

    #[test]
    fn an_oversized_log_starts_over() {
        let path = temp_db("hook-log-big");
        let log = log_of(&path);
        fs::write(&log, "x".repeat(HOOK_LOG_LIMIT as usize + 1)).unwrap();
        log_failure(Some(&path), None, "fresh problem");
        let content = fs::read_to_string(&log).unwrap();
        assert!(content.len() < 100 && content.contains("fresh problem"), "{} bytes", content.len());
    }

    #[test]
    fn a_huge_message_is_cut_so_it_can_still_be_read_back() {
        let path = temp_db("hook-log-huge");
        let db = Db::open(path.clone()).unwrap();
        log_failure(Some(&path), Some(&tid(&"t".repeat(128))), &"ü".repeat(5000));
        let problem = hook_problem(&db).expect("the newest entry is always read whole");
        assert!(problem.message.ends_with('…'));
        assert_eq!(problem.message.chars().filter(|&c| c == 'ü').count(), HOOK_MESSAGE_LIMIT);
    }

    #[test]
    fn reports_failures_logged_during_this_run_only() {
        let path = temp_db("hook-problem");
        fs::write(log_of(&path), "1 - an old failure\n").unwrap();
        let db = Db::open(path.clone()).unwrap();
        assert_eq!(hook_problem(&db), None, "older failures are not this run's");
        let now = unix_secs(SystemTime::now());
        fs::write(log_of(&path), format!("1 - an old failure\n{now} - schema 2 in misag.db, this binary knows 1\n"))
            .unwrap();
        let problem = hook_problem(&db).unwrap();
        assert_eq!((problem.at, problem.message.as_str()), (now, "schema 2 in misag.db, this binary knows 1"));
    }

    #[test]
    fn only_the_failing_tabs_later_report_clears_its_problem() {
        let path = temp_db("hook-recovered");
        let db = Db::open(path.clone()).unwrap();
        let opened = unix_secs(db.opened());
        // Explicit times instead of waiting for the clock: entries and reports `at` seconds after opening.
        let fail = |at: u64, tab: &str| {
            let line = format!("{} {tab} failure\n", opened + at);
            fs::OpenOptions::new().create(true).append(true).open(log_of(&path)).unwrap().write_all(line.as_bytes()).unwrap();
        };
        let conn = db::connect_existing(&path).unwrap();
        let session = id();
        let report = |at: u64, tab: &str| {
            let tab = tid(tab);
            db::record_claude_session(&conn, &db::tests::report(&tab, &session)).unwrap();
            conn.execute("UPDATE claude_sessions SET updated_at = ?1 WHERE tab_id = ?2", rusqlite::params![(opened + at) as i64, tab.as_str()])
                .unwrap();
        };
        fail(1, "a");
        report(2, "b");
        assert!(hook_problem(&db).is_some(), "another tab working says nothing about tab a");
        report(2, "a");
        assert_eq!(hook_problem(&db), None, "the hook worked for tab a after the failure");

        // A failure that did not know its tab is cleared by any later report.
        fail(3, "-");
        assert!(hook_problem(&db).is_some());
        report(4, "b");
        assert_eq!(hook_problem(&db), None);
    }

    #[test]
    fn reads_the_last_complete_entry_from_a_cut_tail() {
        let path = temp_db("hook-tail");
        let db = Db::open(path.clone()).unwrap();
        let now = unix_secs(SystemTime::now());
        // One entry longer than the tail, cut exactly where its rest looks like an entry.
        let looks_like_one = format!("{now} - cut");
        let rest = format!("{looks_like_one}{}\n", "y".repeat(HOOK_LOG_TAIL as usize - 1 - looks_like_one.len()));
        let content = format!("{now} - {}{rest}", "x".repeat(10));
        fs::write(log_of(&path), &content).unwrap();
        assert_eq!(hook_problem(&db), None, "a line cut by the tail is not read as an entry");

        fs::write(log_of(&path), format!("{content}{now} - the latest failure\nnot an entry\n")).unwrap();
        assert_eq!(hook_problem(&db).unwrap().message, "the latest failure");
    }

    #[test]
    fn an_unreadable_log_is_a_problem_with_a_stable_key() {
        let path = temp_db("hook-log-dir");
        let db = Db::open(path.clone()).unwrap();
        fs::create_dir(log_of(&path)).unwrap();
        let first = hook_problem(&db).unwrap();
        assert!(first.message.contains("cannot read the log"), "{first:?}");
        // Keyed by the run, not the time of the check, so polling again cannot undo a dismissal.
        assert_eq!(first.at, unix_secs(db.opened()));
    }

    #[test]
    fn records_the_reported_session_for_the_tab() {
        let path = temp_db("hook-record");
        let db = Db::open(path.clone()).unwrap();
        let earlier = SessionId::parse("1b4e28ba-2fa1-41d2-883f-0016d3cca427").unwrap();
        let tab = Tab {
            tab_id: tid("tab-1"),
            kind: TabKind::Claude { claude_session: Some(earlier) },
            name: "c".into(),
            renamed: false,
            cwd: None,
        };
        db.save_layout(Layout { active: 0, tabs: vec![tab] }).unwrap();

        let payload = format!(
            r#"{{"session_id":"{ID}","transcript_path":"/x/{ID}.jsonl","cwd":"/home/me",
                 "hook_event_name":"SessionStart","source":"clear"}}"#
        );
        record(&payload, &tid("tab-1"), &path, None).unwrap();
        let layout = db.load_layout().unwrap();
        assert_eq!(layout.tabs[0].kind, TabKind::Claude { claude_session: Some(id()) });
    }

    #[test]
    fn the_hook_reads_its_payload_from_input() {
        let path = temp_db("hook-input");
        drop(Db::open(path.clone()).unwrap());
        let payload = format!(r#"{{"session_id":"{ID}","source":"startup"}}"#);
        hook(payload.as_bytes(), Some(&tid("tab-2")), Some(&path), Some(42)).unwrap();
        let conn = db::connect_existing(&path).unwrap();
        let stored: (String, u32) = conn
            .query_row("SELECT session_id, claude_pid FROM claude_sessions WHERE tab_id = 'tab-2'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(stored, (ID.to_string(), 42));
    }

    #[test]
    fn the_hook_needs_its_environment() {
        let path = temp_db("hook-env");
        let err = hook("{}".as_bytes(), None, Some(&path), None).unwrap_err();
        assert!(err.contains("MISAG_TAB"), "{err}");
        assert!(hook("{}".as_bytes(), Some(&tid("t")), None, None).is_err());
        assert!(!path.exists(), "nothing should be written without a tab");
    }

    #[test]
    fn ignores_malformed_payloads() {
        let path = temp_db("hook-bad");
        assert!(record("not json", &tid("tab-1"), &path, None).is_err());
        assert!(record(r#"{"session_id":"; rm -rf ~"}"#, &tid("tab-1"), &path, None).is_err());
    }

    #[test]
    fn a_long_prompt_is_handed_over_whole() {
        // Claude Code shows only a preview of hook context over 10,000 characters.
        let short: Value = serde_json::from_str(&continuation("Fix it.")).unwrap();
        assert_eq!(short["hookSpecificOutput"]["additionalContext"], "Fix it.");
        let long_prompt = "x".repeat(9_500);
        let long: Value = serde_json::from_str(&continuation(&long_prompt)).unwrap();
        assert_eq!((long["decision"].as_str(), long["reason"].as_str()), (Some("block"), Some(long_prompt.as_str())));
    }
}
