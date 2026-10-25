//! PTY session management, independent of Tauri so it can be tested directly.

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// How long a closed session gets to exit on SIGHUP before it is sent SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// What a session runs, always through the user's login shell so PATH and the
/// rest of their environment are set up.
#[derive(Debug, PartialEq, Default)]
pub enum Launch {
    #[default]
    LoginShell,
    /// A command line, interpreted by the shell.
    Command(String),
    /// A program with arguments, passed through without shell interpretation.
    Program { program: String, args: Vec<String> },
}

#[derive(Default)]
pub struct SpawnRequest {
    pub cols: u16,
    pub rows: u16,
    /// None (or blank) for `$HOME`; a leading `~` is expanded.
    pub cwd: Option<String>,
    pub launch: Launch,
    /// Extra environment, set before the terminal variables so it cannot override them.
    pub env: Vec<(String, String)>,
}

struct Session {
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Drained by a per-session writer thread, so a PTY that stops reading
    /// its input never blocks a command or another session.
    input: Sender<Vec<u8>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pid: Option<u32>,
    /// Set once the child has been reaped, so SIGKILL never hits a reused pid.
    exited: Arc<ExitFlag>,
}

/// Set by the thread that reaps a session's child; others can wait for it.
#[derive(Default)]
struct ExitFlag {
    done: Mutex<bool>,
    changed: Condvar,
}

impl ExitFlag {
    fn set(&self) {
        *lock(&self.done) = true;
        self.changed.notify_all();
    }

    /// Waits up to `timeout` for the flag; returns whether it is set.
    fn wait(&self, timeout: Duration) -> bool {
        let guard = lock(&self.done);
        let (done, _) = self.changed.wait_timeout_while(guard, timeout, |done| !*done).unwrap_or_else(|e| e.into_inner());
        *done
    }
}

impl Session {
    /// Sends SIGHUP, then SIGKILL to the child's process groups if it is still
    /// alive after `grace`. The returned thread ends as soon as the child is reaped.
    fn kill(&self, grace: Duration) -> Option<std::thread::JoinHandle<()>> {
        if let Err(e) = lock(&self.killer).kill() {
            eprintln!("misag: SIGHUP to pid {:?} failed: {e}", self.pid);
        }
        let pid = self.pid?;
        // The foreground job may sit in its own process group under job control.
        let foreground = lock(&self.master).process_group_leader();
        let exited = self.exited.clone();
        Some(std::thread::spawn(move || {
            if exited.wait(grace) {
                return;
            }
            // SAFETY: plain syscalls. The child is not reaped yet (`exited` is false),
            // so its pid, and therefore its process group, still exist.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                if let Some(fg) = foreground.filter(|&fg| fg > 0 && fg != pid as libc::pid_t) {
                    libc::kill(-fg, libc::SIGKILL);
                }
            }
        }))
    }
}

/// Why a session could not start. `CwdMissing` (the directory no longer exists)
/// lets a restore fall back to `$HOME` for that reason only.
#[derive(Debug, PartialEq)]
pub enum SpawnError {
    CwdMissing(String),
    Failed(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::CwdMissing(dir) => write!(f, "directory does not exist: {dir}"),
            SpawnError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl From<String> for SpawnError {
    fn from(e: String) -> Self {
        SpawnError::Failed(e)
    }
}

/// Shells (matched by file name) whose `-c` takes `"$0" "$@"` the POSIX way. Any
/// other shell is treated as non-POSIX and goes through the trampoline.
const POSIX_SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "mksh", "ash", "yash"];

fn is_posix_shell(shell: &str) -> bool {
    let name = Path::new(shell).file_name().and_then(|n| n.to_str()).unwrap_or(shell);
    POSIX_SHELLS.contains(&name)
}

/// Variables that only a running Claude Code session sets, to mark its own
/// process and its children (seen in Claude Code 2.1), plus the ones misag sets
/// on its own tabs. They describe whatever launched misag, such as a Claude Code
/// session running a dev build or another misag's tab, not the user's
/// environment, so tabs never inherit them; otherwise a `claude` in a tab would
/// behave as a child of that session. Update when Claude Code adds markers.
/// User settings (`CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_USE_BEDROCK`, `ANTHROPIC_*`,
/// ...) are not on this list.
pub const LAUNCHER_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_PID",
    // Set by misag on its own tabs. A misag started from one of them must not
    // pass on the outer misag's tab and database (`launch_plan` also drops the
    // outer wrapper directory from PATH).
    "MISAG_TAB",
    "MISAG_DB",
    "MISAG_WRAPPER",
    // The same, from tabs of raterm (misag's old name) still open after the switch.
    "RATERM_TAB",
    "RATERM_DB",
    "RATERM_WRAPPER",
];

/// Variables Claude Code also sets for the commands it runs (for example
/// `GIT_EDITOR=true`, which makes `git commit` without `-m` abort). A user may set
/// these on purpose, so they are dropped only when misag itself was started from
/// Claude Code (`CLAUDECODE` is set), which is where they then came from.
pub const CLAUDE_CODE_SHELL_ENV: &[&str] = &[
    "CLAUDE_EFFORT",
    "GIT_EDITOR",
    "DISABLE_UPDATES",
    "DISABLE_INSTALLATION_CHECKS",
    "AI_AGENT",
    "COREPACK_ENABLE_AUTO_PIN",
    "NoDefaultCurrentDirectoryInExePath",
];

/// The variables tabs must not inherit, given whether misag was started from Claude Code.
pub fn launcher_vars(started_from_claude_code: bool) -> Vec<&'static str> {
    let extra = if started_from_claude_code { CLAUDE_CODE_SHELL_ENV } else { &[] };
    LAUNCHER_ENV.iter().chain(extra).copied().collect()
}

/// Environment variable carrying the program and arguments for `misag --exec`.
pub const EXEC_ENV: &str = "MISAG_EXEC";
pub const EXEC_FLAG: &str = "--exec";

/// Whether `path` can be single-quoted the same way in every common shell (POSIX
/// shells, fish and csh): it contains none of `'`, `\`, `!` or a newline.
fn quotable_everywhere(path: &str) -> bool {
    !path.contains(['\'', '\\', '!', '\n'])
}

pub struct Sessions {
    shell: String,
    /// misag's own binary, which runs `Launch::Program` under non-POSIX shells.
    trampoline: Option<String>,
    kill_grace: Duration,
    /// Inherited variables removed from every tab (see `launcher_vars`).
    strip_env: Vec<&'static str>,
    next_id: AtomicU32,
    map: Arc<Mutex<HashMap<u32, Arc<Session>>>>,
}

impl Default for Sessions {
    fn default() -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| {
            eprintln!("misag: $SHELL is not set, falling back to /bin/sh");
            "/bin/sh".into()
        });
        Self::new(shell, KILL_GRACE)
    }
}

impl Sessions {
    pub fn new(shell: impl Into<String>, kill_grace: Duration) -> Self {
        Self {
            shell: shell.into(),
            trampoline: None,
            kill_grace,
            strip_env: launcher_vars(std::env::var_os("CLAUDECODE").is_some()),
            next_id: AtomicU32::new(0),
            map: Arc::default(),
        }
    }

    /// Lets `Launch::Program` work under non-POSIX shells by running `exe --exec`.
    pub fn with_trampoline(mut self, exe: String) -> Self {
        self.trampoline = Some(exe);
        self
    }

    /// Starts a session. `on_data` receives output and returns false once nobody
    /// is listening; `on_exit` gets the exit code after all output was delivered.
    pub fn spawn(
        &self,
        req: SpawnRequest,
        mut on_data: impl FnMut(&[u8]) -> bool + Send + 'static,
        on_exit: impl FnOnce(i32) + Send + 'static,
    ) -> Result<u32, SpawnError> {
        let dir = resolve_cwd(req.cwd)?;
        let shell = &self.shell;

        let pair = native_pty_system()
            .openpty(PtySize { rows: req.rows.max(1), cols: req.cols.max(1), pixel_width: 0, pixel_height: 0 })
            .map_err(|e| format!("could not open a PTY: {e:#}"))?;
        // Take both ends before spawning so a failure here cannot orphan a child.
        let mut reader = pair.master.try_clone_reader().map_err(|e| format!("PTY reader: {e:#}"))?;
        let mut writer = pair.master.take_writer().map_err(|e| format!("PTY writer: {e:#}"))?;

        let mut cmd = CommandBuilder::new(shell);
        // Launcher state first; `req.env` may still set any of these on purpose.
        for key in &self.strip_env {
            cmd.env_remove(key);
        }
        // Caller variables before the terminal type, so they cannot override it.
        for (key, value) in &req.env {
            cmd.env(key, value);
        }
        match &req.launch {
            Launch::LoginShell => cmd.arg("-l"),
            Launch::Command(c) if c.trim().is_empty() => cmd.arg("-l"),
            Launch::Command(c) => cmd.args(["-lc", c]),
            // `exec "$0" "$@"` runs the program with its arguments exactly as given.
            Launch::Program { program, args } if is_posix_shell(shell) => {
                cmd.args(["-lc", r#"exec "$0" "$@""#, program]);
                cmd.args(args);
            }
            // Other shells (fish, csh, ...) spell that differently, so the login shell
            // runs misag itself, which execs the program from an environment variable.
            Launch::Program { program, args } => {
                let exe = self.trampoline.as_deref().ok_or_else(|| {
                    format!("cannot launch {program} under {shell}: no misag binary to run it through")
                })?;
                if !quotable_everywhere(exe) {
                    return Err(format!(
                        "cannot launch {program} under {shell}: misag's path {exe:?} contains a quote, \
                         backslash, ! or newline, which {shell} cannot be given safely; install misag elsewhere"
                    )
                    .into());
                }
                let argv: Vec<&str> = std::iter::once(program.as_str()).chain(args.iter().map(String::as_str)).collect();
                let argv = serde_json::to_string(&argv).map_err(|e| e.to_string())?;
                cmd.env(EXEC_ENV, argv);
                cmd.args(["-l", "-c", &format!("'{exe}' {EXEC_FLAG}")]);
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if let Some(d) = &dir {
            cmd.cwd(d);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("could not start {shell} in {dir:?}: {e:#}"))?;
        drop(pair.slave);

        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let exited = Arc::new(ExitFlag::default());
        let (input, input_rx) = mpsc::channel::<Vec<u8>>();
        lock(&self.map).insert(
            id,
            Arc::new(Session {
                master: Mutex::new(pair.master),
                input,
                killer: Mutex::new(child.clone_killer()),
                pid: child.process_id(),
                exited: exited.clone(),
            }),
        );

        // Ends when the session is removed (sender dropped) or the PTY closes.
        std::thread::spawn(move || {
            for data in input_rx {
                if let Err(e) = writer.write_all(&data).and_then(|_| writer.flush()) {
                    eprintln!("misag: session {id} write failed: {e}");
                    break;
                }
            }
        });

        let map = self.map.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            let mut detached = false;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if !on_data(&buf[..n]) {
                            eprintln!("misag: session {id} lost its frontend, killing it");
                            detached = true;
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => {
                        // EIO is how Linux reports the slave side closing.
                        if e.raw_os_error() != Some(libc::EIO) {
                            eprintln!("misag: session {id} read failed: {e}");
                        }
                        break;
                    }
                }
            }
            if detached {
                let _ = child.kill();
            }
            let code = match child.wait() {
                Ok(status) => status.exit_code() as i32,
                Err(e) => {
                    eprintln!("misag: session {id} wait failed: {e}");
                    -1
                }
            };
            exited.set();
            lock(&map).remove(&id);
            if !detached {
                on_exit(code);
            }
        });

        Ok(id)
    }

    /// Queues input for the session; never blocks on the PTY.
    pub fn write(&self, id: u32, data: Vec<u8>) -> Result<(), String> {
        self.get(id)?.input.send(data).map_err(|_| format!("session {id} is closed"))
    }

    pub fn resize(&self, id: u32, cols: u16, rows: u16) -> Result<(), String> {
        let s = self.get(id)?;
        lock(&s.master)
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| format!("session {id} resize failed: {e:#}"))
    }

    /// Returns immediately: the child is hung up now and force-killed after the grace period.
    pub fn kill(&self, id: u32) {
        if let Some(s) = lock(&self.map).remove(&id) {
            s.kill(self.kill_grace);
        }
    }

    /// The session process's current directory (read from `/proc`, so Linux only).
    /// For a Claude tab under a POSIX login shell that is Claude's directory, since
    /// the shell `exec`s into it; under fish or csh it is the shell's, which stays in
    /// the directory the tab started in.
    /// A directory deleted while in use reads as `<path> (deleted)`; that suffix is
    /// dropped, so a restore finds the path missing and falls back to `$HOME`.
    pub fn cwd(&self, id: u32) -> Option<String> {
        let pid = self.get(id).ok()?.pid?;
        let link = format!("/proc/{pid}/cwd");
        let path = match std::fs::read_link(&link) {
            Ok(path) => path,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("misag: cannot read {link}: {e}");
                }
                return None;
            }
        };
        let path = live_path(path);
        path.into_os_string().into_string().map_err(|p| eprintln!("misag: non-UTF-8 directory {p:?}")).ok()
    }

    /// The leader of the session's foreground process group, when that is not the
    /// session process itself: the program a shell is running in the foreground,
    /// such as `claude` typed at its prompt.
    /// An error means it could not be told (no such session, or the PTY query
    /// failed), which callers must not read as "nothing in the foreground".
    pub fn foreground(&self, id: u32) -> Result<Option<u32>, String> {
        let s = self.get(id)?;
        let master = lock(&s.master);
        let fd = master.as_raw_fd().ok_or_else(|| format!("session {id} has no PTY descriptor"))?;
        // SAFETY: plain syscall on a descriptor the locked master keeps open.
        let leader = unsafe { libc::tcgetpgrp(fd) };
        if leader <= 0 {
            let e = format!("session {id}: cannot read the foreground process: {}", std::io::Error::last_os_error());
            eprintln!("misag: {e}");
            return Err(e);
        }
        let leader = leader as u32;
        Ok((Some(leader) != s.pid).then_some(leader))
    }

    /// The name the kernel gives the process (`/proc/<pid>/comm`), such as `claude`.
    pub fn process_name(pid: u32) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|name| name.trim_end().to_owned())
    }

    /// The command line process `pid` was started with, if it can be read.
    pub fn process_args(pid: u32) -> Option<Vec<String>> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        // Each argument ends in a NUL; a process that is exiting reads as empty.
        let args: Vec<String> = raw.split(|&b| b == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        let args = args.split_last().map_or(&args[..], |(_, rest)| rest);
        (!args.is_empty()).then(|| args.to_vec())
    }

    /// Hangs up every session, force-killing survivors after the grace period in
    /// the background. Returns the threads doing that.
    pub fn kill_all(&self) -> Vec<std::thread::JoinHandle<()>> {
        let all: Vec<_> = lock(&self.map).drain().map(|(_, s)| s).collect();
        all.iter().filter_map(|s| s.kill(self.kill_grace)).collect()
    }

    /// For app exit: like `kill_all`, but waits until every session has exited or
    /// been force-killed, since background threads die with the process. Returns
    /// at once when every child exits on SIGHUP; waits the grace period at most.
    pub fn kill_all_and_wait(&self) {
        for handle in self.kill_all() {
            let _ = handle.join();
        }
    }

    /// Clones the session out so the map lock is never held during PTY work.
    fn get(&self, id: u32) -> Result<Arc<Session>, String> {
        lock(&self.map).get(&id).cloned().ok_or_else(|| format!("no session {id}"))
    }
}

/// Locks a mutex, recovering the data if a previous holder panicked (possible
/// only in debug builds; release builds abort on panic).
pub(crate) fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A `/proc` link target without the ` (deleted)` Linux appends when the target was
/// removed or replaced while in use, unless a path really is named that way.
pub(crate) fn live_path(target: std::path::PathBuf) -> std::path::PathBuf {
    match target.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(stripped) if !target.exists() => stripped.into(),
        _ => target,
    }
}

/// The requested directory, or `$HOME` when it no longer exists; the flag says
/// whether that fallback happened. Other problems are still errors.
pub fn cwd_or_home(cwd: Option<String>) -> Result<(Option<String>, bool), SpawnError> {
    match resolve_cwd(cwd.clone()) {
        Ok(_) => Ok((cwd, false)),
        Err(SpawnError::CwdMissing(_)) => Ok((None, true)),
        Err(e) => Err(e),
    }
}

/// `dir` with a leading `~` (alone or before `/`) replaced by `$HOME`; unchanged if
/// `$HOME` is unset, and `~user` is left alone.
pub fn expand_tilde(dir: &str) -> String {
    match (dir.strip_prefix('~'), std::env::var("HOME")) {
        (Some(rest), Ok(h)) if rest.is_empty() || rest.starts_with('/') => format!("{h}{rest}"),
        _ => dir.to_owned(),
    }
}

/// Resolves the requested working directory, expanding a leading `~`.
fn resolve_cwd(cwd: Option<String>) -> Result<Option<String>, SpawnError> {
    let Some(dir) = cwd.filter(|d| !d.trim().is_empty()) else { return Ok(std::env::var("HOME").ok()) };
    let dir = expand_tilde(&dir);
    // Only "not found" means missing: a permission error or a dead mount must not
    // look like a vanished directory, or a restore would replace it with $HOME.
    match std::fs::metadata(&dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(SpawnError::Failed(format!("not a directory: {dir}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(SpawnError::CwdMissing(dir)),
        Err(e) => return Err(SpawnError::Failed(format!("cannot use directory {dir}: {e}"))),
    }
    Ok(Some(dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;
    use std::time::Instant;

    const TIMEOUT: Duration = Duration::from_secs(10);

    struct Probe {
        id: u32,
        output: Arc<Mutex<Vec<u8>>>,
        exit: Receiver<i32>,
    }

    impl Probe {
        fn text(&self) -> String {
            String::from_utf8_lossy(&lock(&self.output)).into_owned()
        }

        fn wait_for(&self, needle: &str) {
            let start = Instant::now();
            while !self.text().contains(needle) {
                assert!(start.elapsed() < TIMEOUT, "timed out waiting for {needle:?}, got {:?}", self.text());
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn exit_code(&self) -> i32 {
            self.exit.recv_timeout(TIMEOUT).expect("session did not exit")
        }
    }

    /// A request with test defaults; override fields with `..req()`.
    fn req() -> SpawnRequest {
        SpawnRequest { cols: 80, rows: 24, ..Default::default() }
    }

    fn sessions() -> Sessions {
        Sessions::new("/bin/sh", Duration::from_millis(300))
    }

    fn spawn(s: &Sessions, command: &str) -> Probe {
        spawn_in(s, command, None)
    }

    fn spawn_in(s: &Sessions, command: &str, cwd: Option<&str>) -> Probe {
        spawn_with(s, Launch::Command(command.into()), cwd, vec![])
    }

    fn spawn_with(s: &Sessions, launch: Launch, cwd: Option<&str>, env: Vec<(String, String)>) -> Probe {
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        let (tx, exit) = mpsc::channel();
        let req = SpawnRequest { cwd: cwd.map(Into::into), launch, env, ..req() };
        let id = s
            .spawn(
                req,
                move |b| {
                    lock(&sink).extend_from_slice(b);
                    true
                },
                move |code| {
                    let _ = tx.send(code);
                },
            )
            .expect("spawn failed");
        Probe { id, output, exit }
    }

    #[test]
    fn delivers_output_before_exit() {
        let s = sessions();
        let p = spawn(&s, "echo hello-misag");
        assert_eq!(p.exit_code(), 0);
        assert!(p.text().contains("hello-misag"), "output: {:?}", p.text());
    }

    #[test]
    fn reports_real_exit_code() {
        let s = sessions();
        assert_eq!(spawn(&s, "exit 3").exit_code(), 3);
    }

    #[test]
    fn forwards_input_to_the_process() {
        let s = sessions();
        let p = spawn(&s, "read line; echo got:$line");
        s.write(p.id, b"abc\n".to_vec()).unwrap();
        assert_eq!(p.exit_code(), 0);
        assert!(p.text().contains("got:abc"), "output: {:?}", p.text());
    }

    #[test]
    fn knows_the_program_a_shell_runs_in_the_foreground() {
        // With job control on, as at an interactive prompt, the job gets its own process group.
        let s = Sessions::new("/bin/bash", Duration::from_millis(300));
        let p = spawn(&s, "echo idle; sleep 0.5; set -m; sh -c 'echo fg:$$; exec sleep 60'; true");
        p.wait_for("idle");
        assert_eq!(s.foreground(p.id), Ok(None), "only the shell runs");
        p.wait_for("fg:");
        let text = p.text();
        let pid: u32 = text.split("fg:").nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        assert_eq!(s.foreground(p.id), Ok(Some(pid)));
        assert_eq!(Sessions::process_name(pid).as_deref(), Some("sleep"));
        assert_eq!(Sessions::process_args(pid), Some(vec!["sleep".to_string(), "60".to_string()]));
        s.kill(p.id);
        p.exit_code();
        assert!(s.foreground(p.id).is_err(), "gone with the session: unknown, not idle");
        assert_eq!(Sessions::process_args(pid), None, "gone");
    }

    #[test]
    fn removes_session_after_exit() {
        let s = sessions();
        let p = spawn(&s, "true");
        p.exit_code();
        assert!(s.write(p.id, b"x".to_vec()).is_err());
        assert!(s.resize(p.id, 100, 30).is_err());
    }

    #[test]
    fn unknown_session_is_an_error() {
        let s = sessions();
        assert!(s.write(999, b"x".to_vec()).is_err());
        assert!(s.resize(999, 80, 24).is_err());
        s.kill(999);
    }

    #[test]
    fn kill_hangs_up_a_normal_process() {
        let s = sessions();
        let p = spawn(&s, "echo ready; sleep 60");
        p.wait_for("ready");
        let start = Instant::now();
        s.kill(p.id);
        p.exit_code();
        assert!(start.elapsed() < Duration::from_millis(300), "SIGHUP should be enough");
    }

    #[test]
    fn kill_escalates_when_sighup_is_ignored() {
        let s = sessions();
        let p = spawn(&s, "trap '' HUP; echo ready; sleep 60; echo survived");
        p.wait_for("ready");
        let start = Instant::now();
        s.kill(p.id);
        p.exit_code();
        assert!(start.elapsed() >= Duration::from_millis(300), "exited before the grace period");
        assert!(!p.text().contains("survived"));
    }

    #[test]
    fn kill_returns_immediately() {
        let s = sessions();
        let p = spawn(&s, "trap '' HUP; echo ready; sleep 60");
        p.wait_for("ready");
        let start = Instant::now();
        s.kill(p.id);
        assert!(start.elapsed() < Duration::from_millis(100));
        p.exit_code();
    }

    #[test]
    fn stalled_session_does_not_block_others() {
        let s = sessions();
        let stalled = spawn(&s, "echo ready; sleep 60");
        stalled.wait_for("ready");
        let start = Instant::now();
        // Far more than the PTY input buffer holds, and nothing reads it.
        for _ in 0..64 {
            s.write(stalled.id, vec![b'x'; 16 * 1024]).unwrap();
        }
        assert!(start.elapsed() < Duration::from_millis(500), "write blocked the caller");

        let other = spawn(&s, "read line; echo ok:$line");
        s.write(other.id, b"y\n".to_vec()).unwrap();
        assert_eq!(other.exit_code(), 0);
        assert!(other.text().contains("ok:y"));
        s.kill(stalled.id);
        stalled.exit_code();
    }

    #[test]
    fn a_deleted_directory_is_reported_without_the_deleted_suffix() {
        let s = sessions();
        let dir = crate::test_support::temp_dir("gone");
        let p = spawn_in(&s, "echo ready; sleep 60", Some(dir.to_str().unwrap()));
        p.wait_for("ready");
        std::fs::remove_dir(&dir).unwrap();
        assert_eq!(s.cwd(p.id).as_deref(), Some(dir.to_str().unwrap()));
        s.kill(p.id);
        p.exit_code();
    }

    #[test]
    fn reports_the_shell_cwd() {
        let s = sessions();
        let p = spawn(&s, "cd /tmp && echo ready && sleep 60");
        p.wait_for("ready");
        assert_eq!(s.cwd(p.id).as_deref(), Some("/tmp"));
        assert_eq!(s.cwd(999), None);
        s.kill(p.id);
        p.exit_code();
    }

    #[test]
    fn passes_program_arguments_verbatim() {
        let s = sessions();
        let args = ["%s|", "a b", "it's", "$HOME", "\"q\""].map(String::from).to_vec();
        let p = spawn_with(&s, Launch::Program { program: "printf".into(), args }, None, vec![]);
        p.exit_code();
        assert!(p.text().contains(r#"a b|it's|$HOME|"q"|"#), "output: {:?}", p.text());
    }

    #[test]
    fn shell_settings_are_dropped_only_when_started_from_claude_code() {
        assert!(!launcher_vars(false).contains(&"GIT_EDITOR"), "a user's own GIT_EDITOR is kept");
        assert!(launcher_vars(true).contains(&"GIT_EDITOR"));
        assert!(LAUNCHER_ENV.iter().all(|k| launcher_vars(false).contains(k)), "session markers always go");
    }

    #[test]
    fn the_markers_of_raterm_tabs_are_dropped_too() {
        // A misag started from a tab of raterm, its old name, during the switch.
        for k in ["RATERM_TAB", "RATERM_DB", "RATERM_WRAPPER"] {
            assert!(launcher_vars(false).contains(&k), "{k} is kept");
        }
    }

    #[test]
    fn sets_extra_environment() {
        let s = sessions();
        let env = vec![("MISAG_TAB".to_string(), "tab-7".to_string())];
        let p = spawn_with(&s, Launch::Command("echo tab=$MISAG_TAB".into()), None, env);
        p.exit_code();
        assert!(p.text().contains("tab=tab-7"), "output: {:?}", p.text());
    }

    #[test]
    fn resize_reaches_the_process() {
        let s = sessions();
        let p = spawn(&s, "read _; stty size");
        s.resize(p.id, 100, 30).unwrap();
        s.write(p.id, b"\n".to_vec()).unwrap();
        p.exit_code();
        assert!(p.text().contains("30 100"), "output: {:?}", p.text());
    }

    #[test]
    fn kill_all_ends_every_session() {
        let s = sessions();
        let a = spawn(&s, "echo ready; sleep 60");
        let b = spawn(&s, "echo ready; sleep 60");
        a.wait_for("ready");
        b.wait_for("ready");
        s.kill_all();
        a.exit_code();
        b.exit_code();
        assert!(s.write(a.id, b"x".to_vec()).is_err());
        assert!(s.write(b.id, b"x".to_vec()).is_err());
    }

    #[test]
    fn on_app_exit_survivors_are_force_killed_before_returning() {
        let s = sessions();
        let stubborn = spawn(&s, "trap '' HUP; echo ready; sleep 60");
        stubborn.wait_for("ready");
        let start = Instant::now();
        s.kill_all_and_wait();
        assert!(start.elapsed() >= Duration::from_millis(300), "waited for the grace period");
        // Already SIGKILLed: a background kill would only fire 300 ms from now.
        assert!(stubborn.exit.recv_timeout(Duration::from_millis(200)).is_ok(), "killed before returning");
    }

    #[test]
    fn on_app_exit_a_normal_session_does_not_delay_quitting() {
        let s = sessions();
        let p = spawn(&s, "echo ready; sleep 60");
        p.wait_for("ready");
        let start = Instant::now();
        s.kill_all_and_wait();
        assert!(start.elapsed() < Duration::from_millis(250), "took {:?}", start.elapsed());
        p.exit_code();
    }

    #[test]
    fn a_session_whose_frontend_is_gone_is_killed() {
        let s = sessions();
        let (tx, exited) = mpsc::channel();
        let req = SpawnRequest { launch: Launch::Command("echo first; sleep 60".into()), ..req() };
        // The receiver refuses the first output, as a closed webview would.
        let id = s.spawn(req, |_| false, move |code| tx.send(code).unwrap()).unwrap();
        let start = Instant::now();
        while s.write(id, vec![]).is_ok() {
            assert!(start.elapsed() < TIMEOUT, "session was not cleaned up");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(start.elapsed() < Duration::from_secs(5), "sleep 60 should have been killed");
        assert!(exited.try_recv().is_err(), "nobody is listening, so no exit is reported");
    }

    #[test]
    fn tells_posix_shells_apart() {
        assert!(is_posix_shell("/bin/bash"));
        assert!(is_posix_shell("/usr/bin/zsh"));
        assert!(is_posix_shell("sh"));
        assert!(!is_posix_shell("/usr/bin/fish"));
        assert!(!is_posix_shell("/bin/busybox"), "busybox needs an applet name");
        assert!(!is_posix_shell("/bin/tcsh"));
    }

    #[test]
    fn programs_under_other_shells_need_the_trampoline() {
        let claude = || SpawnRequest { launch: Launch::Program { program: "claude".into(), args: vec![] }, ..req() };
        let s = Sessions::new("/usr/bin/fish", Duration::from_millis(300));
        let err = s.spawn(claude(), |_| true, |_| {}).unwrap_err();
        assert!(matches!(&err, SpawnError::Failed(m) if m.contains("no misag binary")), "{err}");
        assert!(!quotable_everywhere("/opt/it's/misag"));
        assert!(quotable_everywhere("/opt/my apps/misag"));

        let unquotable = Sessions::new("/usr/bin/fish", Duration::from_millis(300)).with_trampoline("/opt/it's/misag".into());
        let err = unquotable.spawn(claude(), |_| true, |_| {}).unwrap_err();
        assert!(matches!(&err, SpawnError::Failed(m) if m.contains("install misag elsewhere")), "{err}");
    }

    #[test]
    fn extra_environment_cannot_override_the_terminal_type() {
        let s = sessions();
        let env = vec![("TERM".to_string(), "dumb".to_string())];
        let p = spawn_with(&s, Launch::Command("echo term=$TERM".into()), None, env);
        p.exit_code();
        assert!(p.text().contains("term=xterm-256color"), "output: {:?}", p.text());
    }

    #[test]
    fn runs_in_requested_directory() {
        let s = sessions();
        let p = spawn_in(&s, "pwd", Some("/tmp"));
        p.exit_code();
        assert!(p.text().contains("/tmp"), "output: {:?}", p.text());
    }

    #[test]
    fn rejects_missing_directory() {
        let s = sessions();
        let req = SpawnRequest { cwd: Some("/definitely/not/here".into()), ..req() };
        let err = s.spawn(req, |_| true, |_| {}).unwrap_err();
        assert_eq!(err, SpawnError::CwdMissing("/definitely/not/here".into()));
        assert!(matches!(resolve_cwd(Some("/etc/hostname".into())), Err(SpawnError::Failed(_))), "a file is not missing");
    }

    #[test]
    fn an_inaccessible_directory_is_not_treated_as_missing() {
        if crate::test_support::running_as_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::temp_dir("locked");
        let locked = dir.join("locked");
        std::fs::create_dir_all(locked.join("project")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = cwd_or_home(Some(locked.join("project").to_string_lossy().into_owned()));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(result, Err(SpawnError::Failed(m)) if m.contains("cannot use directory")));
    }

    #[test]
    fn a_blank_command_runs_a_login_shell() {
        let s = sessions();
        let p = spawn_with(&s, Launch::Command("   ".into()), None, vec![]);
        s.write(p.id, b"echo shell-ok; exit\n".to_vec()).unwrap();
        p.exit_code();
        assert!(p.text().contains("shell-ok"), "output: {:?}", p.text());
    }

    #[test]
    fn falls_back_to_home_only_for_a_missing_directory() {
        assert_eq!(cwd_or_home(Some("/definitely/not/here".into())), Ok((None, true)));
        assert_eq!(cwd_or_home(Some("/tmp".into())), Ok((Some("/tmp".into()), false)));
        assert_eq!(cwd_or_home(None), Ok((None, false)));
        assert!(cwd_or_home(Some("/etc/hostname".into())).is_err(), "a file is not a missing directory");
    }

    #[test]
    fn live_paths_lose_the_deleted_suffix_only_when_it_is_not_real() {
        let dir = crate::test_support::temp_dir("live-path");
        let gone = dir.join("project (deleted)");
        assert_eq!(live_path(gone.clone()), dir.join("project"));
        std::fs::create_dir(&gone).unwrap();
        assert_eq!(live_path(gone.clone()), gone, "a directory really named that way is kept");
    }

    #[test]
    fn expands_tilde_in_cwd() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(resolve_cwd(Some("~".into())).unwrap(), Some(home.clone()));
        assert_eq!(resolve_cwd(None).unwrap(), Some(home.clone()));
        assert_eq!(resolve_cwd(Some("  ".into())).unwrap(), Some(home));
        assert!(resolve_cwd(Some("~nobody-xyz".into())).is_err());
    }

    #[test]
    fn expand_tilde_only_touches_the_home_prefix() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/src"), format!("{home}/src"));
        for kept in ["~user", "~x/y", "/a/~", "rel"] {
            assert_eq!(expand_tilde(kept), kept);
        }
    }
}
