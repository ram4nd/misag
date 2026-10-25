//! Drives a real interactive Claude Code in misag sessions: `/clear` moves the
//! tab to a new session, and a restored tab resumes its conversation. Needs
//! `claude` on PATH and a folder Claude already trusts (otherwise its trust prompt
//! blocks the hook), so it is skipped by default:
//!
//!     MISAG_LIVE_DIR=~/some/trusted/project cargo test --test claude_live -- --ignored
//!
//! The `/clear` test sends no prompt of its own. The resume test sends one short
//! prompt (`claude -p`) to create a conversation worth resuming.

mod common;

use misag_lib::claude::{self, SessionId};
use misag_lib::db;
use misag_lib::session::{Launch, Sessions, SpawnRequest};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A Claude session run as a misag tab with tab id `live`, reporting to its own database.
struct LiveTab {
    sessions: Sessions,
    id: u32,
    data: PathBuf,
    session: SessionId,
}

impl LiveTab {
    fn start(name: &str, session: SessionId, resume: bool) -> Self {
        let dir = std::env::var("MISAG_LIVE_DIR").expect("set MISAG_LIVE_DIR to a folder Claude trusts");
        let data = common::temp_dir(name);
        drop(db::Db::open(data.join(db::FILE)).unwrap());
        let args = claude::args(&session, resume, Some(Path::new(env!("CARGO_BIN_EXE_misag"))), Default::default());
        let sessions = Sessions::default();
        let req = SpawnRequest {
            cols: 120,
            rows: 40,
            cwd: Some(dir),
            launch: Launch::Program { program: "claude".into(), args },
            env: vec![
                (claude::TAB_ENV.into(), "live".into()),
                (claude::DB_ENV.into(), data.join(db::FILE).to_string_lossy().into_owned()),
            ],
        };
        let id = sessions.spawn(req, |_| true, |_| {}).unwrap();
        Self { sessions, id, data, session }
    }

    /// Waits for the hook to report a session started for `source`; returns its id.
    fn wait_for(&self, source: &str) -> String {
        let start = Instant::now();
        loop {
            let row: Option<(String, String)> = db::connect_existing(&self.data.join(db::FILE)).ok().and_then(|c| {
                c.query_row("SELECT session_id, source FROM claude_sessions WHERE tab_id = 'live'", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .ok()
            });
            if let Some((id, _)) = row.filter(|r| r.1 == source) {
                return id;
            }
            assert!(start.elapsed() < Duration::from_secs(30), "timed out waiting for {source}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for LiveTab {
    /// Stops Claude and removes this test's data and the starting session's transcript.
    fn drop(&mut self) {
        self.sessions.kill_all();
        let _ = std::fs::remove_dir_all(&self.data);
        if let Some(projects) = claude::projects_dir() {
            for project in std::fs::read_dir(projects).into_iter().flatten().flatten() {
                let _ = std::fs::remove_file(project.path().join(format!("{}.jsonl", self.session)));
            }
        }
    }
}

fn new_session() -> SessionId {
    SessionId::parse(&uuid::Uuid::new_v4().to_string()).unwrap()
}

#[test]
#[ignore = "needs claude and a trusted folder; set MISAG_LIVE_DIR"]
fn clear_moves_the_tab_to_a_new_session() {
    let tab = LiveTab::start("live-clear", new_session(), false);
    assert_eq!(tab.wait_for("startup"), tab.session.as_str());

    tab.sessions.write(tab.id, b"/clear".to_vec()).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    tab.sessions.write(tab.id, b"\r".to_vec()).unwrap();
    assert_ne!(tab.wait_for("clear"), tab.session.as_str());
}

#[test]
#[ignore = "needs claude and a trusted folder; set MISAG_LIVE_DIR; sends one short prompt"]
fn a_restored_tab_resumes_its_conversation() {
    let session = new_session();
    // A first run leaves a conversation behind, as a tab would before misag quits.
    let dir = std::env::var("MISAG_LIVE_DIR").expect("set MISAG_LIVE_DIR to a folder Claude trusts");
    let first = std::process::Command::new("claude")
        .args(["-p", "--session-id", session.as_str(), "Reply with just: ok"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    assert!(claude::should_resume(&session, None, claude::projects_dir().as_deref()).unwrap(), "the transcript is found");

    let tab = LiveTab::start("live-resume", session, true);
    assert_eq!(tab.wait_for("resume"), tab.session.as_str());
}
