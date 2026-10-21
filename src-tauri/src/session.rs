//! PTY session management, independent of Tauri so it can be tested directly.

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// How long a closed session gets to exit on SIGHUP before it is sent SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

pub struct SpawnRequest {
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
    pub command: Option<String>,
}

struct Session {
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// Drained by a per-session writer thread, so a PTY that stops reading
    /// its input never blocks a command or another session.
    input: Sender<Vec<u8>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    pid: Option<u32>,
    /// Set once the child has been reaped, so SIGKILL never hits a reused pid.
    exited: Arc<AtomicBool>,
}

impl Session {
    /// Sends SIGHUP, then SIGKILL to the child's process groups if it is still
    /// alive after `grace`.
    fn kill(&self, grace: Duration) {
        if let Err(e) = lock(&self.killer).kill() {
            eprintln!("raterm: SIGHUP to pid {:?} failed: {e}", self.pid);
        }
        let Some(pid) = self.pid else { return };
        // The foreground job may sit in its own process group under job control.
        let foreground = lock(&self.master).process_group_leader();
        let exited = self.exited.clone();
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            if exited.load(Ordering::Acquire) {
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
        });
    }
}

pub struct Sessions {
    shell: String,
    kill_grace: Duration,
    next_id: AtomicU32,
    map: Arc<Mutex<HashMap<u32, Arc<Session>>>>,
}

impl Default for Sessions {
    fn default() -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| {
            eprintln!("raterm: $SHELL is not set, falling back to /bin/sh");
            "/bin/sh".into()
        });
        Self::new(shell, KILL_GRACE)
    }
}

impl Sessions {
    pub fn new(shell: impl Into<String>, kill_grace: Duration) -> Self {
        Self {
            shell: shell.into(),
            kill_grace,
            next_id: AtomicU32::new(0),
            map: Arc::default(),
        }
    }

    /// Starts a session. `on_data` receives output and returns false once nobody
    /// is listening; `on_exit` gets the exit code after all output was delivered.
    pub fn spawn(
        &self,
        req: SpawnRequest,
        mut on_data: impl FnMut(&[u8]) -> bool + Send + 'static,
        on_exit: impl FnOnce(i32) + Send + 'static,
    ) -> Result<u32, String> {
        let dir = resolve_cwd(req.cwd)?;
        let shell = &self.shell;

        let pair = native_pty_system()
            .openpty(PtySize { rows: req.rows, cols: req.cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| format!("could not open a PTY: {e:#}"))?;
        // Take both ends before spawning so a failure here cannot orphan a child.
        let mut reader = pair.master.try_clone_reader().map_err(|e| format!("PTY reader: {e:#}"))?;
        let mut writer = pair.master.take_writer().map_err(|e| format!("PTY writer: {e:#}"))?;

        let mut cmd = CommandBuilder::new(shell);
        match req.command.filter(|c| !c.trim().is_empty()) {
            Some(c) => cmd.args(["-lc", &c]),
            None => cmd.arg("-l"),
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
        let exited = Arc::new(AtomicBool::new(false));
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
                    eprintln!("raterm: session {id} write failed: {e}");
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
                            eprintln!("raterm: session {id} lost its frontend, killing it");
                            detached = true;
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => {
                        // EIO is how Linux reports the slave side closing.
                        if e.raw_os_error() != Some(libc::EIO) {
                            eprintln!("raterm: session {id} read failed: {e}");
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
                    eprintln!("raterm: session {id} wait failed: {e}");
                    -1
                }
            };
            exited.store(true, Ordering::Release);
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

    /// Hangs up every session; called when the app exits.
    pub fn kill_all(&self) {
        let all: Vec<_> = lock(&self.map).drain().map(|(_, s)| s).collect();
        for s in all {
            s.kill(self.kill_grace);
        }
    }

    /// Clones the session out so the map lock is never held during PTY work.
    fn get(&self, id: u32) -> Result<Arc<Session>, String> {
        lock(&self.map).get(&id).cloned().ok_or_else(|| format!("no session {id}"))
    }
}

/// Locks a mutex, recovering the data if a previous holder panicked.
fn lock<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Resolves the requested working directory, expanding a leading `~`.
fn resolve_cwd(cwd: Option<String>) -> Result<Option<String>, String> {
    let home = std::env::var("HOME").ok();
    let Some(dir) = cwd.filter(|d| !d.trim().is_empty()) else { return Ok(home) };
    let dir = match (dir.strip_prefix('~'), &home) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with('/') => format!("{h}{rest}"),
        _ => dir,
    };
    if !Path::new(&dir).is_dir() {
        return Err(format!("not a directory: {dir}"));
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

    fn sessions() -> Sessions {
        Sessions::new("/bin/sh", Duration::from_millis(300))
    }

    fn spawn(s: &Sessions, command: &str) -> Probe {
        spawn_in(s, command, None)
    }

    fn spawn_in(s: &Sessions, command: &str, cwd: Option<&str>) -> Probe {
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = output.clone();
        let (tx, exit) = mpsc::channel();
        let req = SpawnRequest {
            cols: 80,
            rows: 24,
            cwd: cwd.map(Into::into),
            command: Some(command.into()),
        };
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
        let p = spawn(&s, "echo hello-raterm");
        assert_eq!(p.exit_code(), 0);
        assert!(p.text().contains("hello-raterm"), "output: {:?}", p.text());
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
    fn runs_in_requested_directory() {
        let s = sessions();
        let p = spawn_in(&s, "pwd", Some("/tmp"));
        p.exit_code();
        assert!(p.text().contains("/tmp"), "output: {:?}", p.text());
    }

    #[test]
    fn rejects_missing_directory() {
        let s = sessions();
        let req = SpawnRequest {
            cols: 80,
            rows: 24,
            cwd: Some("/definitely/not/here".into()),
            command: None,
        };
        let err = s.spawn(req, |_| true, |_| {}).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn expands_tilde_in_cwd() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(resolve_cwd(Some("~".into())).unwrap(), Some(home.clone()));
        assert_eq!(resolve_cwd(None).unwrap(), Some(home.clone()));
        assert_eq!(resolve_cwd(Some("  ".into())).unwrap(), Some(home));
        assert!(resolve_cwd(Some("~nobody-xyz".into())).is_err());
    }
}
