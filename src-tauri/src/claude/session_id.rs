//! Claude Code session ids, and finding the transcript Claude saved for one.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// A Claude Code session id. Only canonical lowercase UUIDs are accepted, since
/// the id ends up in a command line and a file path; holding one proves that.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(id: &str) -> Result<Self, String> {
        // Only the canonical form: the id must print back exactly as given.
        match uuid::Uuid::try_parse(id) {
            Ok(uuid) if uuid.hyphenated().to_string() == id => Ok(Self(id.to_owned())),
            _ => Err(format!("invalid Claude session id {id:?}")),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SessionId {
    type Error = String;
    fn try_from(id: String) -> Result<Self, String> {
        Self::parse(&id)
    }
}

impl From<SessionId> for String {
    fn from(id: SessionId) -> String {
        id.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Claude's projects directory: `$CLAUDE_CONFIG_DIR/projects`, defaulting to
/// `~/.claude/projects`.
pub fn projects_dir() -> Option<PathBuf> {
    projects_dir_from(std::env::var_os("CLAUDE_CONFIG_DIR"), crate::session::home_dir().map(OsString::from))
}

fn projects_dir_from(config_dir: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match config_dir {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home?).join(".claude"),
    };
    Some(base.join("projects"))
}

/// Whether `path` is a file. Only a path that is not there (or runs through a
/// non-directory) counts as absent; any other error, such as a permission
/// problem, is reported rather than read as "no file".
fn file_exists(path: &Path) -> Result<bool, String> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.is_file()),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Ok(false),
        Err(e) => Err(format!("cannot check {}: {e}", path.display())),
    }
}

/// Whether Claude Code saved a transcript for `id` in any project. A missing
/// projects directory means none; any other read error is reported, not guessed.
fn transcript_in_projects(projects: &Path, id: &SessionId) -> Result<bool, String> {
    let cannot_read = |e: std::io::Error| format!("cannot read {}: {e}", projects.display());
    let entries = match std::fs::read_dir(projects) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(cannot_read(e)),
    };
    let file = format!("{id}.jsonl");
    for entry in entries {
        if file_exists(&entry.map_err(cannot_read)?.path().join(&file))? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `session` has a conversation to resume. The transcript path the hook
/// reported is checked first; Claude's projects directory is scanned when no
/// reported transcript exists on disk. An error means the answer is unknown, and
/// guessing would either lose the conversation or fail to start.
pub fn should_resume(session: &SessionId, reported: Option<&Path>, projects: Option<&Path>) -> Result<bool, String> {
    if let Some(reported) = reported
        && file_exists(reported)?
    {
        return Ok(true);
    }
    match projects {
        Some(projects) => transcript_in_projects(projects, session),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::test_util::{id, ID};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn validates_session_ids() {
        assert!(SessionId::parse(ID).is_ok());
        for bad in [
            "0F8FAD5B-D9CB-469F-A165-70867728950E",
            "../../etc/passwd-aaaaaaaaaaaaaaaaaaaaaa",
            "0f8fad5b; rm -rf ~; 469f-a165-7086772895",
            "0f8fad5b-d9cb-469f-a165-70867728950g",
            "0f8fad5bd-9cb-469f-a165-70867728950e",
            "",
        ] {
            assert!(SessionId::parse(bad).is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<SessionId>(r#""nope""#).is_err());
        assert_eq!(serde_json::to_string(&id()).unwrap(), format!("\"{ID}\""));
    }

    #[test]
    fn decides_to_resume_from_the_reported_transcript_or_the_projects_dir() {
        let base = crate::test_support::temp_dir("resume");
        let project = base.join("projects").join("-home-me-src");
        fs::create_dir_all(&project).unwrap();
        let reported = base.join("elsewhere.jsonl");
        fs::write(&reported, "{}").unwrap();

        assert!(should_resume(&id(), Some(&reported), None).unwrap(), "reported transcript exists");
        assert!(!should_resume(&id(), Some(&base.join("gone.jsonl")), Some(&base.join("projects"))).unwrap());
        fs::write(project.join(format!("{ID}.jsonl")), "{}").unwrap();
        assert!(should_resume(&id(), None, Some(&base.join("projects"))).unwrap(), "found by scanning");
        assert!(!should_resume(&id(), None, Some(&base.join("missing"))).unwrap());
        assert!(!should_resume(&id(), None, None).unwrap());

        // An unreadable projects directory is an error, not "no conversation".
        let not_a_dir = base.join("file");
        fs::write(&not_a_dir, "").unwrap();
        assert!(should_resume(&id(), None, Some(&not_a_dir)).is_err());
        // A stray file among the projects is not a project.
        fs::write(base.join("projects").join("notes.txt"), "").unwrap();
        fs::remove_file(project.join(format!("{ID}.jsonl"))).unwrap();
        assert!(!should_resume(&id(), None, Some(&base.join("projects"))).unwrap());
        fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_transcript_that_cannot_be_checked_is_an_error_not_absent() {
        if crate::test_support::running_as_root() {
            return;
        }
        let base = crate::test_support::temp_dir("resume-locked");
        let project = base.join("projects").join("-home-me-src");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(format!("{ID}.jsonl")), "{}").unwrap();
        // Listable but not searchable: the transcript's metadata cannot be read.
        fs::set_permissions(&project, fs::Permissions::from_mode(0o644)).unwrap();
        let scanned = should_resume(&id(), None, Some(&base.join("projects")));
        let reported = should_resume(&id(), Some(&project.join(format!("{ID}.jsonl"))), None);
        fs::set_permissions(&project, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(scanned.unwrap_err().contains("cannot check"));
        assert!(reported.unwrap_err().contains("cannot check"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn finds_the_projects_dir_from_the_config_dir_or_home() {
        let config = projects_dir_from(Some("/cfg".into()), Some("/home/me".into()));
        assert_eq!(config, Some(PathBuf::from("/cfg/projects")));
        assert_eq!(projects_dir_from(None, Some("/home/me".into())), Some(PathBuf::from("/home/me/.claude/projects")));
        assert_eq!(projects_dir_from(None, None), None);
    }
}
