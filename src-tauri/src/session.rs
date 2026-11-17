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
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Drained by a per-session writer thread, so a PTY that stops reading
    /// its input never blocks a command or another session.
    input: Sender<Vec<u8>>,
    /// Bytes sent to `input` whose write to the PTY has not finished (see `INPUT_LIMIT`).
    queued: Arc<AtomicUsize>,
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
            let foreground = lock(&self.master).process_group_leader();
            Some(std::thread::spawn(move || {
                if exited.wait(grace) {
                    return;
                }
                // SAFETY (the kills below): plain syscalls. The child is not reaped yet
                // (`exited` is false), so its pid, and therefore its process group, still exist.
                let groups = [Some(pid as libc::pid_t), foreground.filter(|&fg| fg > 0 && fg != pid as libc::pid_t)];
                for group in groups.into_iter().flatten() {
                    if unsafe { libc::kill(-group, libc::SIGKILL) } == -1 {
                        let e = std::io::Error::last_os_error();
                        // The group may have ended in the meantime.
                        if e.raw_os_error() != Some(libc::ESRCH) {
                            eprintln!("misag: killing process group {group} failed: {e}");
                        }
                    }
                }
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
        let (mut reader, mut writer, end) = {
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
                master: Mutex::new(pair.master),
                input,
                queued: queued.clone(),
                killer: Mutex::new(child.clone_killer()),
                pid: child.process_id(),
                exited: exited.clone(),
                #[cfg(windows)]
                job,
            }),
        );

        // Ends when the session is removed (sender dropped), the PTY closes, or (on
        // Unix) the child exits, even while a job holds the PTY without reading it.
        std::thread::spawn(move || {
            for data in input_rx {
                let written = writer.write_all(&data).and_then(|_| writer.flush());
                queued.fetch_sub(data.len(), Ordering::Relaxed);
                if let Err(e) = written {
                    #[cfg(unix)]
                    let ended = pty_io::ended(&e);
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
        // the child wrote last (on Unix, until the PTY is quiet; on Windows, until the
        // console, closed by dropping the session, has passed on its last output).
        let waiter = {
            let exited = exited.clone();
            #[cfg(windows)]
            let map = self.map.clone();
            std::thread::spawn(move || {
                let code = match child.wait() {
                    Ok(status) => status.exit_code() as i32,
                    Err(e) => {
                        eprintln!("misag: session {id} wait failed: {e}");
                        -1
                    }
                };
                exited.set();
                #[cfg(unix)]
                drop(end);
                #[cfg(windows)]
                {
                    let session = lock(&map).remove(&id);
                    drop(session);
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
            // As a closed tab is: hung up, then its process groups killed after the
            // grace period, so its jobs go too. Gone from the map already if the tab
            // was closed meanwhile, which killed it the same way.
            if detached {
                let session = lock(&map).remove(&id);
                if let Some(s) = session {
                    s.kill(grace);
                }
            }
            let code = waiter.join().unwrap_or(-1);
            lock(&map).remove(&id);
            if !detached {
                on_exit(code);
            }
        });

        Ok(id)
    }

    /// Queues input for the session; never blocks on the PTY. Fails while at least
    /// `INPUT_LIMIT` bytes wait for the program to read them; a write of any size is
    /// taken below that, so the queue can go past the limit by one write.
    pub fn write(&self, id: u32, data: Vec<u8>) -> Result<(), String> {
        let s = self.get(id)?;
        let len = data.len();
        // Added up front, so writers at once cannot all slip under the limit, and judged
        // by the count before this write, so a large write is refused only on a full queue.
        let before = s.queued.fetch_add(len, Ordering::Relaxed);
        if before >= INPUT_LIMIT {
            s.queued.fetch_sub(len, Ordering::Relaxed);
            return Err(format!(
                "its program is not reading input ({before} bytes are still waiting); interrupt it or close the tab"
            ));
        }
        s.input.send(data).map_err(|_| {
            s.queued.fetch_sub(len, Ordering::Relaxed);
            format!("session {id} is closed")
        })
    }

    /// Like `spawn`, clamps 0 columns or rows to 1, which programs cannot draw in.
    pub fn resize(&self, id: u32, cols: u16, rows: u16) -> Result<(), String> {
        let s = self.get(id)?;
        lock(&s.master)
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

    /// The programs that may run in the session's foreground, most likely first: the
    /// program a shell is running, such as `claude` typed at its prompt; empty when
    /// only the shell runs. On Windows, which has no foreground process group, the
    /// candidates from the shell's process tree (see `proc_tree`).
    /// An error means it could not be told (no such session, or the query failed),
    /// which callers must not read as "nothing in the foreground".
    #[cfg(windows)]
    pub fn foreground(&self, id: u32) -> Result<Vec<u32>, String> {
        let pid = self.get(id)?.pid.ok_or_else(|| format!("session {id} has no process id"))?;
        let all = crate::win::processes().map_err(|e| format!("session {id}: cannot list processes: {e}"))?;
        Ok(crate::proc_tree::foreground_candidates(pid, &all))
    }

    /// The same, from the PTY's foreground process group: its leader, if that is not
    /// the session process itself.
    #[cfg(unix)]
    pub fn foreground(&self, id: u32) -> Result<Vec<u32>, String> {
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
/// non-blocking, waiting in `poll` on the PTY and on a pipe whose write end the
/// child's waiter drops once the child has exited.
#[cfg(unix)]
mod pty_io {
    use std::fs::File;
    use std::io::{Error, ErrorKind, Read, Result, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// After the child exits, output is read until the PTY has been quiet this long
    /// (what the child wrote last can take a moment to arrive), ...
    const DRAIN_QUIET: Duration = Duration::from_millis(100);
    /// ... but no longer than this, in case a job keeps writing.
    const DRAIN_MAX: Duration = Duration::from_secs(1);

    /// The reader, the writer, and the end of the pipe to drop once the child exited.
    pub fn open(master: RawFd) -> Result<(Reader, Writer, OwnedFd)> {
        // SAFETY: plain calls on a descriptor the caller keeps open; results are checked.
        let (reader, writer, ended, end) = unsafe {
            let flags = libc::fcntl(master, libc::F_GETFL);
            if flags < 0 || libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(Error::last_os_error());
            }
            let dup = || {
                let fd = libc::fcntl(master, libc::F_DUPFD_CLOEXEC, 0);
                if fd < 0 { Err(Error::last_os_error()) } else { Ok(File::from(OwnedFd::from_raw_fd(fd))) }
            };
            let (reader, writer) = (dup()?, dup()?);
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
            (reader, writer, Arc::new(ended), end)
        };
        Ok((Reader { file: reader, ended: ended.clone(), ended_at: None }, Writer { file: writer, ended }, end))
    }

    /// Whether a write failed because the child had exited.
    pub fn ended(e: &Error) -> bool {
        e.kind() == ErrorKind::BrokenPipe
    }

    /// Waits up to `timeout` (None: for good) for `fd` to be ready for `events`, or
    /// for `ended` to close. Returns (fd ready, ended).
    fn wait(fd: RawFd, events: libc::c_short, ended: Option<&OwnedFd>, timeout: Option<Duration>) -> Result<(bool, bool)> {
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
        if fds[0].revents & libc::POLLNVAL != 0 {
            return Err(Error::from_raw_os_error(libc::EBADF));
        }
        Ok((fds[0].revents != 0, fds[1].revents != 0))
    }

    pub struct Reader {
        file: File,
        ended: Arc<OwnedFd>,
        /// When the child was seen to have exited.
        ended_at: Option<Instant>,
    }

    impl Read for Reader {
        /// Like a blocking read, with 0 (end of file) once the child has exited and
        /// its output is drained.
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            loop {
                let timeout = match self.ended_at {
                    Some(at) if at.elapsed() >= DRAIN_MAX => return Ok(0),
                    Some(_) => Some(DRAIN_QUIET),
                    None => None,
                };
                let ended = self.ended_at.is_none().then_some(&*self.ended);
                let (ready, now_ended) = wait(self.file.as_raw_fd(), libc::POLLIN, ended, timeout)?;
                if now_ended {
                    self.ended_at = Some(Instant::now());
                    continue;
                }
                if !ready {
                    return Ok(0);
                }
                match self.file.read(buf) {
                    Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                    result => return result,
                }
            }
        }
    }

    pub struct Writer {
        file: File,
        ended: Arc<OwnedFd>,
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
                let foreground = try_lock(&session.master).and_then(|master| master.process_group_leader());
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

    #[test]
    fn a_background_job_does_not_keep_the_tab_open() {
        let s = sessions();
        // The job ignores the hang-up and holds the terminal open after the shell exits.
        let p = spawn(&s, "(trap '' HUP; exec sleep 30) & echo bg:$!; i=0; while [ $i -lt 2000 ]; do echo line$i; i=$((i+1)); done; echo last-line");
        let start = Instant::now();
        let code = p.exit.recv_timeout(Duration::from_secs(5));
        let job: u32 = p.text().split("bg:").nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap();
        // SAFETY: plain syscall; the job is this test's own process.
        unsafe { libc::kill(job as libc::pid_t, libc::SIGKILL) };
        assert_eq!(code, Ok(0), "the tab ended only after {:?}", start.elapsed());
        assert!(p.text().contains("last-line"), "the shell's last output was cut off: {:?}", p.text());
        assert!(s.write(p.id, b"x".to_vec()).is_err(), "the session is gone");
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
