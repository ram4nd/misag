//! The arguments `claude` is started with: the session, and the `--settings` JSON
//! that carries misag's hook and the settings misag passes on.

use super::{shell_quote, SessionId, HOOK_FLAG};
use crate::settings::Settings;
use serde_json::{Map, Value};
use std::ffi::OsString;
use std::path::Path;

/// The hook command line for a POSIX shell, which Claude runs hook commands through
/// everywhere but Windows (see `hook_entry`), so the path is single-quoted.
pub fn hook_command(exe: &Path) -> String {
    format!("{} {HOOK_FLAG}", shell_quote(&exe.to_string_lossy()))
}

/// misag's hook as Claude's settings give it: a command line for the shell, or with
/// `direct` the program and its arguments, run without a shell (`HOOK_ARGS_SINCE`).
fn hook_entry(exe: &Path, direct: bool) -> Value {
    if direct {
        serde_json::json!({ "type": "command", "command": exe.to_string_lossy(), "args": [HOOK_FLAG] })
    } else {
        serde_json::json!({ "type": "command", "command": hook_command(exe) })
    }
}

/// The `--settings` JSON: `keys` (from misag's own settings) plus, with `hook_exe`,
/// misag's hook for SessionStart (to record the tab's session), Stop (to record
/// background tasks and move the tab's flow run on), SessionEnd (to forget the
/// background tasks) and UserPromptSubmit (for a flow run waiting for the
/// person's prompt). None when there are no keys and no hook, so no
/// `--settings` is passed.
fn settings_json(hook_exe: Option<&Path>, mut keys: Map<String, Value>) -> Option<String> {
    if let Some(exe) = hook_exe {
        debug_assert!(!keys.contains_key("hooks"), "a setting would replace misag's hook");
        // Windows may run hook commands through Git Bash or PowerShell, which quote
        // differently, so there the hook runs without a shell.
        let hook = serde_json::json!([{ "hooks": [hook_entry(exe, cfg!(windows))] }]);
        keys.insert(
            "hooks".into(),
            serde_json::json!({ "SessionStart": hook, "Stop": hook, "SessionEnd": hook, "UserPromptSubmit": hook }),
        );
    }
    (!keys.is_empty()).then(|| Value::Object(keys).to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::test_util::{id, EXE, ID};

    #[test]
    fn starts_a_new_session_with_the_hook() {
        let args = args(&id(), false, Some(Path::new(EXE)), Map::new());
        assert_eq!(args[..3], ["--session-id", ID, "--settings"]);
        let settings: serde_json::Value = serde_json::from_str(&args[3]).unwrap();
        let hook = &settings["hooks"]["SessionStart"][0]["hooks"][0];
        // This platform's form: a shell command line, or on Windows a program and arguments.
        assert_eq!(*hook, hook_entry(Path::new(EXE), cfg!(windows)));
        // The same command runs when Claude ends a turn, to move a flow run on, and
        // when its session ends, so nothing is left waiting on background tasks.
        assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0], *hook);
        assert_eq!(settings["hooks"]["SessionEnd"][0]["hooks"][0], *hook);
        // And when a prompt is sent, which a run moved on by hand waits for.
        assert_eq!(settings["hooks"]["UserPromptSubmit"][0]["hooks"][0], *hook);
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
        assert_eq!(settings["hooks"]["SessionStart"][0]["hooks"][0], hook_entry(Path::new(EXE), cfg!(windows)));
    }

    #[test]
    fn hooks_run_through_a_shell_or_directly() {
        let shell = hook_entry(Path::new("/opt/my apps/misag"), false);
        assert_eq!(shell, serde_json::json!({ "type": "command", "command": "'/opt/my apps/misag' --claude-hook" }));
        let exe = r"C:\Users\Ra Mänd\AppData\Roaming\io.github.ram4nd.misag\bin\claude.exe";
        let direct = hook_entry(Path::new(exe), true);
        assert_eq!(direct, serde_json::json!({ "type": "command", "command": exe, "args": ["--claude-hook"] }));
    }

    #[test]
    fn without_a_hook_the_settings_keys_are_passed_alone() {
        let args = args(&id(), false, None, no_auto_memory());
        assert_eq!(args[..3], ["--session-id", ID, "--settings"]);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&args[3]).unwrap(), serde_json::json!({ "autoMemoryEnabled": false }));
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
}
