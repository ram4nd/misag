//! What the app does when it panics. The release profile aborts on a panic (see
//! Cargo.toml), and every command runs inside webkit's C callback, where even an
//! unwinding panic would abort, so nothing gets to clean up: the panic hook is the
//! last code that runs. It kills the tabs' processes, which a hang-up alone would
//! orphan if they ignore SIGHUP, then records the panic in `PANIC_LOG` next to the
//! database, since an abort leaves no other trace.

use crate::session::SessionKiller;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The panic log, in the app's data directory next to `misag.db`.
pub const PANIC_LOG: &str = "panic.log";
/// A panic log larger than this is started over at the next panic.
const PANIC_LOG_LIMIT: u64 = 256 * 1024;

/// The data directory, known only once Tauri has set up and only if it could be
/// resolved; until then a panic goes to stderr alone.
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Sets where `install`'s hook writes the panic log.
pub fn set_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// Installs the app's panic hook. When the panic ends the app, it first kills the
/// tabs' processes through `sessions`; then it logs the panic with a backtrace and
/// what was killed, and runs the hook it replaces (by default the one printing the
/// panic to stderr).
pub fn install(sessions: SessionKiller) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Killing comes first: symbolizing the backtrace and writing the log are
        // slower and more likely to fail.
        let report = if ends_process(std::thread::current().name()) {
            sessions.kill_now().to_string()
        } else {
            "the app keeps running, so the tabs' processes were left alone".to_owned()
        };
        let backtrace = std::backtrace::Backtrace::force_capture();
        match DIR.get() {
            Some(dir) => write(dir, &format!("{info}\n{report}\n{backtrace}")),
            None => {
                use std::io::Write;
                let _ = writeln!(std::io::stderr(), "misag: panicked, {report}");
            }
        }
        previous(info);
    }));
}

/// Whether a panic on the thread named `thread` ends the app: always under
/// `panic = "abort"` (the release profile), and on the main thread whatever the
/// profile, since commands run there inside webkit's C callback, which aborts even
/// an unwinding panic. A background thread's panic in a debug build unwinds and
/// the app keeps running.
fn ends_process(thread: Option<&str>) -> bool {
    cfg!(panic = "abort") || thread == Some("main")
}

/// Appends `message` to `PANIC_LOG` in `dir` with the time, creating `dir` and
/// starting the log over once it grows past `PANIC_LOG_LIMIT`. Notes on stderr
/// where the log is, or why it could not be written.
pub(crate) fn write(dir: &Path, message: &str) {
    use std::io::Write;
    let path = dir.join(PANIC_LOG);
    let file = std::fs::create_dir_all(dir).and_then(|()| crate::open_capped_log(&path, PANIC_LOG_LIMIT));
    let at = crate::db::unix_secs(std::time::SystemTime::now());
    let written = file.and_then(|mut file| writeln!(file, "{at} misag panicked: {}", message.trim_end()));
    let _ = match written {
        Ok(()) => writeln!(std::io::stderr(), "misag: panicked, see {}", path.display()),
        Err(e) => writeln!(std::io::stderr(), "misag: could not write {}: {e}", path.display()),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panics_are_appended_to_the_log() {
        let dir = crate::test_support::temp_dir("panic-log");
        write(&dir, "panicked at x.rs:1:1:\nboom\n");
        write(&dir, "panicked at y.rs:2:2:\nagain");
        let log = std::fs::read_to_string(dir.join(PANIC_LOG)).unwrap();
        let entries: Vec<_> = log.lines().filter_map(|l| l.split_once(' ')).map(|(_, rest)| rest).collect();
        assert_eq!(entries, ["misag panicked: panicked at x.rs:1:1:", "misag panicked: panicked at y.rs:2:2:"], "{log}");
        assert!(log.contains("\nboom\n") && log.ends_with("\nagain\n"), "{log}");
    }

    #[test]
    fn the_log_directory_is_created() {
        let dir = crate::test_support::temp_dir("panic-log-dir").join("missing");
        write(&dir, "boom");
        let log = std::fs::read_to_string(dir.join(PANIC_LOG)).unwrap();
        assert!(log.ends_with(" misag panicked: boom\n"), "{log}");
    }

    #[test]
    fn only_a_panic_that_ends_the_app_kills_the_tabs() {
        assert!(ends_process(Some("main")));
        // Tests unwind, like a debug build: a background thread's panic is recoverable.
        assert_eq!(ends_process(Some("pty-reader")), cfg!(panic = "abort"));
        assert_eq!(ends_process(None), cfg!(panic = "abort"));
    }

    #[test]
    fn a_full_log_is_started_over() {
        let dir = crate::test_support::temp_dir("panic-log-full");
        std::fs::write(dir.join(PANIC_LOG), vec![b'x'; PANIC_LOG_LIMIT as usize + 1]).unwrap();
        write(&dir, "fresh");
        let log = std::fs::read_to_string(dir.join(PANIC_LOG)).unwrap();
        assert!(log.ends_with(" misag panicked: fresh\n") && !log.contains('x'), "{log}");
    }
}
