//! The `claude` wrapper and the `misag` shim misag puts on its tabs' `PATH`: shell
//! scripts on Unix, and on Windows hard links to (or copies of) misag itself.

#[cfg(unix)]
use super::{shell_quote, LAUNCH_FLAG};
use std::path::{Path, PathBuf};

/// The subdirectory of the database's directory that holds the `claude` wrapper.
pub const WRAPPER_DIR: &str = "bin";
/// The one that holds the `misag` shim. Not the wrapper's, which the wrapper takes
/// off PATH before it starts Claude.
pub const CLI_DIR: &str = "cli";

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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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
    #[cfg(unix)]
    install_script(&dir, "the claude wrapper", "claude", &wrapper_script(text, exe))?;
    // Windows runs no shell scripts by name, so the wrapper is misag's console build
    // itself, which acts as the wrapper when named `claude.exe`.
    #[cfg(windows)]
    {
        let _ = text;
        install_link(&dir, "the claude wrapper", "claude.exe", exe)?;
    }
    Ok(dir)
}

/// Puts `exe` in `dir` as `name`: a hard link, else a copy. One a tab still runs
/// cannot be replaced, so it is moved aside first and removed at a later start.
#[cfg(windows)]
fn install_link(dir: &Path, what: &str, name: &str, exe: &Path) -> Result<(), String> {
    let failed = |step: &str, e: std::io::Error| format!("cannot write {what} in {} ({step}): {e}", dir.display());
    std::fs::create_dir_all(dir).map_err(|e| failed("creating the folder", e))?;
    let target = dir.join(name);
    let stale = format!(".{name}.old");
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(&stale)) {
            // One a tab still runs cannot be removed yet: left for a later start.
            if let Err(e) = std::fs::remove_file(entry.path()) {
                if e.kind() != std::io::ErrorKind::PermissionDenied {
                    eprintln!("misag: cannot remove the old {what} {}: {e}", entry.path().display());
                }
            }
        }
    }
    if target.exists() && std::fs::remove_file(&target).is_err() {
        let aside = (0..u32::MAX).map(|n| dir.join(format!("{stale}{n}"))).find(|p| !p.exists());
        let aside = aside.ok_or_else(|| failed("moving the old one aside", std::io::Error::other("no free name")))?;
        std::fs::rename(&target, &aside).map_err(|e| failed("moving the old one aside", e))?;
    }
    match std::fs::hard_link(exe, &target) {
        Ok(()) => Ok(()),
        // Across drives, or where a link is not allowed, a copy does as well.
        Err(link) => std::fs::copy(exe, &target)
            .map(|_| ())
            .map_err(|e| failed("linking or copying", std::io::Error::other(format!("link: {link}; copy: {e}")))),
    }
}

/// Writes the `misag` shim (see `cli_script`) into `cli_dir(db)`, replacing any
/// earlier one, so it always runs this misag. Returns its directory. Installed apart
/// from the wrapper, so neither failing keeps the other from tabs.
pub fn install_cli(db: &Path, exe: &Path) -> Result<PathBuf, String> {
    let dir = cli_dir(db).ok_or_else(|| format!("{} has no directory", db.display()))?;
    #[cfg(unix)]
    install_script(&dir, "the misag shim", "misag", &cli_script(exe))?;
    #[cfg(windows)]
    install_link(&dir, "the misag shim", "misag.exe", exe)?;
    Ok(dir)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::claude::test_util::EXE;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

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
}
