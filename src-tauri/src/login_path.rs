//! macOS starts an app opened from Finder or the Dock with a bare `PATH`
//! (`/usr/bin:/bin:/usr/sbin:/sbin`), and a tab's login shell run with `-c` does not
//! read `~/.zshrc`, where installers such as Claude Code's add their directory. So at
//! startup misag asks the user's shell, as an interactive login shell, for its `PATH`
//! and takes that, as Terminal would have given it.

/// Printed around the `PATH`, so whatever the shell's startup files print is skipped.
const MARK: &str = "__misag_path__";

/// How long the shell may take: startup files can be slow, but misag must still open.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The `PATH` printed between two `MARK` lines in `out`, if any.
fn between_marks(out: &str) -> Option<&str> {
    let mut lines = out.lines().map(str::trim_end);
    lines.find(|l| *l == MARK)?;
    let path = lines.next()?;
    (lines.next()? == MARK && path.contains('/')).then_some(path)
}

/// The `PATH` that `shell` sets up as an interactive login shell, or None if it fails,
/// prints none or takes longer than `TIMEOUT`. `printenv` and `echo` work the same in
/// POSIX shells, fish and csh.
pub fn of_shell(shell: &str) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let script = format!("echo {MARK}; /usr/bin/printenv PATH; echo {MARK}");
    let mut child = Command::new(shell)
        .args(["-i", "-l", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read on another thread, so a chatty startup file cannot fill the pipe and stall.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        stdout.read_to_string(&mut out).ok().map(|_| out)
    });
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("misag: {shell} did not report its PATH within {TIMEOUT:?}");
                return None;
            }
        }
    }
    let out = reader.join().ok()??;
    between_marks(&out).map(str::to_owned)
}

/// Takes the login shell's `PATH` for misag and so for every tab. Call before any
/// other thread starts: it changes the environment.
#[cfg(target_os = "macos")]
pub fn adopt() {
    let shell = std::env::var("SHELL").ok().filter(|s| s.starts_with('/')).unwrap_or_else(|| "/bin/zsh".into());
    match of_shell(&shell) {
        // SAFETY: called first thing in `run`, before misag starts any thread.
        Some(path) => unsafe { std::env::set_var("PATH", path) },
        None => eprintln!("misag: keeping PATH as launched; tabs may not find programs set up in {shell}'s startup files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn asks_a_real_shell() {
        let path = of_shell("/bin/sh").expect("sh reports its PATH");
        assert!(path.contains('/'), "{path}");
        assert_eq!(of_shell("/no/such/shell"), None);
    }
}
