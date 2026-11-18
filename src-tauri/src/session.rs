//! PTY session management, independent of Tauri so it can be tested directly.

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
#[cfg(unix)]
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// How long a closed session's processes get to exit after the hang-up (SIGHUP on
/// Unix; on Windows their console closing) before they are killed.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// How long `Sessions::type_at_prompt` waits for the shell's prompt by default before
/// giving up, typing nothing.
pub const PROMPT_WAIT: Duration = Duration::from_secs(15);

/// How many bytes of input may wait for a session's program to read them. Far
/// more than anyone types or pastes while a program is busy, so in practice only
/// input to a program that has stopped reading is refused. A refused write returns
/// an error, which the tab shows.
pub const INPUT_LIMIT: usize = 8 * 1024 * 1024;

/// What a session runs: on Unix always through the user's login shell, so PATH and
/// the rest of their environment are set up; on Windows, which has no login shells,
/// through PowerShell, or a program directly.
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
    /// None (or blank) for the home directory; a leading `~` is expanded.
    pub cwd: Option<String>,
    pub launch: Launch,
    /// Extra environment, set before the terminal variables so it cannot override them.
    pub env: Vec<(String, String)>,
}

struct Session {
    /// For resizing and, on Unix, reading the foreground process group; None once
    /// the console is closed (Windows, after the child exited). On Unix the reader
    /// and writer set O_NONBLOCK on the file description this shares (see
    /// `pty_io`), so it must not be used for I/O there.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    /// Drained by a per-session writer thread, so a PTY that stops reading
    /// its input never blocks a command or another session.
    input: Sender<Vec<u8>>,
    /// Bytes sent to `input` whose write to the PTY has not finished (see `INPUT_LIMIT`).
    queued: Arc<AtomicUsize>,
    /// While a command waits for the shell's prompt (`Sessions::type_at_prompt`):
    /// the person's input held back to follow it, counted in `queued` (see `write`).
    held: Mutex<Option<Vec<Vec<u8>>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pid: Option<u32>,
    /// Set once the child has been reaped, so SIGKILL never hits a reused pid.
    exited: Arc<ExitFlag>,
    /// The tab's processes on Windows, which has no process groups to signal.
    #[cfg(windows)]
    job: Option<Arc<crate::win::Job>>,
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
    #[cfg(unix)]
    fn wait(&self, timeout: Duration) -> bool {
        let guard = lock(&self.done);
        let (done, _) = self.changed.wait_timeout_while(guard, timeout, |done| !*done).unwrap_or_else(|e| e.into_inner());
        *done
    }
}

impl Session {
    /// Counts `len` bytes into `queued`, or refuses them; see `Sessions::write`.
    fn reserve(&self, len: usize) -> Result<(), String> {
        // Added up front, so writers at once cannot all slip under the limit, and judged
        // by the count before this write, so a large write is refused only on a full queue.
        let before = self.queued.fetch_add(len, Ordering::Relaxed);
        if before >= INPUT_LIMIT {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            return Err(format!(
                "its program is not reading input ({before} bytes are still waiting); interrupt it or close the tab"
            ));
        }
        Ok(())
    }

    /// Hands input `reserve` counted to the writer thread.
    fn push(&self, id: u32, data: Vec<u8>) -> Result<(), String> {
        let len = data.len();
        self.input.send(data).map_err(|_| {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            format!("session {id} is closed")
        })
    }

    /// Queues input, ahead of any held back (see `write`).
    fn send(&self, id: u32, data: Vec<u8>) -> Result<(), String> {
        self.reserve(data.len())?;
        self.push(id, data)
    }

    /// Queues the person's input; see `Sessions::write`. While a command waits for
    /// the shell's prompt (`Sessions::type_at_prompt`), input is held back to follow
    /// it, so keys typed ahead during startup do not run into it, unless it is meant
    /// for what reads the terminal now: in canonical mode a write with a key that
    /// ends or interrupts the line (the answer to a startup question, or Ctrl+C on
    /// a slow profile), sent with what was held before it; otherwise anything typed
    /// into a program the profile started in the foreground (tmux, a picker, a
    /// passphrase prompt). So keys typed ahead without Enter wait for the command, and
    /// a line typed ahead runs before it, whole.
    fn write(&self, id: u32, data: Vec<u8>) -> Result<(), String> {
        let mut held = lock(&self.held);
        let Some(waiting) = held.as_mut() else { return self.send(id, data) };
        self.reserve(data.len())?;
        #[cfg(unix)]
        let now = self.terminal().map_or(true, |t| t.is_none_or(|t| t.takes_now(&data)));
        #[cfg(windows)]
        let now = true;
        waiting.push(data);
        if now {
            self.push_all(id, std::mem::take(waiting))?;
        }
        Ok(())
    }

    /// Ends the wait of `Sessions::type_at_prompt`: queues `typed` (its command, or
    /// None when it gave up), then the input held back meanwhile. Fails when
    /// `typed` is refused or the session is closed.
    #[cfg(unix)]
    fn release(&self, id: u32, typed: Option<Vec<u8>>) -> Result<(), String> {
        let mut held = lock(&self.held);
        let waiting = held.take().unwrap_or_default();
        let typed = typed.map_or(Ok(()), |data| self.send(id, data));
        let rest = self.push_all(id, waiting);
        typed.and(rest)
    }

    /// Pushes input `reserve` already counted, in order; once one is refused (the
    /// session closed), the rest are uncounted too.
    fn push_all(&self, id: u32, all: Vec<Vec<u8>>) -> Result<(), String> {
        let mut result = Ok(());
        for data in all {
            if result.is_err() {
                self.queued.fetch_sub(data.len(), Ordering::Relaxed);
            } else {
                result = self.push(id, data);
            }
        }
        result
    }

    /// The terminal's mode and foreground process group; None when the session
    /// has no terminal any more (it is closing).
    #[cfg(unix)]
    fn terminal(&self) -> Result<Option<Terminal>, String> {
        let master = lock(&self.master);
        let Some(master) = master.as_ref() else { return Ok(None) };
        let fd = master.as_raw_fd().ok_or("the PTY has no descriptor")?;
        // SAFETY: a plain-data struct the kernel fills, for a descriptor the locked master keeps open.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
            return Err(format!("cannot read the terminal's mode: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: plain syscall on the same descriptor. The shell leads a session of its
        // own, so its process group's id is its pid.
        let foreground = unsafe { libc::tcgetpgrp(fd) };
        let shell_foreground = foreground > 0 && self.pid.is_some_and(|pid| pid as libc::pid_t == foreground);
        // Disabled keys read as 0 on Linux and 0xff on macOS.
        let keys = [libc::VINTR, libc::VQUIT, libc::VSUSP, libc::VEOF, libc::VEOL].map(|k| t.c_cc[k]);
        let line_keys = b"\r\n".iter().copied().chain(keys.into_iter().filter(|&k| k != 0 && k != 0xff)).collect();
        Ok(Some(Terminal { canonical: t.c_lflag & libc::ICANON != 0, shell_foreground, line_keys }))
    }

    /// Hangs the child up, then kills what is left of it after `grace`: on Unix
    /// SIGHUP, then SIGKILL to its process groups, in a thread that ends as soon as
    /// the child is reaped; on Windows the shell ends at once and the tab's job
    /// object gets the grace period (see `Job::end_within`).
    fn kill(&self, grace: Duration) -> Option<std::thread::JoinHandle<()>> {
        if let Err(e) = lock(&self.killer).kill() {
            eprintln!("misag: hanging up pid {:?} failed: {e}", self.pid);
        }
        let exited = self.exited.clone();
        #[cfg(unix)]
        {
            let Some(pid) = self.pid else {
                eprintln!("misag: a session has no pid, so its process groups cannot be killed");
                return None;
            };
            // The foreground job may sit in its own process group under job control.
            let foreground = lock(&self.master).as_ref().and_then(|m| m.process_group_leader());
            Some(std::thread::spawn(move || {
                if exited.wait(grace) {
                    return;
                }
                // The child is not reaped yet (`exited` is false), so its pid, and
                // therefore its process group and session, still exist.
                let pid = pid as libc::pid_t;
                signal_groups(&session_groups(pid, foreground), libc::SIGKILL);
            }))
        }
        // The shell is gone already (Windows has no hang-up signal). Once the session
        // drops its console, what the shell started gets the grace period to exit;
        // the job's processes are counted, not the shell's, which ended at once.
        #[cfg(windows)]
        {
            let _ = exited;
            let job = self.job.clone()?;
            Some(std::thread::spawn(move || job.end_within(grace)))
        }
    }
}

/// How often `type_at_prompt` looks whether the shell shows its prompt.
#[cfg(unix)]
const PROMPT_POLL: Duration = Duration::from_millis(20);

/// `type_at_prompt`'s wait: types `data` once the shell shows its prompt, and
/// returns what became of it. Sends the held-back input after it, or on its own
/// when giving up.
#[cfg(unix)]
fn type_when_at_prompt(session: &std::sync::Weak<Session>, id: u32, data: Vec<u8>, wait: Duration) -> Result<(), String> {
    let closed = || "the tab closed before its shell showed its prompt".to_string();
    let start = std::time::Instant::now();
    loop {
        // Gone from the map and dropped: closed, with the held input.
        let Some(s) = session.upgrade() else { return Err(closed()) };
        let gave_up = match s.terminal() {
            Ok(Some(t)) if t.at_prompt() => return s.release(id, Some(data)),
            Ok(Some(_)) if start.elapsed() >= wait => {
                eprintln!("misag: session {id}: its shell did not show its prompt within {wait:?}, so the command waiting for it is not typed");
                format!("its shell did not show its prompt within {} s", wait.as_secs_f32())
            }
            Ok(Some(_)) => {
                drop(s);
                std::thread::sleep(PROMPT_POLL);
                continue;
            }
            // Its terminal went with the child (Windows only, so far): the tab is closing.
            Ok(None) => closed(),
            Err(e) => {
                eprintln!("misag: session {id}: cannot tell whether its shell shows its prompt, so the command waiting for it is not typed: {e}");
                format!("cannot tell whether its shell shows its prompt: {e}")
            }
        };
        if let Err(e) = s.release(id, None) {
            eprintln!("misag: session {id}: {e}");
        }
        return Err(gave_up);
    }
}

/// What `Session::terminal` reads from a session's terminal.
#[cfg(unix)]
struct Terminal {
    /// In canonical (line) mode: the kernel collects a line before a program reads it.
    canonical: bool,
    /// The shell's own process group, not a job it started, is in the foreground.
    shell_foreground: bool,
    /// The keys that end or interrupt a line in canonical mode (Enter, Ctrl+C,
    /// Ctrl+D and the like, as the terminal sets them).
    line_keys: Vec<u8>,
}

#[cfg(unix)]
impl Terminal {
    /// Whether the shell shows its prompt: its line editor (readline, zle, fish's)
    /// reads only there, with the terminal out of canonical (line) mode, and the
    /// shell's own process group in the foreground. So neither a `read` in its
    /// startup files (canonical mode) nor a program they start, which job control
    /// puts in a process group of its own (tmux or zellij attaching, a curses
    /// passphrase prompt, an fzf picker, all in raw mode), counts. Not told apart: a
    /// raw-mode read in the shell itself (zsh's `read -k`, bash's `read -n 1`), and
    /// a program the profile runs without job control (in the shell's process
    /// group) or `exec`s in the shell's place (`exec tmux`) that sets raw mode. A shell without a line editor, such as dash,
    /// stays in canonical mode, so never counts.
    fn at_prompt(&self) -> bool {
        !self.canonical && self.shell_foreground
    }

    /// Whether the person's `data` goes to the terminal at once while a command
    /// waits for the prompt, rather than after it (see `Session::write`).
    fn takes_now(&self, data: &[u8]) -> bool {
        if self.canonical {
            data.iter().any(|b| self.line_keys.contains(b))
        } else {
            !self.shell_foreground
        }
    }
}

/// How long after killing what a session's exited child left running its terminal
/// may still be held before that is logged (see `end_leftovers`).
#[cfg(unix)]
const KILL_CHECK: Duration = Duration::from_millis(250);

/// Sends `signal` to each process group in `groups`, which the caller made sure
/// are a session's (see `session_groups`).
#[cfg(unix)]
fn signal_groups(groups: &[libc::pid_t], signal: libc::c_int) {
    for &group in groups {
        // SAFETY: plain syscall.
        if unsafe { libc::kill(-group, signal) } == -1 {
            let e = std::io::Error::last_os_error();
            // The group may have ended in the meantime.
            if e.raw_os_error() != Some(libc::ESRCH) {
                eprintln!("misag: signalling process group {group} failed: {e}");
            }
        }
    }
}

/// The session id of process `pid`.
#[cfg(unix)]
fn session_of(pid: libc::pid_t) -> std::io::Result<libc::pid_t> {
    // SAFETY: plain syscall.
    match unsafe { libc::getsid(pid) } {
        -1 => Err(std::io::Error::last_os_error()),
        sid => Ok(sid),
    }
}

/// The process groups of the session that unreaped child `sid` leads: its own
/// group, `foreground` if that group's leader is in the session, and every group
/// with a process in the session, such as the `&` jobs of an interactive shell,
/// which job control puts in groups of their own. A process that left the session
/// (`setsid`) is not among them.
///
/// The child is not reaped, so its pid stays its session's id and no other
/// session can have it; a group listed here could end and its id be reused only
/// in the moment before the caller signals it.
#[cfg(unix)]
fn session_groups(sid: libc::pid_t, foreground: Option<libc::pid_t>) -> Vec<libc::pid_t> {
    let mut groups = vec![sid];
    let mut unexpected = None;
    let mut in_session = |pid: libc::pid_t| match session_of(pid) {
        Ok(of) => of == sid,
        Err(e) => {
            // ESRCH: the process ended in the meantime.
            if e.raw_os_error() != Some(libc::ESRCH) {
                unexpected.get_or_insert(e);
            }
            false
        }
    };
    groups.extend(foreground.filter(|&fg| fg > 0 && in_session(fg)));
    match proc_info::pids() {
        Ok(pids) => {
            for pid in pids.into_iter().filter(|&pid| pid != sid && in_session(pid)) {
                // SAFETY: plain syscall.
                let group = unsafe { libc::getpgid(pid) };
                if group > 0 {
                    groups.push(group);
                }
            }
        }
        Err(e) => eprintln!(
            "misag: cannot list processes, so only the main and foreground process groups of session {sid} are signalled: {e}"
        ),
    }
    if let Some(e) = unexpected {
        eprintln!("misag: cannot tell the session of some processes, so some of session {sid}'s process groups may not be signalled: {e}");
    }
    groups.sort_unstable();
    groups.dedup();
    groups
}

/// After a session's child `pid` exited, unreaped: ends what still holds its
/// terminal the way closing the tab does. Nothing, if the terminal is let go within
/// `pty_io::DRAIN_MAX`; else a hang-up to the process groups of the child's
/// session (see `session_groups`), then SIGKILL to them, listed again, once `grace`
/// passes with the terminal still held.
#[cfg(unix)]
fn end_leftovers(exit: &pty_io::Exit, pid: libc::pid_t, grace: Duration) {
    match exit.hung_up_within(pty_io::DRAIN_MAX) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => eprintln!("misag: cannot tell whether anything still holds the terminal of exited pid {pid}, so its session is hung up: {e}"),
    }
    signal_groups(&session_groups(pid, exit.foreground()), libc::SIGHUP);
    match exit.hung_up_within(grace) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            eprintln!(
                "misag: cannot tell whether the session of exited pid {pid} let go of its terminal after the hang-up, so it is killed after the full {grace:?}: {e}"
            );
            std::thread::sleep(grace);
        }
    }
    // Listed again: the foreground may be another group by now, and a job may have started.
    signal_groups(&session_groups(pid, exit.foreground()), libc::SIGKILL);
    match exit.hung_up_within(KILL_CHECK) {
        Ok(true) => {}
        Ok(false) => eprintln!(
            "misag: the terminal of exited pid {pid} is still held after its session's process groups were killed: a process that left the session (setsid) keeps running and holding it"
        ),
        Err(e) => eprintln!("misag: cannot tell whether killing the session of exited pid {pid} let go of its terminal: {e}"),
    }
}

/// Reaps a session's exited child and flags it as reaped when dropped, so that
/// happens even when ending what it left running panics or cannot start.
#[cfg(unix)]
struct Reap {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    exited: Arc<ExitFlag>,
    id: u32,
}

#[cfg(unix)]
impl Drop for Reap {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("misag: session {}: ending what its shell left running panicked; reaping the shell anyway", self.id);
        }
        reap(&mut *self.child, self.id);
        self.exited.set();
    }
}

/// Waits for a session's child to exit and reaps it; returns its exit code (-1 if
/// waiting failed).
fn reap(child: &mut dyn portable_pty::Child, id: u32) -> i32 {
    match child.wait() {
        Ok(status) => status.exit_code() as i32,
        Err(e) => {
            eprintln!("misag: session {id} wait failed: {e}");
            -1
        }
    }
}

/// Why a session could not start. `CwdMissing` (the directory no longer exists)
/// lets a restore fall back to the home directory for that reason only.
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
#[cfg(unix)]
const POSIX_SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "mksh", "ash", "yash"];

#[cfg(unix)]
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
#[cfg(unix)]
fn quotable_everywhere(path: &str) -> bool {
    !path.contains(['\'', '\\', '!', '\n'])
}

pub struct Sessions {
    shell: String,
    /// misag's own binary, which runs `Launch::Program` under non-POSIX shells.
    trampoline: Option<String>,
    kill_grace: Duration,
    /// See `type_at_prompt`; Windows types at once.
    #[cfg_attr(windows, allow(dead_code))]
    prompt_wait: Duration,
    /// Inherited variables removed from every tab (see `launcher_vars`).
    strip_env: Vec<&'static str>,
    next_id: AtomicU32,
    map: Arc<Mutex<HashMap<u32, Arc<Session>>>>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new(default_shell(), KILL_GRACE)
    }
}

/// The user's shell: `$SHELL`, else `/bin/sh`.
#[cfg(unix)]
pub(crate) fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| {
        eprintln!("misag: $SHELL is not set, falling back to /bin/sh");
        "/bin/sh".into()
    })
}

/// PowerShell 7 (`pwsh`) when installed, else the Windows PowerShell every Windows
/// has. Only absolute PATH entries count, so a `pwsh.exe` in misag's own working
/// folder is never picked.
#[cfg(windows)]
fn default_shell() -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pwsh = std::env::split_paths(&path).filter(|dir| dir.is_absolute()).map(|dir| dir.join("pwsh.exe")).find(|exe| exe.is_file());
    pwsh.map_or_else(|| "powershell.exe".into(), |exe| exe.to_string_lossy().into_owned())
}

/// The user's home directory (`$HOME` on Unix, `%USERPROFILE%` on Windows), if it
/// is UTF-8.
pub fn home_dir() -> Option<String> {
    std::env::home_dir().and_then(|dir| dir.into_os_string().into_string().ok()).filter(|dir| !dir.is_empty())
}

/// What PowerShell runs before a terminal tab's first prompt: a prompt that also
/// moves the process's current directory to PowerShell's location, which `cd`
/// (`Set-Location`) alone does not, so the tab follows it (see `Sessions::cwd`).
#[cfg(any(windows, test))]
const POWERSHELL_FOLLOW_CD: &str = "$global:__misagPrompt = $function:prompt; \
function global:prompt { $l = Get-Location; \
if ($l.Provider.Name -eq 'FileSystem') { [Environment]::CurrentDirectory = $l.ProviderPath }; \
& $global:__misagPrompt }";

/// PowerShell's arguments for `launch` (a program runs without it): a terminal tab
/// stays open (`-NoExit`) with its prompt following `cd`; a command runs and exits.
#[cfg(any(windows, test))]
fn powershell_args(launch: &Launch) -> Vec<&str> {
    match launch {
        Launch::Command(c) if !c.trim().is_empty() => vec!["-NoLogo", "-Command", c],
        _ => vec!["-NoLogo", "-NoExit", "-Command", POWERSHELL_FOLLOW_CD],
    }
}

impl Sessions {
    pub fn new(shell: impl Into<String>, kill_grace: Duration) -> Self {
        Self {
            shell: shell.into(),
            trampoline: None,
            kill_grace,
            prompt_wait: PROMPT_WAIT,
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

    /// How long `type_at_prompt` waits for the shell's prompt (`PROMPT_WAIT` by default).
    pub fn with_prompt_wait(mut self, wait: Duration) -> Self {
        self.prompt_wait = wait;
        self
    }

    /// What runs `launch`, before the environment and directory are set: the login
    /// shell, with a program run through `exec` (or misag's trampoline).
    #[cfg(unix)]
    fn command(&self, launch: &Launch) -> Result<CommandBuilder, SpawnError> {
        let shell = &self.shell;
        let mut cmd = CommandBuilder::new(shell);
        match launch {
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
        Ok(cmd)
    }

    /// What runs `launch` on Windows: a program directly (every program inherits the
    /// user's environment there), anything else in PowerShell.
    #[cfg(windows)]
    fn command(&self, launch: &Launch) -> Result<CommandBuilder, SpawnError> {
        if let Launch::Program { program, args } = launch {
            let mut cmd = CommandBuilder::new(program);
            cmd.args(args);
            return Ok(cmd);
        }
        let mut cmd = CommandBuilder::new(&self.shell);
        cmd.args(powershell_args(launch));
        Ok(cmd)
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
        #[cfg(unix)]
        let (mut reader, mut writer, mut exit) = {
            let fd = pair.master.as_raw_fd().ok_or_else(|| "the PTY has no descriptor".to_string())?;
            pty_io::open(fd).map_err(|e| format!("PTY reader and writer: {e}"))?
        };
        #[cfg(windows)]
        let (mut reader, mut writer) = (
            pair.master.try_clone_reader().map_err(|e| format!("PTY reader: {e:#}"))?,
            pair.master.take_writer().map_err(|e| format!("PTY writer: {e:#}"))?,
        );

        let mut cmd = self.command(&req.launch)?;
        // Launcher state first; `req.env` may still set any of these on purpose.
        for key in &self.strip_env {
            cmd.env_remove(key);
        }
        // Caller variables before the terminal type, so they cannot override it.
        for (key, value) in &req.env {
            cmd.env(key, value);
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
        #[cfg(windows)]
        let job = child.process_id().and_then(|pid| {
            crate::win::Job::for_process(pid)
                .map_err(|e| eprintln!("misag: cannot group the processes of pid {pid}, so closing its tab may leave some running: {e}"))
                .ok()
                .map(Arc::new)
        });

        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let exited = Arc::new(ExitFlag::default());
        let (input, input_rx) = mpsc::channel::<Vec<u8>>();
        let queued = Arc::new(AtomicUsize::new(0));
        lock(&self.map).insert(
            id,
            Arc::new(Session {
                master: Mutex::new(Some(pair.master)),
                input,
                queued: queued.clone(),
                held: Mutex::default(),
                killer: Mutex::new(child.clone_killer()),
                pid: child.process_id(),
                exited: exited.clone(),
                #[cfg(windows)]
                job,
            }),
        );

        // Ends when the session is removed (sender dropped) or a write fails, such as
        // because the PTY closed or (on Unix) the child exited, which ends a write
        // waiting on a job that holds the PTY without reading it.
        std::thread::spawn(move || {
            for data in input_rx {
                let written = writer.write_all(&data).and_then(|_| writer.flush());
                queued.fetch_sub(data.len(), Ordering::Relaxed);
                if let Err(e) = written {
                    #[cfg(unix)]
                    let ended = writer.ended(&e);
                    #[cfg(windows)]
                    let ended = false;
                    if !ended {
                        eprintln!("misag: session {id} write failed: {e}");
                    }
                    break;
                }
            }
        });

        // The session ends when its child exits, not when the PTY closes: a
        // background job can hold the PTY open for good. The reader then gets what
        // the child wrote last (until the PTY is quiet), and what still holds the
        // PTY is ended as closing the tab would, without holding up the tab's end.
        // The child stays unreaped (a zombie) until then, so `exited` still means
        // reaped and no signal can reach a reused pid.
        #[cfg(unix)]
        let waiter = {
            let exited = exited.clone();
            let grace = self.kill_grace;
            let pid = child.process_id().map(|pid| pid as libc::pid_t);
            std::thread::spawn(move || match pid.map(|pid| (pid, pty_io::wait_exit(pid))) {
                Some((pid, Ok(code))) => {
                    exit.child_exited();
                    let reaped = Reap { child, exited, id };
                    // On failure the closure is dropped, and with it `reaped`, which reaps the child.
                    let ended = std::thread::Builder::new().name(format!("session {id} leftovers")).spawn(move || {
                        end_leftovers(&exit, pid, grace);
                        drop(reaped);
                    });
                    if let Err(e) = ended {
                        eprintln!("misag: session {id}: cannot start a thread to end what its shell left running, so that keeps running: {e}");
                    }
                    code
                }
                failed => {
                    match failed {
                        Some((pid, Err(e))) => {
                            eprintln!("misag: session {id}: waiting for pid {pid} to exit failed, reaping it instead: {e}")
                        }
                        _ => eprintln!("misag: session {id} has no process id, so what it leaves running is not ended when it exits"),
                    }
                    let code = reap(&mut *child, id);
                    exited.set();
                    drop(exit);
                    code
                }
            })
        };
        // On Windows the reader gets the last output once the console closes, which
        // the waiter does as closing the tab would: it asks console programs to
        // exit, and the job's processes left after the grace period are ended. The
        // session stays in the map until the reader is done; one already gone from
        // it was closed, which did all this.
        #[cfg(windows)]
        let waiter = {
            let exited = exited.clone();
            let grace = self.kill_grace;
            let map = self.map.clone();
            std::thread::spawn(move || {
                let code = reap(&mut *child, id);
                exited.set();
                let session = lock(&map).get(&id).cloned();
                if let Some(s) = session {
                    let console = lock(&s.master).take();
                    drop(console);
                    // Holds the job (whose last handle closing kills it at once) until then.
                    if let Some(job) = s.job.clone() {
                        std::thread::spawn(move || job.end_within(grace));
                    }
                }
                code
            })
        };

        let map = self.map.clone();
        let grace = self.kill_grace;
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
                        // EIO is how Unix reports the slave side closing.
                        #[cfg(unix)]
                        let closed = e.raw_os_error() == Some(libc::EIO);
                        // Windows reports it as a broken pipe.
                        #[cfg(windows)]
                        let closed = e.kind() == ErrorKind::BrokenPipe;
                        if !closed {
                            eprintln!("misag: session {id} read failed: {e}");
                        }
                        break;
                    }
                }
            }
            #[cfg(unix)]
            if reader.cut_off() {
                eprintln!(
                    "misag: session {id}'s output was cut off: something it left running kept writing for {:?} after it exited",
                    pty_io::DRAIN_MAX
                );
            }
            // As a closed tab is: hung up, then its process groups killed after the
            // grace period, so its jobs go too. Gone from the map already if the tab
            // was closed meanwhile, which killed it the same way.
            if detached {
                let session = lock(&map).remove(&id);
                if let Some(s) = session {
                    s.kill(grace);
                }
            }
            let code = waiter.join().unwrap_or_else(|_| {
                eprintln!("misag: session {id}'s exit waiter panicked, so its exit code is unknown");
                -1
            });
            lock(&map).remove(&id);
            if !detached {
                on_exit(code);
            }
        });

        Ok(id)
    }

    /// Queues input for the session; never blocks on the PTY. Fails while at least
    /// `INPUT_LIMIT` bytes wait for the program to read them; a write of any size is
    /// taken below that, so the queue can go past the limit by one write. While
    /// `type_at_prompt` waits, input may be held back to follow its command (see
    /// `Session::write`); held input counts towards the limit too.
    pub fn write(&self, id: u32, data: Vec<u8>) -> Result<(), String> {
        self.get(id)?.write(id, data)
    }

    /// Types `data` at a login shell's prompt once the shell shows it (see
    /// `Terminal::at_prompt`), so neither a question its startup files ask nor a
    /// program they start (tmux, a picker) takes it. Returns at once, with the
    /// typing queued; `on_outcome` is called once it is typed, or with why it never
    /// will be: the prompt did not show within the prompt wait (`PROMPT_WAIT` unless
    /// `with_prompt_wait` changed it; a shell without a line editor, such as dash,
    /// never shows one this can tell, so nothing is typed in it), the tab closed, or
    /// the input was refused. Nothing is typed blind. Meanwhile what the person types
    /// is held back to follow `data` (see `Session::write`), and sent as it was once
    /// the wait ends either way. Fails, typing nothing, when the session is gone, a
    /// command already waits for its prompt, or the waiting thread cannot start.
    #[cfg(unix)]
    pub fn type_at_prompt(&self, id: u32, data: Vec<u8>, on_outcome: impl FnOnce(Result<(), String>) + Send + 'static) -> Result<(), String> {
        let session = self.get(id)?;
        {
            let mut held = lock(&session.held);
            if held.is_some() {
                return Err(format!("a command already waits for the prompt of session {id}"));
            }
            *held = Some(Vec::new());
        }
        // Weak, so a closed tab is not kept open while this waits.
        let weak = Arc::downgrade(&session);
        let wait = self.prompt_wait;
        let spawned = std::thread::Builder::new().name(format!("session {id} prompt")).spawn(move || {
            on_outcome(type_when_at_prompt(&weak, id, data, wait));
        });
        if let Err(e) = spawned {
            // Sends what was typed meanwhile; fails only once the session is closed.
            let _ = session.release(id, None);
            return Err(format!("cannot start a thread to wait for its prompt: {e}"));
        }
        Ok(())
    }

    /// The same on Windows, where it is typed at once (`on_outcome` is called before
    /// this returns): PowerShell has no terminal mode that tells its prompt apart.
    #[cfg(windows)]
    pub fn type_at_prompt(&self, id: u32, data: Vec<u8>, on_outcome: impl FnOnce(Result<(), String>) + Send + 'static) -> Result<(), String> {
        self.write(id, data)?;
        on_outcome(Ok(()));
        Ok(())
    }

    /// Like `spawn`, clamps 0 columns or rows to 1, which programs cannot draw in.
    pub fn resize(&self, id: u32, cols: u16, rows: u16) -> Result<(), String> {
        let s = self.get(id)?;
        lock(&s.master)
            .as_ref()
            .ok_or_else(|| format!("session {id} has exited"))?
            .resize(PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 })
            .map_err(|e| format!("session {id} resize failed: {e:#}"))
    }

    /// Returns immediately: the child is hung up now and force-killed after the grace period.
    pub fn kill(&self, id: u32) {
        if let Some(s) = lock(&self.map).remove(&id) {
            s.kill(self.kill_grace);
        }
    }

    /// The session process's current directory.
    /// On Unix, for a Claude tab under a POSIX login shell that is Claude's directory,
    /// since the shell `exec`s into it; under fish or csh it is the shell's, which
    /// stays in the directory the tab started in. On Windows a terminal tab's
    /// PowerShell keeps it at its location (see `POWERSHELL_FOLLOW_CD`).
    /// On Linux a directory deleted while in use reads as `<path> (deleted)`; that
    /// suffix is dropped, so a restore finds the path missing and falls back to the
    /// home directory.
    pub fn cwd(&self, id: u32) -> Option<String> {
        let pid = self.get(id).ok()?.pid?;
        let path = match proc_info::cwd(pid) {
            Ok(path) => path,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("misag: cannot read the directory of process {pid}: {e}");
                }
                return None;
            }
        };
        path.into_os_string().into_string().map_err(|p| eprintln!("misag: non-UTF-8 directory {p:?}")).ok()
    }

    /// On Windows, which has no foreground process group, the programs that may run
    /// in the session's foreground, most likely first: the candidates from the
    /// shell's process tree (see `proc_tree`) in `all`, one snapshot of the running
    /// processes (or why it could not be taken), shared by every tab of a poll; empty
    /// when only the shell runs. An error means it could not be told (no such
    /// session, the session exited, no process id, or no snapshot), which callers
    /// must not read as "nothing in the foreground".
    #[cfg(windows)]
    pub fn foreground_in(&self, id: u32, all: &Result<Vec<crate::proc_tree::Process>, String>) -> Result<Vec<u32>, String> {
        let s = self.get(id)?;
        if lock(&s.master).is_none() {
            return Err(format!("session {id} has exited"));
        }
        let pid = s.pid.ok_or_else(|| format!("session {id} has no process id"))?;
        let all = all.as_ref().map_err(|e| format!("session {id}: {e}"))?;
        Ok(crate::proc_tree::foreground_candidates(pid, all))
    }

    /// The programs that may run in the session's foreground: the program a shell
    /// is running, such as `claude` typed at its prompt (the PTY's foreground process
    /// group's leader, if that is not the session process itself); empty when only
    /// the shell runs. An error means it could not be told (no such session, or the
    /// query failed), which callers must not read as "nothing in the foreground".
    #[cfg(unix)]
    pub fn foreground(&self, id: u32) -> Result<Vec<u32>, String> {
        let s = self.get(id)?;
        let master = lock(&s.master);
        let fd = master.as_ref().and_then(|m| m.as_raw_fd()).ok_or_else(|| format!("session {id} has no PTY descriptor"))?;
        // SAFETY: plain syscall on a descriptor the locked master keeps open.
        let leader = unsafe { libc::tcgetpgrp(fd) };
        if leader <= 0 {
            let e = format!("session {id}: cannot read the foreground process: {}", std::io::Error::last_os_error());
            eprintln!("misag: {e}");
            return Err(e);
        }
        let leader = leader as u32;
        Ok((Some(leader) != s.pid).then_some(leader).into_iter().collect())
    }

    /// The process's name, such as `claude` (on Windows its program's file name
    /// without `.exe`).
    pub fn process_name(pid: u32) -> Option<String> {
        proc_info::name(pid)
    }

    /// The command line process `pid` was started with, if it can be read.
    pub fn process_args(pid: u32) -> Option<Vec<String>> {
        let args = proc_info::args(pid)?;
        (!args.is_empty()).then_some(args)
    }

    /// Hangs up every session, force-killing survivors after the grace period in
    /// the background. Returns the threads doing that.
    pub fn kill_all(&self) -> Vec<std::thread::JoinHandle<()>> {
        let all: Vec<_> = lock(&self.map).drain().map(|(_, s)| s).collect();
        all.iter().filter_map(|s| s.kill(self.kill_grace)).collect()
    }

    /// For app exit: like `kill_all`, but waits until every session has exited or
    /// been force-killed, since background threads die with the process. Returns
    /// at once when every child exits on the hang-up; waits the grace period at most.
    pub fn kill_all_and_wait(&self) {
        for handle in self.kill_all() {
            let _ = handle.join();
        }
    }

    /// What the panic hook kills the sessions' processes with (see `panic_log`).
    pub fn killer(&self) -> SessionKiller {
        SessionKiller { map: self.map.clone() }
    }

    /// Clones the session out so the map lock is never held during PTY work.
    fn get(&self, id: u32) -> Result<Arc<Session>, String> {
        lock(&self.map).get(&id).cloned().ok_or_else(|| format!("no session {id}"))
    }
}

/// The PTY's reader and writer on Unix, which stop once the child has exited even
/// while a background job keeps the PTY open: through their own descriptors,
/// waiting in `poll` on the PTY and on a pipe whose write end the child's waiter
/// drops once the child has exited. `open` sets O_NONBLOCK on the PTY master's
/// file description, which every descriptor of it shares, `Session.master`'s
/// included; so only these do I/O on it.
#[cfg(unix)]
mod pty_io {
    use std::fs::File;
    use std::io::{Error, ErrorKind, Read, Result, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// After the child exits, output is read until the PTY has been quiet this long
    /// (what the child wrote last can take a moment to arrive), ...
    const DRAIN_QUIET: Duration = Duration::from_millis(100);
    /// ... but no longer than this, in case a job keeps writing. What still holds
    /// the PTY this long after the child exited is ended (see `Exit::hung_up_within`).
    pub const DRAIN_MAX: Duration = Duration::from_secs(1);
    /// How long `Exit::hung_up_within` pauses after its poll woke for output only.
    const HANG_UP_PAUSE: Duration = Duration::from_millis(50);

    /// The reader, the writer, and what the child's waiter needs once it exited.
    pub fn open(master: RawFd) -> Result<(Reader, Writer, Exit)> {
        // SAFETY: plain calls on a descriptor the caller keeps open; results are
        // checked. Each `OwnedFd` below takes a fresh descriptor that F_DUPFD_CLOEXEC
        // or pipe2/pipe just returned, so it owns it alone and closes it once.
        let (reader, writer, watch, ended, end) = unsafe {
            let flags = libc::fcntl(master, libc::F_GETFL);
            if flags < 0 || libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(Error::last_os_error());
            }
            let dup = || {
                let fd = libc::fcntl(master, libc::F_DUPFD_CLOEXEC, 0);
                if fd < 0 { Err(Error::last_os_error()) } else { Ok(OwnedFd::from_raw_fd(fd)) }
            };
            let (reader, writer, watch) = (File::from(dup()?), File::from(dup()?), dup()?);
            // Close-on-exec, or a program started meanwhile could hold the write end
            // open, and the session would never end. PTY children close it anyway.
            let mut fds = [0; 2];
            #[cfg(target_os = "linux")]
            let made = libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) == 0;
            #[cfg(not(target_os = "linux"))]
            let made = libc::pipe(fds.as_mut_ptr()) == 0;
            if !made {
                return Err(Error::last_os_error());
            }
            let (ended, end) = (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]));
            #[cfg(not(target_os = "linux"))]
            for fd in fds {
                if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                    return Err(Error::last_os_error());
                }
            }
            (reader, writer, watch, Arc::new(ended), end)
        };
        let foreground = Arc::new(AtomicI32::new(0));
        let reader = Reader { file: reader, ended: ended.clone(), ended_at: None, cut_off: false, foreground: foreground.clone(), told_foreground_failure: false };
        Ok((reader, Writer { file: writer, ended }, Exit { end: Some(end), master: watch, foreground }))
    }

    /// Waits for child `pid` to exit without reaping it, so its pid (and with it its
    /// process group and session) cannot be reused until the caller reaps it. Returns
    /// its exit code, as portable-pty reports it: 1 when a signal ended it.
    pub fn wait_exit(pid: libc::pid_t) -> Result<i32> {
        // SAFETY: an all-zero siginfo_t is valid; waitid fills it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: plain syscall writing into `info`.
            if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) } == 0 {
                break;
            }
            let e = Error::last_os_error();
            if e.kind() != ErrorKind::Interrupted {
                return Err(e);
            }
        }
        // SAFETY: waitid filled `info` for a child that exited.
        let status = unsafe { info.si_status() };
        Ok(if info.si_code == libc::CLD_EXITED { status } else { 1 })
    }

    /// What the child's waiter holds: the pipe end it drops once the child exited,
    /// and its own descriptor of the PTY, to tell whether anything still holds it.
    pub struct Exit {
        end: Option<OwnedFd>,
        master: OwnedFd,
        foreground: Arc<AtomicI32>,
    }

    impl Exit {
        /// Tells the reader and the writer that the child has exited (dropping
        /// `Exit` does too).
        pub fn child_exited(&mut self) {
            self.end = None;
        }

        /// Whether, within `timeout`, nothing holds the PTY's terminal side any more:
        /// a hang-up (or error) on the master. It is polled for input too, as macOS
        /// watches nothing on a descriptor polled for no events and so would never
        /// report its hang-up; nothing is read here, so output stays the reader's,
        /// and a wake-up for output alone is waited out in short pauses until the
        /// deadline instead of spinning on output nobody reads any more.
        pub fn hung_up_within(&self, timeout: Duration) -> Result<bool> {
            let deadline = Instant::now() + timeout;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let (revents, _) = wait(self.master.as_raw_fd(), libc::POLLIN | libc::POLLHUP, None, Some(left))?;
                if revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    return Ok(true);
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(false);
                }
                std::thread::sleep(left.min(HANG_UP_PAUSE));
            }
        }

        /// The terminal's foreground process group the last time output was read,
        /// if one was seen. It may have ended since; the caller must check it.
        pub fn foreground(&self) -> Option<libc::pid_t> {
            Some(self.foreground.load(Ordering::Relaxed)).filter(|&pg| pg > 0)
        }
    }

    /// Waits up to `timeout` (None: for good) for `fd` to have any event of
    /// `events`, or a hang-up or error, or for `ended` to close. Returns (fd's
    /// events, ended); (0, false) when the timeout passed.
    fn wait(fd: RawFd, events: libc::c_short, ended: Option<&OwnedFd>, timeout: Option<Duration>) -> Result<(libc::c_short, bool)> {
        let mut fds = [
            libc::pollfd { fd, events, revents: 0 },
            libc::pollfd { fd: ended.map_or(-1, AsRawFd::as_raw_fd), events: libc::POLLIN, revents: 0 },
        ];
        let timeout = timeout.map_or(-1, |t| t.as_millis().try_into().unwrap_or(libc::c_int::MAX));
        loop {
            // SAFETY: two valid pollfd entries; a negative fd is skipped.
            match unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) } {
                n if n >= 0 => break,
                _ if Error::last_os_error().kind() == ErrorKind::Interrupted => continue,
                _ => return Err(Error::last_os_error()),
            }
        }
        if (fds[0].revents | fds[1].revents) & libc::POLLNVAL != 0 {
            return Err(Error::from_raw_os_error(libc::EBADF));
        }
        Ok((fds[0].revents, fds[1].revents != 0))
    }

    pub struct Reader {
        file: File,
        ended: Arc<OwnedFd>,
        /// When the child was seen to have exited.
        ended_at: Option<Instant>,
        /// Whether reading stopped at `DRAIN_MAX` while output was still coming.
        cut_off: bool,
        /// The terminal's foreground process group, as last seen (see `Exit::foreground`).
        foreground: Arc<AtomicI32>,
        /// Whether reading the foreground process group failed and that was logged.
        told_foreground_failure: bool,
    }

    impl Reader {
        /// Whether output was still coming when reading stopped at `DRAIN_MAX`.
        pub fn cut_off(&self) -> bool {
            self.cut_off
        }
    }

    impl Read for Reader {
        /// Like a blocking read. Once the child has exited, returns 0 (end of file)
        /// when the PTY has been quiet for `DRAIN_QUIET` or `DRAIN_MAX` has passed,
        /// whichever comes first; an EIO error when nothing holds the PTY any more.
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            loop {
                let timeout = match self.ended_at {
                    Some(at) => {
                        let left = DRAIN_MAX.saturating_sub(at.elapsed());
                        if left.is_zero() {
                            // Only reached after a read: a quiet PTY returns below.
                            self.cut_off = true;
                            return Ok(0);
                        }
                        Some(left.min(DRAIN_QUIET))
                    }
                    None => None,
                };
                let ended = self.ended_at.is_none().then_some(&*self.ended);
                let (events, now_ended) = wait(self.file.as_raw_fd(), libc::POLLIN, ended, timeout)?;
                if now_ended {
                    self.ended_at = Some(Instant::now());
                    continue;
                }
                if events == 0 {
                    return Ok(0);
                }
                match self.file.read(buf) {
                    Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Ok(n) => {
                        // SAFETY: plain syscall on a descriptor `file` owns.
                        let group = unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) };
                        if group > 0 {
                            self.foreground.store(group, Ordering::Relaxed);
                        } else if group < 0 && !self.told_foreground_failure {
                            let e = Error::last_os_error();
                            // EIO, ENOTTY: the terminal side has closed.
                            if !matches!(e.raw_os_error(), Some(libc::EIO | libc::ENOTTY)) {
                                self.told_foreground_failure = true;
                                eprintln!(
                                    "misag: cannot read a terminal's foreground process group, so a foreground job may be missed when its shell exits: {e}"
                                );
                            }
                        }
                        return Ok(n);
                    }
                    result => return result,
                }
            }
        }
    }

    pub struct Writer {
        file: File,
        ended: Arc<OwnedFd>,
    }

    impl Writer {
        /// Whether a failed write means the session has ended rather than a fault:
        /// the child has exited, or nothing holds the PTY's terminal side any more.
        pub fn ended(&self, e: &Error) -> bool {
            e.kind() == ErrorKind::BrokenPipe
                || e.raw_os_error() == Some(libc::EIO)
                || matches!(wait(-1, 0, Some(&self.ended), Some(Duration::ZERO)), Ok((_, true)))
        }
    }

    impl Write for Writer {
        /// Like a blocking write, failing with `BrokenPipe` once the child has exited.
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            loop {
                let (_, ended) = wait(self.file.as_raw_fd(), libc::POLLOUT, Some(&self.ended), None)?;
                if ended {
                    return Err(Error::new(ErrorKind::BrokenPipe, "the session's process has exited"));
                }
                match self.file.write(buf) {
                    Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                    result => return result,
                }
            }
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }
}

/// Locks a mutex, recovering the data if a previous holder panicked. That is only
/// possible when the panic unwinds (a debug build, on a thread outside webkit's
/// callbacks); release builds abort.
pub(crate) fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Locks a mutex without waiting, recovering the data like `lock`; None if another
/// holder has it.
#[cfg(unix)]
fn try_lock<T: ?Sized>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// Kills every session's processes at once, from the panic hook. It never waits
/// for a lock: one the panicking thread holds would never be released.
pub struct SessionKiller {
    map: Arc<Mutex<HashMap<u32, Arc<Session>>>>,
}

/// What `SessionKiller::kill_now` did, for the panic log.
#[derive(Debug, PartialEq)]
pub enum KillReport {
    /// The sessions were locked, so none was killed.
    Locked,
    /// Sessions whose processes were signalled, and running ones skipped because
    /// their state was locked or they have no pid. Exited sessions count as neither.
    Killed { killed: usize, skipped: usize },
    /// Windows: each tab's job object kills its processes when the app ends.
    LeftToJobs,
}

impl std::fmt::Display for KillReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KillReport::Locked => write!(f, "the sessions were locked, so no tab's processes were killed"),
            KillReport::Killed { killed, skipped } => {
                write!(f, "killed the processes of {killed} tab(s), skipped {skipped} whose state was locked")
            }
            KillReport::LeftToJobs => write!(f, "each tab's job object kills its processes"),
        }
    }
}

impl SessionKiller {
    /// Best effort: on Unix SIGKILL to each session's process groups, skipping a
    /// session whose state is locked or whose child was reaped (its pid may be
    /// reused); the whole step is skipped if the sessions are locked. Locks a panic
    /// poisoned are used anyway. On Windows nothing: each tab's job object kills
    /// its processes when the app ends.
    pub fn kill_now(&self) -> KillReport {
        #[cfg(unix)]
        {
            let Some(map) = try_lock(&self.map) else { return KillReport::Locked };
            let (mut killed, mut skipped) = (0, 0);
            for session in map.values() {
                let (Some(pid), Some(exited)) = (session.pid, try_lock(&session.exited.done)) else {
                    skipped += 1;
                    continue;
                };
                if *exited {
                    continue;
                }
                let foreground = try_lock(&session.master).and_then(|master| master.as_ref().and_then(|m| m.process_group_leader()));
                // SAFETY: plain syscalls. Best effort, as in `Session::kill`: a child
                // reaped but not yet flagged as exited can still be signalled, so its
                // pid could in theory have been reused.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    if let Some(fg) = foreground.filter(|&fg| fg > 0 && fg != pid as libc::pid_t) {
                        libc::kill(-fg, libc::SIGKILL);
                    }
                }
                killed += 1;
            }
            KillReport::Killed { killed, skipped }
        }
        #[cfg(windows)]
        KillReport::LeftToJobs
    }
}

/// What the kernel says about a process, through `/proc` on Linux and libproc and
/// sysctl on macOS.
#[cfg(target_os = "linux")]
mod proc_info {
    use std::path::PathBuf;

    pub fn cwd(pid: u32) -> std::io::Result<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).map(super::live_path)
    }

    /// Every process's pid (threads other than the main one are not listed).
    pub fn pids() -> std::io::Result<Vec<libc::pid_t>> {
        Ok(std::fs::read_dir("/proc")?.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok()).collect())
    }

    pub fn name(pid: u32) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|name| name.trim_end().to_owned())
    }

    pub fn args(pid: u32) -> Option<Vec<String>> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        // Each argument ends in a NUL; a process that is exiting reads as empty.
        Some(super::nul_terminated(&raw))
    }
}

#[cfg(windows)]
mod proc_info {
    pub use crate::win::{args, cwd};

    pub fn name(pid: u32) -> Option<String> {
        crate::win::process_name(pid)
    }
}

#[cfg(target_os = "macos")]
mod proc_info {
    use std::ffi::{c_char, c_int, c_void, CStr};
    use std::io::{Error, ErrorKind};
    use std::path::PathBuf;

    fn pid_arg(pid: u32) -> std::io::Result<c_int> {
        c_int::try_from(pid).map_err(|_| Error::new(ErrorKind::NotFound, "no such process"))
    }

    pub fn cwd(pid: u32) -> std::io::Result<PathBuf> {
        let pid = pid_arg(pid)?;
        // SAFETY: a plain-data struct the kernel fills, of the size it is told.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as c_int;
        let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut info).cast::<c_void>(), size) };
        if n != size {
            let e = Error::last_os_error();
            // ESRCH: gone; EPERM: someone else's, which a tab's process never is.
            return Err(if e.raw_os_error() == Some(libc::ESRCH) || n == 0 { Error::new(ErrorKind::NotFound, e) } else { e });
        }
        // SAFETY: vip_path is a NUL-terminated MAXPATHLEN buffer, split in rows only for old compilers.
        let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast::<c_char>()) };
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
    }

    /// Every process's pid.
    pub fn pids() -> std::io::Result<Vec<libc::pid_t>> {
        // SAFETY: a null buffer asks only for (an estimate of) how many there are.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        let n = usize::try_from(n).map_err(|_| Error::last_os_error())?;
        // Room for processes started since.
        let mut pids: Vec<libc::pid_t> = vec![0; n + 64];
        let size = c_int::try_from(pids.len() * std::mem::size_of::<libc::pid_t>()).map_err(Error::other)?;
        // SAFETY: the kernel writes at most `size` bytes; returns how many pids it wrote.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), size) };
        let n = usize::try_from(n).map_err(|_| Error::last_os_error())?;
        pids.truncate(n);
        Ok(pids)
    }

    pub fn name(pid: u32) -> Option<String> {
        let pid = pid_arg(pid).ok()?;
        let mut buf = [0u8; 2 * libc::MAXCOMLEN + 1];
        // SAFETY: the kernel writes at most the given size into buf.
        let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast::<c_void>(), buf.len() as u32) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    }

    pub fn args(pid: u32) -> Option<Vec<String>> {
        let pid = pid_arg(pid).ok()?;
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // SAFETY: a null buffer asks only for the size.
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: the kernel writes at most `size` bytes and updates it.
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast::<c_void>(), &mut size, std::ptr::null_mut(), 0) } != 0 {
            return None;
        }
        buf.truncate(size);
        super::procargs2(&buf)
    }
}

/// The arguments in a NUL-terminated list such as `/proc/<pid>/cmdline`.
#[cfg(any(target_os = "linux", test))]
fn nul_terminated(raw: &[u8]) -> Vec<String> {
    let args: Vec<String> = raw.split(|&b| b == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    args.split_last().map_or(args.clone(), |(_, rest)| rest.to_vec())
}

/// The arguments in a macOS `KERN_PROCARGS2` buffer: the argument count, the
/// executable's path, NUL padding, then that many NUL-terminated arguments.
#[cfg(any(target_os = "macos", test))]
fn procargs2(buf: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let rest = &buf[4..];
    let path_end = rest.iter().position(|&b| b == 0)?;
    let start = path_end + rest[path_end..].iter().position(|&b| b != 0).unwrap_or(rest.len() - path_end);
    // Only NUL-terminated pieces are arguments; a count larger than what is there
    // yields what is there.
    let args = rest[start..]
        .split_inclusive(|&b| b == 0)
        .filter_map(|a| a.strip_suffix(&[0]))
        .take(usize::try_from(argc).ok()?)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    Some(args)
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
    match (dir.strip_prefix('~'), home_dir().ok_or(())) {
        (Some(rest), Ok(h)) if rest.is_empty() || rest.starts_with(std::path::is_separator) => format!("{h}{rest}"),
        _ => dir.to_owned(),
    }
}

/// Resolves the requested working directory, expanding a leading `~`.
fn resolve_cwd(cwd: Option<String>) -> Result<Option<String>, SpawnError> {
    let Some(dir) = cwd.filter(|d| !d.trim().is_empty()) else { return Ok(home_dir()) };
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

    #[test]
    fn powershell_tabs_follow_cd_and_commands_exit() {
        let shell = powershell_args(&Launch::LoginShell);
        assert_eq!(shell[..3], ["-NoLogo", "-NoExit", "-Command"]);
        assert!(shell[3].contains("[Environment]::CurrentDirectory = $l.ProviderPath"));
        assert!(shell[3].contains("& $global:__misagPrompt"), "the person's own prompt still runs");
        assert_eq!(powershell_args(&Launch::Command(" ".into())), shell, "a blank command is a terminal");
        assert_eq!(powershell_args(&Launch::Command("git status".into())), ["-NoLogo", "-Command", "git status"]);
    }

    #[test]
    fn reads_macos_procargs2() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/local/bin/claude\0\0\0\0claude\0--resume\0SHELL=/bin/zsh\0");
        assert_eq!(procargs2(&buf), Some(vec!["claude".to_string(), "--resume".to_string()]), "environment after argc is left out");
        assert_eq!(procargs2(&[1, 0]), None, "truncated");
        let mut empty = 0i32.to_ne_bytes().to_vec();
        empty.extend_from_slice(b"/bin/sh\0\0");
        assert_eq!(procargs2(&empty), Some(vec![]));
    }

    #[test]
    fn reads_nul_terminated_arguments() {
        assert_eq!(nul_terminated(b"sleep\x0060\0"), vec!["sleep".to_string(), "60".to_string()]);
        assert!(nul_terminated(b"").is_empty(), "an exiting process reads as empty");
        assert_eq!(nul_terminated(b"a\0\0b\0"), vec!["a".to_string(), String::new(), "b".to_string()], "an empty argument stays");
    }

    #[test]
    fn procargs2_trusts_neither_its_count_nor_its_length() {
        let mut short = 3i32.to_ne_bytes().to_vec();
        short.extend_from_slice(b"/bin/x\0x\0");
        assert_eq!(procargs2(&short), Some(vec!["x".to_string()]), "fewer arguments than counted");
        let mut negative = (-1i32).to_ne_bytes().to_vec();
        negative.extend_from_slice(b"/bin/x\0x\0");
        assert_eq!(procargs2(&negative), None);
        let mut no_nul = 1i32.to_ne_bytes().to_vec();
        no_nul.extend_from_slice(b"/bin/x");
        assert_eq!(procargs2(&no_nul), None);
    }
}

/// Tests that run shells and Unix process calls.
#[cfg(all(test, unix))]
mod unix_tests {
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
        assert_eq!(s.foreground(p.id), Ok(vec![]), "only the shell runs");
        p.wait_for("fg:");
        let text = p.text();
        let pid: u32 = text.split("fg:").nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        assert_eq!(s.foreground(p.id), Ok(vec![pid]));
        // `fg:` is printed before sh execs sleep.
        let until = Instant::now() + TIMEOUT;
        while Sessions::process_name(pid).as_deref() != Some("sleep") && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Sessions::process_name(pid).as_deref(), Some("sleep"));
        assert_eq!(Sessions::process_args(pid), Some(vec!["sleep".to_string(), "60".to_string()]));
        s.kill(p.id);
        p.exit_code();
        assert!(s.foreground(p.id).is_err(), "gone with the session: unknown, not idle");
        assert_eq!(Sessions::process_args(pid), None, "gone");
    }

    /// SIGKILLs a test's own process when dropped, so a failing test leaves none behind.
    struct KillOnDrop(u32);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            // SAFETY: plain syscall on a process this test started.
            unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
        }
    }

    /// The number printed after `tag` (such as `bg:` before `$!`).
    fn number_after(text: &str, tag: &str) -> u32 {
        let after = text.split(tag).nth(1).unwrap_or_else(|| panic!("no {tag:?} in {text:?}"));
        after.split_whitespace().next().unwrap().parse().unwrap()
    }

    /// That `job`, left by a shell that exited, is killed once the grace period
    /// passes. Except on macOS, which revokes a terminal when the shell leading its
    /// session exits: the job no longer holds it, so it is left running, as one that
    /// let go of the terminal is elsewhere.
    fn assert_killed_after_the_grace_period(job: u32) {
        assert!(Sessions::process_name(job).is_some(), "killed before the tab ended");
        if cfg!(target_os = "macos") {
            std::thread::sleep(Duration::from_secs(2));
            assert!(Sessions::process_name(job).is_some(), "killed, though the terminal was revoked");
            return;
        }
        let took = wait_gone(job, Duration::from_secs(4));
        assert!(took >= Duration::from_millis(300), "killed before the grace period, after {took:?}");
    }

    /// Waits until process `pid` is gone (reaped, not a zombie); returns how long that took.
    fn wait_gone(pid: u32, within: Duration) -> Duration {
        let start = Instant::now();
        while Sessions::process_name(pid).is_some() {
            assert!(start.elapsed() < within, "process {pid} still there after {within:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        start.elapsed()
    }

    #[test]
    fn a_background_job_does_not_keep_the_tab_open() {
        let s = sessions();
        // The job ignores the hang-up and holds the terminal open after the shell exits.
        let p = spawn(&s, "(trap '' HUP; exec sleep 5) & echo bg:$!; i=0; while [ $i -lt 2000 ]; do echo line$i; i=$((i+1)); done; echo last-line");
        p.wait_for("bg:");
        let _job = KillOnDrop(number_after(&p.text(), "bg:"));
        let start = Instant::now();
        let code = p.exit.recv_timeout(Duration::from_secs(4));
        assert_eq!(code, Ok(0), "the tab ended only after {:?}", start.elapsed());
        assert!(p.text().contains("last-line"), "the shell's last output was cut off: {:?}", p.text());
        assert!(s.write(p.id, b"x".to_vec()).is_err(), "the session is gone");
    }

    /// What became of a command typed at the prompt (see `type_resume`).
    type Outcome = Receiver<Result<(), String>>;

    /// Types a resume command the way `pty_spawn` does; the receiver gets its outcome.
    fn type_resume(s: &Sessions, id: u32, data: &[u8]) -> Outcome {
        let (tx, rx) = mpsc::channel();
        s.type_at_prompt(id, data.to_vec(), move |outcome| {
            let _ = tx.send(outcome);
        })
        .unwrap();
        rx
    }

    /// A bash login shell whose `.bash_profile` is `profile`, in a home of its own.
    fn login_bash(s: &Sessions, name: &str, profile: &str) -> Probe {
        let home = crate::test_support::temp_dir(name);
        std::fs::write(home.join(".bash_profile"), profile).unwrap();
        let env = vec![("HOME".to_string(), home.to_string_lossy().into_owned())];
        spawn_with(s, Launch::LoginShell, Some(home.to_str().unwrap()), env)
    }

    #[test]
    fn typed_input_waits_for_the_prompt_past_a_startup_question() {
        let s = Sessions::new("/bin/bash", Duration::from_millis(300));
        let p = login_bash(&s, "startup-question", "read -p 'update? ' answer; echo \"answer=[$answer]\"\n");
        let outcome = type_resume(&s, p.id, b"echo resumed-$((1+1))\r");
        p.wait_for("update? ");
        std::thread::sleep(Duration::from_millis(300));
        s.write(p.id, b"n\r".to_vec()).unwrap();
        p.wait_for("answer=[");
        p.wait_for("resumed-2");
        assert!(p.text().contains("answer=[n]"), "output: {:?}", p.text());
        assert_eq!(outcome.recv_timeout(TIMEOUT), Ok(Ok(())));
        s.kill(p.id);
        p.exit_code();
    }

    #[test]
    fn a_prompt_that_never_comes_types_nothing_and_says_so() {
        let s = Sessions::new("/bin/bash", Duration::from_millis(300)).with_prompt_wait(Duration::from_millis(500));
        let p = login_bash(&s, "unanswered-question", "read -p 'update? ' answer; echo \"answer=[$answer]\"\n");
        let outcome = type_resume(&s, p.id, b"echo resumed-$((1+1))\r");
        p.wait_for("update? ");
        let failed = outcome.recv_timeout(TIMEOUT).expect("no outcome");
        assert!(failed.as_ref().is_err_and(|e| e.contains("did not show its prompt")), "{failed:?}");
        s.write(p.id, b"n\r".to_vec()).unwrap();
        p.wait_for("answer=[n]");
        s.write(p.id, b"echo done-$((2+2))\r".to_vec()).unwrap();
        p.wait_for("done-4");
        assert!(!p.text().contains("resumed"), "typed anyway: {:?}", p.text());
        s.kill(p.id);
        p.exit_code();
    }

    #[test]
    fn a_raw_mode_program_in_the_foreground_is_not_the_prompt() {
        // Started from the profile under job control, so in its own process group, as
        // tmux or a picker would be: it reads raw input for a second and shows what it got.
        let profile = "set -m\nsh -c 'stty raw -echo min 0 time 10; got=$(dd bs=200 count=1 2>/dev/null); stty sane; echo \"child-got=[$got]\"'\n";
        let s = Sessions::new("/bin/bash", Duration::from_millis(300));
        let p = login_bash(&s, "raw-child", profile);
        let outcome = type_resume(&s, p.id, b"echo resumed-$((1+1))\r");
        p.wait_for("child-got=[");
        p.wait_for("resumed-2");
        assert!(p.text().contains("child-got=[]"), "output: {:?}", p.text());
        assert_eq!(outcome.recv_timeout(TIMEOUT), Ok(Ok(())));
        s.kill(p.id);
        p.exit_code();
    }

    #[test]
    fn closing_the_tab_while_waiting_for_the_prompt_ends_the_wait() {
        let s = Sessions::new("/bin/bash", Duration::from_millis(300));
        let p = login_bash(&s, "closed-waiting", "read -p 'update? ' answer\n");
        let outcome = type_resume(&s, p.id, b"echo resumed\r");
        p.wait_for("update? ");
        s.kill(p.id);
        // Well within the 15 s prompt wait.
        let closed = outcome.recv_timeout(Duration::from_secs(2)).expect("still waiting");
        assert!(closed.as_ref().is_err_and(|e| e.contains("closed")), "{closed:?}");
        p.exit_code();
        assert!(!p.text().contains("resumed"), "typed anyway: {:?}", p.text());
    }

    #[test]
    fn keys_held_back_for_the_prompt_count_towards_the_input_limit() {
        let s = Sessions::new("/bin/bash", Duration::from_millis(300));
        let p = login_bash(&s, "held-limit", "read -p 'update? ' answer\n");
        let _outcome = type_resume(&s, p.id, b"echo resumed\r");
        p.wait_for("update? ");
        s.write(p.id, vec![b'x'; INPUT_LIMIT]).unwrap();
        assert!(s.write(p.id, b"y".to_vec()).is_err(), "held input is not counted");
        s.kill(p.id);
        p.exit_code();
    }

    #[test]
    fn keys_typed_while_waiting_for_the_prompt_follow_the_command() {
        let s = Sessions::new("/bin/bash", Duration::from_millis(300));
        let p = login_bash(&s, "typed-ahead", "echo starting; sleep 1\n");
        let outcome = type_resume(&s, p.id, b"echo resumed-$((1+1))\r");
        p.wait_for("starting");
        s.write(p.id, b"echo typed-$((2+1))".to_vec()).unwrap();
        assert_eq!(outcome.recv_timeout(TIMEOUT), Ok(Ok(())));
        s.write(p.id, b"\r".to_vec()).unwrap();
        p.wait_for("typed-3");
        let text = p.text();
        let resumed = text.find("resumed-2").unwrap_or_else(|| panic!("not resumed: {text:?}"));
        assert!(resumed < text.find("typed-3").unwrap(), "typed ahead of the command: {text:?}");
        s.kill(p.id);
        p.exit_code();
    }

    #[test]
    fn a_job_still_holding_the_terminal_when_the_shell_exits_is_killed_after_the_grace_period() {
        let s = sessions();
        let p = spawn(&s, "(trap '' HUP; exec sleep 10) & echo bg:$!; sleep 0.2");
        p.wait_for("bg:");
        let job = number_after(&p.text(), "bg:");
        let _guard = KillOnDrop(job);
        assert_eq!(p.exit_code(), 0);
        assert_killed_after_the_grace_period(job);
    }

    #[test]
    fn a_job_in_its_own_process_group_is_killed_after_the_shell_exits() {
        let s = sessions();
        // An interactive shell, as a terminal tab runs, puts a `&` job in its own
        // process group under job control.
        // +H: no history expansion, which bash 3.2 (macOS) wrongly applies to `$!;`.
        let args = ["--norc", "--noprofile", "+H", "-i"].map(String::from).to_vec();
        let p = spawn_with(&s, Launch::Program { program: "bash".into(), args }, None, vec![]);
        s.write(p.id, b"sh -c \"trap '' HUP; exec sleep 10\" & echo \"b\"g:$!; sleep 0.2; exit\n".to_vec()).unwrap();
        p.wait_for("bg:");
        let job = number_after(&p.text(), "bg:");
        let _guard = KillOnDrop(job);
        // (The terminal echoes the typed line, so the tag is split there.)
        // SAFETY: plain syscalls on a process this test started.
        let group = unsafe { libc::getpgid(job as libc::pid_t) };
        assert_eq!(group, job as libc::pid_t, "the job has its own process group");
        p.exit_code();
        assert_killed_after_the_grace_period(job);
    }

    #[test]
    fn hang_up_is_seen_without_taking_the_readers_output() {
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty writes two descriptors, which this test closes.
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }, 0);
        let (mut reader, _writer, exit) = pty_io::open(master).unwrap();
        // SAFETY: writing a byte buffer to a descriptor this test owns.
        assert_eq!(unsafe { libc::write(slave, b"kept".as_ptr().cast(), 4) }, 4);
        let start = Instant::now();
        assert!(!exit.hung_up_within(Duration::from_millis(300)).unwrap(), "the terminal is still held");
        assert!(start.elapsed() >= Duration::from_millis(300), "gave up early, after {:?}", start.elapsed());
        let mut buf = [0u8; 16];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"kept", "the output was taken");
        // SAFETY: closing descriptors this test owns.
        unsafe { libc::close(slave) };
        let start = Instant::now();
        assert!(exit.hung_up_within(Duration::from_secs(2)).unwrap());
        assert!(start.elapsed() < Duration::from_millis(500), "saw the hang-up only after {:?}", start.elapsed());
        // SAFETY: closing a descriptor this test owns.
        unsafe { libc::close(master) };
    }

    #[test]
    fn a_job_that_keeps_writing_does_not_keep_the_tab_open() {
        let s = sessions();
        let p = spawn(&s, "(trap '' HUP; while :; do echo spam; done) & echo bg:$!; sleep 0.2");
        p.wait_for("bg:");
        let _job = KillOnDrop(number_after(&p.text(), "bg:"));
        let start = Instant::now();
        let code = p.exit.recv_timeout(Duration::from_millis(2500));
        assert_eq!(code, Ok(0), "the tab ended only after {:?}", start.elapsed());
    }

    #[test]
    fn input_queued_for_a_job_that_does_not_read_it_does_not_keep_the_tab_open() {
        let s = sessions();
        let p = spawn(&s, "(trap '' HUP; exec sleep 10) & echo bg:$!; sleep 0.5");
        p.wait_for("bg:");
        let _job = KillOnDrop(number_after(&p.text(), "bg:"));
        // Far more than the PTY input buffer holds, so the writer is blocked when the shell exits.
        for _ in 0..64 {
            s.write(p.id, vec![b'x'; 16 * 1024]).unwrap();
        }
        let start = Instant::now();
        let code = p.exit.recv_timeout(Duration::from_millis(2500));
        assert_eq!(code, Ok(0), "the tab ended only after {:?}", start.elapsed());
    }

    #[test]
    fn a_plain_exit_ends_the_tab_and_reaps_the_shell_at_once() {
        let s = sessions();
        let p = spawn(&s, "echo pid:$$; exit 4");
        let start = Instant::now();
        assert_eq!(p.exit_code(), 4);
        let took = start.elapsed();
        assert!(took < Duration::from_millis(500), "the tab ended only after {took:?}");
        let shell = number_after(&p.text(), "pid:");
        let reaped = wait_gone(shell, Duration::from_secs(2));
        assert!(reaped < Duration::from_millis(200), "the shell was reaped only {reaped:?} after the tab ended");
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
    fn the_panic_killer_kills_at_once_even_when_sighup_is_ignored() {
        let s = sessions();
        let p = spawn(&s, "trap '' HUP; echo ready; sleep 60; echo survived");
        p.wait_for("ready");
        let start = Instant::now();
        assert_eq!(s.killer().kill_now(), KillReport::Killed { killed: 1, skipped: 0 });
        p.exit_code();
        assert!(start.elapsed() < Duration::from_millis(300), "waited for the grace period");
        assert!(!p.text().contains("survived"));
    }

    #[test]
    fn the_panic_killer_recovers_locks_a_panic_poisoned() {
        let s = sessions();
        let p = spawn(&s, "echo ready; sleep 60; echo survived");
        p.wait_for("ready");
        let session = s.get(p.id).unwrap();
        let map = s.map.clone();
        let _ = std::thread::spawn(move || {
            let _map = lock(&map);
            let _exited = lock(&session.exited.done);
            let _master = lock(&session.master);
            panic!("poisoning the session locks");
        })
        .join();
        assert!(s.map.is_poisoned());
        assert_eq!(s.killer().kill_now(), KillReport::Killed { killed: 1, skipped: 0 });
        p.exit_code();
        assert!(!p.text().contains("survived"));
    }

    #[test]
    fn the_panic_killer_counts_a_session_it_skips() {
        let s = sessions();
        let p = spawn(&s, "echo ready; sleep 60");
        p.wait_for("ready");
        let session = s.get(p.id).unwrap();
        let guard = lock(&session.exited.done);
        assert_eq!(s.killer().kill_now(), KillReport::Killed { killed: 0, skipped: 1 });
        drop(guard);
        s.kill(p.id);
        p.exit_code();
    }

    #[test]
    fn the_panic_killer_skips_the_sessions_when_they_are_locked() {
        let s = sessions();
        let p = spawn(&s, "echo ready; sleep 60");
        p.wait_for("ready");
        let guard = lock(&s.map);
        assert_eq!(s.killer().kill_now(), KillReport::Locked);
        drop(guard);
        assert!(!s.get(p.id).unwrap().exited.wait(Duration::from_millis(200)), "killed though the map was locked");
        s.kill(p.id);
        p.exit_code();
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
    fn input_is_refused_while_a_session_does_not_read_it_and_accepted_once_it_does() {
        let s = sessions();
        let p = spawn(&s, "stty raw -echo; echo ready; sleep 1; cat >/dev/null");
        p.wait_for("ready");
        let chunk = 64 * 1024;
        let refused = (0..2 * INPUT_LIMIT / chunk).find_map(|_| s.write(p.id, vec![b'x'; chunk]).err());
        let e = refused.expect("input queued without limit");
        assert!(e.contains("not reading"), "{e}");
        // Once cat drains the queue, input is taken again.
        let start = Instant::now();
        while s.write(p.id, b"y".to_vec()).is_err() {
            assert!(start.elapsed() < TIMEOUT, "the queue never drained");
            std::thread::sleep(Duration::from_millis(20));
        }
        s.kill(p.id);
        p.exit_code();
    }

    #[test]
    fn a_deleted_directory_is_reported_without_the_deleted_suffix() {
        let s = sessions();
        // Canonical, as the kernel reports it (macOS's temp dir is under a symlink).
        let dir = std::fs::canonicalize(crate::test_support::temp_dir("gone")).unwrap();
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
        // macOS's /tmp is a symlink to /private/tmp, which the kernel reports.
        let tmp = std::fs::canonicalize("/tmp").unwrap();
        assert_eq!(s.cwd(p.id).as_deref(), tmp.to_str());
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
    fn resize_to_zero_is_clamped_like_spawn() {
        let s = sessions();
        let p = spawn(&s, "read _; stty size");
        s.resize(p.id, 0, 0).unwrap();
        s.write(p.id, b"\n".to_vec()).unwrap();
        p.exit_code();
        assert!(p.text().contains("1 1"), "output: {:?}", p.text());
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
    fn a_session_whose_frontend_is_gone_loses_its_jobs_too() {
        let s = sessions();
        let (tx, exited) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        // The shell ignores the hang-up, so only killing its process group ends its job.
        let req = SpawnRequest { launch: Launch::Command("trap '' HUP; sleep 60 & echo bg:$!; wait".into()), ..req() };
        let on_data = move |b: &[u8]| {
            lock(&sink).extend_from_slice(b);
            false
        };
        let id = s.spawn(req, on_data, move |code| tx.send(code).unwrap()).unwrap();
        let start = Instant::now();
        while s.write(id, vec![]).is_ok() {
            assert!(start.elapsed() < TIMEOUT, "session was not cleaned up");
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = String::from_utf8_lossy(&lock(&seen)).into_owned();
        let job: u32 = text.split("bg:").nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        while Sessions::process_name(job).is_some() {
            assert!(start.elapsed() < TIMEOUT, "the job {job} outlived its tab");
            std::thread::sleep(Duration::from_millis(20));
        }
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
        let file = crate::test_support::temp_dir("not-a-dir").join("file");
        std::fs::write(&file, "").unwrap();
        assert!(matches!(resolve_cwd(Some(file.to_string_lossy().into_owned())), Err(SpawnError::Failed(_))), "a file is not missing");
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
        let file = crate::test_support::temp_dir("file-not-home").join("file");
        std::fs::write(&file, "").unwrap();
        assert!(cwd_or_home(Some(file.to_string_lossy().into_owned())).is_err(), "a file is not a missing directory");
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
