//! Past Claude Code conversations for the Session history box, read from the
//! transcripts Claude keeps in its projects directory. Mirrored by `Past` in src/history.ts.

use crate::claude::SessionId;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Titles are cut to this many characters.
const MAX_TITLE: usize = 200;
/// Claude Code cuts longer project folder names and appends a hash.
const MAX_PROJECT_NAME: usize = 200;
/// Longer transcript lines (tool output, pasted files) are skipped unread: the
/// lines a title comes from are far shorter.
const MAX_LINE: usize = 1 << 20;

/// A conversation Claude saved, as the box lists it.
#[derive(Serialize, Debug, PartialEq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Past {
    pub id: SessionId,
    pub title: String,
    /// The folder it was listed for, which is where Claude ran (a transcript that
    /// names another folder is left out), and where `claude --resume` finds it.
    pub cwd: String,
    /// When the transcript last changed, in unix milliseconds.
    pub modified: u64,
}

/// The newest conversations of a folder, as many as asked for.
#[derive(Serialize, Debug, PartialEq)]
pub struct Page {
    pub sessions: Vec<Past>,
    /// There are older ones.
    pub more: bool,
    /// Transcripts left out because they could not be read.
    pub unreadable: usize,
}

/// What a transcript says about its conversation.
#[derive(Debug, PartialEq, Clone, Default)]
struct Summary {
    cwd: Option<String>,
    /// The name it was given with /rename.
    name: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    first_prompt: Option<String>,
}

impl Summary {
    fn title(&self) -> Option<&str> {
        [&self.name, &self.ai_title, &self.last_prompt, &self.first_prompt].into_iter().flatten().map(String::as_str).next()
    }
}

/// How far a transcript was read, and what it said up to there.
#[derive(Clone)]
struct Scan {
    modified: SystemTime,
    size: u64,
    /// Where the first line not yet read in full starts.
    offset: u64,
    summary: Summary,
}

#[derive(Clone)]
enum Entry {
    Read(Scan),
    /// Reading failed; it is tried again (and logged again) only once the file changes.
    Failed { modified: SystemTime, size: u64 },
}

/// Transcripts as last read, so a refresh reads only what changed: nothing for an
/// unchanged one, and only the new lines of one Claude has appended to.
#[derive(Default)]
pub struct Cache(Mutex<HashMap<PathBuf, Entry>>);

/// The name of Claude's project folder for `cwd`: every character other than an
/// ASCII letter or digit becomes `-`.
fn project_name(cwd: &str) -> String {
    // Per UTF-16 unit, as Claude Code (JavaScript) does: a character outside the
    // Basic Multilingual Plane becomes two dashes.
    cwd.encode_utf16()
        .map(|u| match char::from_u32(u32::from(u)) {
            Some(c) if c.is_ascii_alphanumeric() => c,
            _ => '-',
        })
        .collect()
}

/// The project folders that can hold `cwd`'s conversations. A name too long for
/// Claude is cut and given a hash, so any folder with the cut name counts.
fn project_dirs(projects: &Path, cwd: &str) -> Result<Vec<PathBuf>, String> {
    let name = project_name(cwd);
    if name.len() <= MAX_PROJECT_NAME {
        return Ok(vec![projects.join(name)]);
    }
    let prefix = format!("{}-", &name[..MAX_PROJECT_NAME]);
    let entries = match std::fs::read_dir(projects) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", projects.display())),
    };
    Ok(entries.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(&prefix)).map(|e| e.path()).collect())
}

/// One line of text: control characters and runs of whitespace become one space.
fn one_line(text: &str) -> Option<String> {
    let line = text.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then(|| line.chars().take(MAX_TITLE).collect())
}

/// The text the person typed in a `user` entry; None for Claude Code's own entries
/// (meta notes, slash commands, their output) and for tool results.
fn typed_prompt(entry: &Value) -> Option<String> {
    if entry["isMeta"].as_bool() == Some(true) || entry["isSidechain"].as_bool() == Some(true) {
        return None;
    }
    let content = &entry["message"]["content"];
    let text = match content {
        Value::String(text) => text.as_str(),
        Value::Array(blocks) => blocks.iter().find(|b| b["type"] == "text")?["text"].as_str()?,
        _ => return None,
    };
    if text.trim_start().starts_with('<') {
        return None;
    }
    one_line(text)
}

/// Skips the rest of a line; returns the bytes skipped and whether its end was found.
fn skip_line(reader: &mut impl BufRead) -> std::io::Result<(u64, bool)> {
    let mut skipped = 0;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok((skipped, false));
        }
        let (n, found) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        reader.consume(n);
        skipped += n as u64;
        if found {
            return Ok((skipped, true));
        }
    }
}

/// Folds one transcript line into `summary`. Only lines that can name the
/// conversation are parsed, picked by a look at their text; `user` entries only
/// until the folder and the first typed prompt are known.
fn fold_line(summary: &mut Summary, text: &str) {
    let wanted = ["\"custom-title\"", "\"agent-name\"", "\"ai-title\"", "\"last-prompt\""].iter().any(|t| text.contains(t))
        || (text.contains("\"user\"") && (summary.first_prompt.is_none() || summary.cwd.is_none()));
    if !wanted {
        return;
    }
    // A line cut short by a crash, or one Claude writes in a newer shape, is left out.
    let Ok(entry) = serde_json::from_str::<Value>(text) else { return };
    let field = |key: &str| entry[key].as_str().and_then(one_line);
    match entry["type"].as_str() {
        Some("custom-title") => summary.name = field("customTitle").or(summary.name.take()),
        Some("agent-name") => summary.name = field("agentName").or(summary.name.take()),
        Some("ai-title") => summary.ai_title = field("aiTitle").or(summary.ai_title.take()),
        Some("last-prompt") => summary.last_prompt = field("lastPrompt").or(summary.last_prompt.take()),
        Some("user") => {
            if summary.cwd.is_none() {
                summary.cwd = entry["cwd"].as_str().map(str::to_owned);
            }
            if summary.first_prompt.is_none() {
                summary.first_prompt = typed_prompt(&entry);
            }
        }
        _ => {}
    }
}

/// Reads a transcript from `offset` on, adding to `summary`. Returns where the
/// first line not read in full starts: a last line still being written is read,
/// and read again next time. Lines over `MAX_LINE` are skipped.
fn scan(path: &Path, mut offset: u64, mut summary: Summary) -> Result<(u64, Summary), String> {
    let cannot_read = |e: std::io::Error| format!("cannot read {}: {e}", path.display());
    let mut file = std::fs::File::open(path).map_err(cannot_read)?;
    file.seek(SeekFrom::Start(offset)).map_err(cannot_read)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line).map_err(cannot_read)?;
        if n == 0 {
            break;
        }
        let complete = line.last() == Some(&b'\n');
        if !complete && n > MAX_LINE {
            let (skipped, found) = skip_line(&mut reader).map_err(cannot_read)?;
            if !found {
                break;
            }
            offset += n as u64 + skipped;
            continue;
        }
        fold_line(&mut summary, &String::from_utf8_lossy(&line));
        if !complete {
            break;
        }
        offset += n as u64;
    }
    Ok((offset, summary))
}

/// Why a transcript was left out: `new` is false when the failure was already logged.
struct Unreadable {
    new: bool,
    message: String,
}

impl Cache {
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Entry>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The summary of the transcript at `path`. An unchanged one is not read; one
    /// that grew is read from where the last read stopped. The file is read with
    /// the cache unlocked.
    fn summary(&self, path: &Path, modified: SystemTime, size: u64) -> Result<Summary, Unreadable> {
        let known = self.entries().get(path).cloned();
        let (offset, summary) = match known {
            Some(Entry::Read(scan)) if scan.modified == modified && scan.size == size => return Ok(scan.summary),
            Some(Entry::Failed { modified: m, size: s }) if m == modified && s == size => {
                return Err(Unreadable { new: false, message: String::new() });
            }
            // Claude only appends; a file that did not grow was replaced.
            Some(Entry::Read(scan)) if size > scan.size => (scan.offset, scan.summary),
            _ => (0, Summary::default()),
        };
        let read = scan(path, offset, summary);
        let entry = match &read {
            Ok((offset, summary)) => Entry::Read(Scan { modified, size, offset: *offset, summary: summary.clone() }),
            Err(_) => Entry::Failed { modified, size },
        };
        self.entries().insert(path.to_owned(), entry);
        read.map(|(_, summary)| summary).map_err(|message| Unreadable { new: true, message })
    }

    /// The newest `limit` conversations Claude saved in `cwd`, newest first.
    /// Ones with no name, title or typed prompt, and ones of another folder whose
    /// name maps to the same project folder, are left out. A missing projects
    /// folder lists nothing.
    pub fn list(&self, projects: &Path, cwd: &str, limit: usize) -> Result<Page, String> {
        let cwd = cwd.trim_end_matches('/');
        let cwd = if cwd.is_empty() { "/" } else { cwd };
        let dirs = project_dirs(projects, cwd)?;
        let mut files = Vec::new();
        let mut unreadable = 0;
        for dir in &dirs {
            let entries = match std::fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        eprintln!("raterm: cannot read {}: {e}", dir.display());
                        unreadable += 1;
                        continue;
                    }
                };
                let path = entry.path();
                let id = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".jsonl"));
                let Some(Ok(id)) = id.map(SessionId::parse) else { continue };
                match entry.metadata().and_then(|meta| Ok((meta.is_file(), meta.modified()?, meta.len()))) {
                    Ok((true, modified, size)) => files.push((modified, size, id, path)),
                    Ok((false, ..)) => {}
                    Err(e) => {
                        eprintln!("raterm: cannot read {}: {e}", path.display());
                        unreadable += 1;
                    }
                }
            }
        }
        // Transcripts that have gone leave the cache.
        let seen: HashSet<&Path> = files.iter().map(|f| f.3.as_path()).collect();
        self.entries().retain(|path, _| !path.parent().is_some_and(|p| dirs.iter().any(|d| d == p)) || seen.contains(path.as_path()));
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        let mut found = Vec::new();
        for (modified, size, id, path) in files {
            // One past the limit, to tell whether there are more.
            if found.len() > limit {
                break;
            }
            // One unreadable transcript should not hide the others.
            let summary = match self.summary(&path, modified, size) {
                Ok(summary) => summary,
                Err(e) => {
                    if e.new {
                        eprintln!("raterm: {}", e.message);
                    }
                    unreadable += 1;
                    continue;
                }
            };
            if summary.cwd.as_deref().is_some_and(|c| c.trim_end_matches('/') != cwd) {
                continue;
            }
            let Some(title) = summary.title() else { continue };
            let ms = modified.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
            found.push(Past { id, title: title.to_owned(), cwd: cwd.to_owned(), modified: ms });
        }
        let more = found.len() > limit;
        found.truncate(limit);
        Ok(Page { sessions: found, more, unreadable })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    const A: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const B: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

    fn user(cwd: &str, content: &str) -> String {
        format!(r#"{{"type":"user","cwd":"{cwd}","message":{{"role":"user","content":{content}}}}}"#)
    }

    /// Writes a transcript and sets its modification time to `age` seconds ago.
    fn transcript(projects: &Path, cwd: &str, id: &str, lines: &[String], age: u64) {
        let dir = projects.join(project_name(cwd));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(age)).unwrap();
    }

    fn titles(list: &[Past]) -> Vec<&str> {
        list.iter().map(|p| p.title.as_str()).collect()
    }

    #[test]
    fn names_project_folders_like_claude() {
        assert_eq!(project_name("/home/me/my.app_x"), "-home-me-my-app-x");
        // Claude replaces UTF-16 units, so a character outside the BMP becomes two dashes.
        assert_eq!(project_name("/a\u{1F600}b/é"), "-a--b--");
    }

    fn cached(cache: &Cache) -> usize {
        cache.0.lock().unwrap().len()
    }

    /// Sets a file's modification time, so a rewrite can keep the one it had.
    fn set_mtime(path: &Path, time: SystemTime) {
        fs::File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
    }

    fn path_of(projects: &Path, cwd: &str, id: &str) -> PathBuf {
        projects.join(project_name(cwd)).join(format!("{id}.jsonl"))
    }

    #[test]
    fn an_unchanged_transcript_is_not_read_again() {
        let projects = crate::test_support::temp_dir("history-hit").join("projects");
        let cache = Cache::default();
        transcript(&projects, "/w", A, &[user("/w", r#""one""#)], 50);
        let path = path_of(&projects, "/w", A);
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["one"]);
        // Same size and time: the cached title stays.
        fs::write(&path, user("/w", r#""two""#) + "\n").unwrap();
        set_mtime(&path, mtime);
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["one"]);
        // Only the time changed: read again.
        set_mtime(&path, mtime + Duration::from_secs(1));
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["two"]);
    }

    #[test]
    fn a_grown_transcript_is_read_from_where_the_last_read_stopped() {
        let projects = crate::test_support::temp_dir("history-tail").join("projects");
        let cache = Cache::default();
        transcript(&projects, "/w", A, &[user("/w", r#""first""#)], 50);
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["first"]);
        // The start is not read again: a changed first line goes unnoticed.
        let path = path_of(&projects, "/w", A);
        let lines = [user("/w", r#""other""#), r#"{"type":"assistant","message":{}}"#.into()];
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["first"]);
        // A line still being written is read again once it is complete.
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(r#"{"type":"ai-title","aiTi"#);
        fs::write(&path, &text).unwrap();
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["first"]);
        text.push_str("tle\":\"Done\"}\n");
        fs::write(&path, &text).unwrap();
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["Done"]);
        // A shorter file was replaced, so it is read from the start.
        transcript(&projects, "/w", A, &[user("/w", r#""new""#)], 0);
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["new"]);
    }

    #[test]
    fn the_last_name_and_title_win() {
        let projects = crate::test_support::temp_dir("history-last").join("projects");
        let lines = [
            user("/w", r#""hi""#),
            r#"{"type":"custom-title","customTitle":"old"}"#.into(),
            r#"{"type":"custom-title","customTitle":"new"}"#.into(),
        ];
        transcript(&projects, "/w", A, &lines, 0);
        assert_eq!(titles(&Cache::default().list(&projects, "/w", 100).unwrap().sessions), ["new"]);
    }

    #[test]
    fn oversized_lines_are_skipped() {
        let projects = crate::test_support::temp_dir("history-big").join("projects");
        let huge = format!(r#"{{"type":"ai-title","aiTitle":"{}"}}"#, "x".repeat(MAX_LINE));
        let lines = [user("/w", r#""prompt""#), huge, r#"{"type":"last-prompt","lastPrompt":"after"}"#.into()];
        transcript(&projects, "/w", A, &lines, 0);
        assert_eq!(titles(&Cache::default().list(&projects, "/w", 100).unwrap().sessions), ["after"]);
    }

    #[test]
    fn unreadable_transcripts_are_counted_and_the_others_listed() {
        if crate::test_support::running_as_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let projects = crate::test_support::temp_dir("history-unreadable").join("projects");
        transcript(&projects, "/w", A, &[user("/w", r#""fine""#)], 10);
        transcript(&projects, "/w", B, &[user("/w", r#""locked""#)], 0);
        fs::set_permissions(path_of(&projects, "/w", B), fs::Permissions::from_mode(0o000)).unwrap();
        let cache = Cache::default();
        for _ in 0..2 {
            let page = cache.list(&projects, "/w", 100).unwrap();
            assert_eq!(titles(&page.sessions), ["fine"]);
            assert_eq!(page.unreadable, 1);
        }
    }

    #[test]
    fn deleted_transcripts_leave_the_cache() {
        let projects = crate::test_support::temp_dir("history-evict").join("projects");
        transcript(&projects, "/w", A, &[user("/w", r#""a""#)], 10);
        transcript(&projects, "/w", B, &[user("/w", r#""b""#)], 0);
        let cache = Cache::default();
        cache.list(&projects, "/w", 100).unwrap();
        assert_eq!(cached(&cache), 2);
        fs::remove_file(path_of(&projects, "/w", B)).unwrap();
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["a"]);
        assert_eq!(cached(&cache), 1);
    }

    #[test]
    fn lists_a_folders_conversations_newest_first_with_their_best_title() {
        let projects = crate::test_support::temp_dir("history-list").join("projects");
        let first = user("/w", r#""fix the  build\nplease""#);
        transcript(&projects, "/w", A, std::slice::from_ref(&first), 100);
        let titled = [
            first.clone(),
            r#"{"type":"last-prompt","lastPrompt":"later prompt"}"#.into(),
            r#"{"type":"ai-title","aiTitle":"Build fix"}"#.into(),
        ];
        transcript(&projects, "/w", B, &titled, 10);
        let list = Cache::default().list(&projects, "/w/", 100).unwrap().sessions;
        assert_eq!(titles(&list), ["Build fix", "fix the build please"]);
        assert_eq!(list[0].id.as_str(), B);
        assert_eq!(list[0].cwd, "/w");
        assert!(list[0].modified > list[1].modified);
    }

    #[test]
    fn a_rename_wins_over_other_titles() {
        let projects = crate::test_support::temp_dir("history-rename").join("projects");
        let lines = [
            user("/w", r#""hi""#),
            r#"{"type":"ai-title","aiTitle":"Greeting"}"#.into(),
            r#"{"type":"agent-name","agentName":"my-session"}"#.into(),
        ];
        transcript(&projects, "/w", A, &lines, 0);
        assert_eq!(titles(&Cache::default().list(&projects, "/w", 100).unwrap().sessions), ["my-session"]);
    }

    #[test]
    fn leaves_out_commands_meta_notes_empty_sessions_and_other_folders() {
        let projects = crate::test_support::temp_dir("history-skip").join("projects");
        let lines = [
            r#"{"type":"user","isMeta":true,"cwd":"/w","message":{"content":"caveat"}}"#.into(),
            user("/w", r#""<command-name>/model</command-name>""#),
            user("/w", r#"[{"type":"tool_result","content":"x"}]"#),
            "{\"type\":\"user\", cut short".into(),
            user("/w", r#"[{"type":"text","text":"real prompt"}]"#),
        ];
        transcript(&projects, "/w", A, &lines, 0);
        // Only Claude Code's own entries: nothing to show.
        transcript(&projects, "/w", B, &[user("/w", r#""<command-name>/clear</command-name>""#)], 0);
        // `/w-x` and `/w/x` share a project folder; each lists only its own.
        transcript(&projects, "/w/x", "9b2e1f0c-1d2e-4f3a-8b4c-5d6e7f8a9b0c", &[user("/w-x", r#""other""#)], 0);
        fs::write(projects.join(project_name("/w")).join("notes.jsonl"), "").unwrap();
        assert_eq!(titles(&Cache::default().list(&projects, "/w", 100).unwrap().sessions), ["real prompt"]);
        assert!(Cache::default().list(&projects, "/w/x", 100).unwrap().sessions.is_empty());
        assert_eq!(titles(&Cache::default().list(&projects, "/w-x", 100).unwrap().sessions), ["other"]);
    }

    #[test]
    fn a_missing_projects_folder_lists_nothing() {
        let base = crate::test_support::temp_dir("history-missing");
        assert_eq!(Cache::default().list(&base.join("projects"), "/w", 100).unwrap().sessions, []);
    }

    #[test]
    fn long_folder_names_match_claudes_cut_and_hashed_folders() {
        let projects = crate::test_support::temp_dir("history-long").join("projects");
        let cwd = format!("/{}", "a".repeat(250));
        let dir = projects.join(format!("{}-1abc", &project_name(&cwd)[..MAX_PROJECT_NAME]));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{A}.jsonl")), user(&cwd, r#""long""#)).unwrap();
        assert_eq!(titles(&Cache::default().list(&projects, &cwd, 100).unwrap().sessions), ["long"]);
    }

    #[test]
    fn a_changed_transcript_is_read_again() {
        let projects = crate::test_support::temp_dir("history-cache").join("projects");
        let cache = Cache::default();
        transcript(&projects, "/w", A, &[user("/w", r#""one""#)], 50);
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["one"]);
        let lines = [user("/w", r#""one""#), r#"{"type":"ai-title","aiTitle":"Two"}"#.into()];
        transcript(&projects, "/w", A, &lines, 0);
        assert_eq!(titles(&cache.list(&projects, "/w", 100).unwrap().sessions), ["Two"]);
    }

    #[test]
    fn lists_a_page_of_the_newest_and_says_whether_there_are_more() {
        let projects = crate::test_support::temp_dir("history-page").join("projects");
        for i in 0..12u64 {
            let id = format!("00000000-0000-4000-8000-{i:012}");
            transcript(&projects, "/w", &id, &[user("/w", &format!(r#""p{i}""#))], 1000 - i);
        }
        // The oldest has nothing typed in it, so it does not count as more.
        transcript(&projects, "/w", A, &[user("/w", r#""<command-name>/clear</command-name>""#)], 5000);
        let cache = Cache::default();
        let page = cache.list(&projects, "/w", 10).unwrap();
        assert_eq!(titles(&page.sessions), (2..12).rev().map(|i| format!("p{i}")).collect::<Vec<_>>());
        assert!(page.more);
        let page = cache.list(&projects, "/w", 12).unwrap();
        assert_eq!(page.sessions.len(), 12);
        assert!(!page.more);
    }
}
