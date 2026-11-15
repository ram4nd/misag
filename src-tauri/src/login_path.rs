//! macOS starts an app opened from Finder or the Dock with a bare `PATH`
//! (`LAUNCHD_PATH`), and a tab's login shell run with `-c` does not read `~/.zshrc`,
//! where installers such as Claude Code's add their directory. So misag asks the
//! user's shell, as an interactive login shell, for its `PATH` and gives that to
//! every tab, as Terminal would have. It asks on a thread at startup, so the window
//! never waits for the shell; the first tab waits for the answer if it is not in yet.

// Only macOS asks; elsewhere `path` and `report` find nothing started.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Printed around the `PATH`, so whatever the shell's startup files print is skipped.
const MARK: &str = "__misag_path__";

/// How long the shell may take: startup files can be slow, but tabs must still start.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The `PATH` macOS gives an app opened from Finder or the Dock. Any other `PATH`
/// (misag started from a terminal, say) is kept as it is.
const LAUNCHD_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// What asking the shell came to, kept for the log and for tabs.
#[derive(Debug)]
struct Outcome {
    shell: String,
    took: Duration,
    path: Result<String, String>,
}

static STARTED: AtomicBool = AtomicBool::new(false);
static OUTCOME: OnceLock<Outcome> = OnceLock::new();

/// Starts asking `shell` for its `PATH` on another thread, when misag was opened
/// with macOS's bare one.
#[cfg(target_os = "macos")]
pub fn start(shell: String) {
    if std::env::var("PATH").is_ok_and(|p| p != LAUNCHD_PATH) {
        return;
    }
    STARTED.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        let begun = Instant::now();
        let path = of_shell(&shell, TIMEOUT);
        let _ = OUTCOME.set(Outcome { shell, took: begun.elapsed(), path });
    });
}

/// The `PATH` for tabs, waiting for the shell's answer if it is still coming, or
/// None when misag keeps the `PATH` it was started with.
pub fn path() -> Option<String> {
    if !STARTED.load(Ordering::SeqCst) {
        return None;
    }
    OUTCOME.wait().path.as_ref().ok().cloned()
}

/// Logs the outcome once it is in. Call after stderr goes to misag.log, so a launch
/// from Finder, which has no terminal, still leaves a trace.
pub fn report() {
    if !STARTED.load(Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let o = OUTCOME.wait();
        match &o.path {
            Ok(path) => eprintln!("misag: tabs get the PATH {} printed in {:?}: {path}", o.shell, o.took),
            Err(e) => eprintln!(
                "misag: tabs keep the bare PATH ({e}); programs set up in {}'s startup files may not be found",
                o.shell
            ),
        }
    });
}

/// The `PATH` printed between two `MARK` lines in `out`, if any.
fn between_marks(out: &str) -> Option<&str> {
    let mut lines = out.lines().map(str::trim_end);
    lines.find(|l| *l == MARK)?;
    let path = lines.next()?;
    (lines.next()? == MARK && path.contains('/')).then_some(path)
}

/// The flags that make `shell` an interactive login shell running a command. csh and
/// tcsh take `-l` only on its own, and read `~/.cshrc`, where they set `PATH`, for
/// `-c` anyway.
fn shell_args(shell: &str) -> &'static [&'static str] {
    match shell.rsplit('/').next() {
        Some("csh" | "tcsh") => &["-c"],
        _ => &["-i", "-l", "-c"],
    }
}

/// The `PATH` that `shell` sets up as an interactive login shell, or why not.
/// `printenv` and `echo` work the same in POSIX shells, fish and csh. The shell
/// runs in its own session, away from any terminal misag was started from, and
/// everything it started is killed if it takes longer than `timeout`. Reading stops
/// at the closing mark, so a program a startup file left running with the output
/// still open cannot hold misag up.
pub fn of_shell(shell: &str, timeout: Duration) -> Result<String, String> {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let script = format!("echo {MARK}; /usr/bin/printenv PATH; echo {MARK}");
    let mut cmd = Command::new(shell);
    cmd.args(shell_args(shell)).arg(&script).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no memory of this process.
    unsafe {
        cmd.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start {shell}: {e}"))?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{shell} has no output to read"));
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Bytes, since startup files may print text that is not UTF-8.
        let (mut reader, mut out, mut line) = (BufReader::new(stdout), String::new(), Vec::new());
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => out.push_str(&String::from_utf8_lossy(&line)),
                Err(e) => {
                    let _ = tx.send(Err(format!("could not read its output: {e}")));
                    return;
                }
            }
            if between_marks(&out).is_some() {
                break;
            }
        }
        let _ = tx.send(Ok(out));
    });
    let deadline = Instant::now() + timeout;
    let read = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let found = match read {
        Ok(Ok(out)) => between_marks(&out).map(str::to_owned).ok_or_else(|| format!("{shell} printed no PATH")),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("{shell} did not print its PATH within {timeout:?}")),
    };
    // The shell has printed all it will. Wait for it to end, up to the deadline,
    // then end it; on a timeout also what it started (its session is its process
    // group), since that is what held it up.
    let ended = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline && found.is_ok() => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => break false,
            Err(e) => {
                eprintln!("misag: could not check on {shell}: {e}");
                break false;
            }
        }
    };
    if !ended {
        if found.is_err() {
            // SAFETY: kill only sends a signal; the group is the shell's own session.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn finds_the_path_between_the_marks() {
        let out = format!("Welcome!\n{MARK}\n/Users/me/.local/bin:/usr/bin:/bin\n{MARK}\nbye\n");
        assert_eq!(between_marks(&out), Some("/Users/me/.local/bin:/usr/bin:/bin"));
        let crlf = format!("{MARK}\r\n/usr/bin\r\n{MARK}\r\n");
        assert_eq!(between_marks(&crlf), Some("/usr/bin"));
    }

    #[test]
    fn rejects_output_without_a_path() {
        assert_eq!(between_marks(""), None);
        assert_eq!(between_marks("no marks at all\n"), None);
        assert_eq!(between_marks(&format!("{MARK}\n/usr/bin\n")), None, "no closing mark");
        assert_eq!(between_marks(&format!("{MARK}\n\n{MARK}\n")), None, "empty PATH");
        assert_eq!(between_marks(&format!("{MARK}\n{MARK}\n")), None);
    }

    #[test]
    fn csh_gets_only_c() {
        assert_eq!(shell_args("/bin/tcsh"), ["-c"]);
        assert_eq!(shell_args("/bin/csh"), ["-c"]);
        assert_eq!(shell_args("/bin/zsh"), ["-i", "-l", "-c"]);
        assert_eq!(shell_args("/opt/homebrew/bin/fish"), ["-i", "-l", "-c"]);
    }

    /// A stand-in shell: runs `prelude`, then the command it was given with /bin/sh.
    fn fake_shell(name: &str, prelude: &str) -> String {
        let dir = crate::test_support::temp_dir(name);
        let shell = dir.join("fakesh");
        std::fs::write(&shell, format!("#!/bin/sh\n{prelude}\nfor last; do :; done\nexec /bin/sh -c \"$last\"\n")).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        shell.to_str().unwrap().to_owned()
    }

    #[test]
    fn asks_a_real_shell() {
        let path = of_shell("/bin/sh", TIMEOUT).expect("sh reports its PATH");
        assert!(path.contains('/'), "{path}");
        assert!(of_shell("/no/such/shell", TIMEOUT).unwrap_err().contains("could not start"));
    }

    #[test]
    fn skips_what_startup_files_print_even_when_not_utf8() {
        let shell = fake_shell("login-path-noise", "printf 'hello \\377 world\\n'");
        assert!(of_shell(&shell, TIMEOUT).unwrap().contains('/'));
    }

    #[test]
    fn a_program_left_running_with_the_output_open_does_not_hold_it_up() {
        let shell = fake_shell("login-path-daemon", "sleep 30 &");
        let begun = Instant::now();
        assert!(of_shell(&shell, TIMEOUT).unwrap().contains('/'));
        assert!(begun.elapsed() < Duration::from_secs(3), "took {:?}", begun.elapsed());
    }

    #[test]
    fn a_slow_shell_times_out() {
        let shell = fake_shell("login-path-slow", "sleep 30");
        let begun = Instant::now();
        let err = of_shell(&shell, Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("within"), "{err}");
        assert!(begun.elapsed() < Duration::from_secs(3), "took {:?}", begun.elapsed());
    }

    #[test]
    fn a_shell_that_prints_no_path_is_an_error() {
        let shell = fake_shell("login-path-none", "exit 3");
        assert!(of_shell(&shell, TIMEOUT).unwrap_err().contains("printed no PATH"));
    }

    #[test]
    fn nothing_is_asked_unless_started() {
        assert_eq!(path(), None);
    }
}
