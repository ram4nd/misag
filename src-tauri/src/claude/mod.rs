//! Launching Claude Code in Claude tabs, and the hook that reports which session
//! each tab is in, whether its Claude waits on background tasks, and moves the
//! tab's flow run on.
//!
//! Each Claude tab runs `claude` with a hook passed through `--settings`, so the
//! user's own Claude configuration is untouched. The hook is misag's own binary
//! in `--claude-hook` mode, for the tab named by `MISAG_TAB`. Claude runs it
//! whenever a session starts (launch, `--resume`, `/resume`, `/clear`, compaction),
//! and it records the session in the database; whenever Claude ends a turn
//! (Stop), and it records whether Claude waits on background tasks and moves the
//! tab's flow run on (see `runs`), printing the next prompt for Claude when there
//! is one; and when the session ends (SessionEnd), so the tab waits on nothing.
//!
//! Terminal tabs find a `claude` wrapper first on their `PATH` (see
//! `install_wrapper`), which hands `claude` to `misag --claude-launch`: that
//! starts the real `claude` with the same hook and the current settings, so a
//! Claude session started in a terminal tab is recorded for that tab and set up
//! like a Claude tab's. Tabs also find a `misag` shim on their `PATH` (see
//! `install_cli`), so `misag flow` there works on this app's flows.

mod hook;
mod hook_log;
mod install;
mod launch;
mod session_id;

pub use hook::{run_hook, PANIC_TEST_ENV};
pub use hook_log::{hook_problem, HookProblem, HOOK_LOG};
pub use install::{cli_dir, install_cli, install_wrapper, wrapper_dir, CLI_DIR, WRAPPER_DIR};
pub use launch::{args, hook_command, launch_args};
pub use session_id::{projects_dir, should_resume, SessionId};

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
/// Claude Code's cap on Stop-hook continuations in a row (8 by default). Tabs get
/// it raised above what a flow run can need (each step's prompt up to
/// `runs::MAX_RUN_STEPS`), with room to spare, so a run is not cut short; it applies to the user's own Stop hooks in those tabs too.
pub const STOP_CAP_ENV: &str = "CLAUDE_CODE_STOP_HOOK_BLOCK_CAP";
pub const STOP_CAP: u32 = 60;
/// Command-line flag that runs the binary as the hook.
pub const HOOK_FLAG: &str = "--claude-hook";
/// The Claude Code that first runs a hook's `args` (its exec form), which misag's
/// hooks use on Windows.
pub const HOOK_ARGS_SINCE: &str = "Claude Code from mid-May 2026 (hooks with `args`)";
/// Command-line flag the wrapper runs the binary with to start the real `claude`.
pub const LAUNCH_FLAG: &str = "--claude-launch";

/// Single-quotes `s` for a POSIX shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Helpers the tests of every part of this module share.
#[cfg(test)]
mod test_util {
    use super::SessionId;

    pub(super) const ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    pub(super) const EXE: &str = "/opt/my apps/misag";

    pub(super) fn id() -> SessionId {
        SessionId::parse(ID).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_paths_for_the_shell() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
