//! The open tabs, as saved between runs (stored by `db`). Mirrored by `Layout`
//! and `LayoutTab` in src/ipcTypes.ts; `src-tauri/tests/fixtures/layout.json` is a
//! shared example both sides parse.

use crate::claude::SessionId;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A tab's id. It is passed to Claude's hook in an environment variable and written
/// to the hook log as one word, so it must be non-empty printable text without
/// whitespace; holding one proves that.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TabId(String);

impl TabId {
    pub fn parse(id: &str) -> Result<Self, String> {
        if id.is_empty() || id.len() > 128 || id.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(format!("invalid tab id {id:?}"));
        }
        Ok(Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TabId {
    type Error = String;
    fn try_from(id: String) -> Result<Self, String> {
        Self::parse(&id)
    }
}

impl From<TabId> for String {
    fn from(id: TabId) -> String {
        id.0
    }
}

impl fmt::Display for TabId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Layout {
    pub tabs: Vec<Tab>,
    pub active: usize,
}

/// What a tab runs; each kind carries only its own fields.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum TabKind {
    /// A login shell, or `command` through it. `claude_session` is the Claude
    /// session open in the shell when the tab was saved, resumed in it on restore.
    Terminal {
        command: Option<String>,
        #[serde(default)]
        claude_session: Option<SessionId>,
    },
    /// Claude Code, pinned to `claude_session` (None until one is assigned).
    Claude { claude_session: Option<SessionId> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Tab {
    /// Stable across restarts; Claude's session hook reports under this id.
    pub tab_id: TabId,
    #[serde(flatten)]
    pub kind: TabKind,
    pub name: String,
    /// True when the user named the tab, so terminal titles must not replace it.
    pub renamed: bool,
    pub cwd: Option<String>,
}

impl Layout {
    /// Checks a layout before it is stored, so a bad one cannot break every later
    /// restore: tab ids must be unique (`TabId` makes each one valid). `active` is
    /// clamped to a tab.
    pub fn validated(mut self) -> Result<Self, String> {
        let mut seen = std::collections::HashSet::new();
        for tab in &self.tabs {
            if !seen.insert(&tab.tab_id) {
                return Err(format!("duplicate tab id {:?}", tab.tab_id.as_str()));
            }
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

    fn tab(id: &str, kind: TabKind) -> Tab {
        Tab { tab_id: TabId::parse(id).unwrap(), kind, name: id.into(), renamed: false, cwd: None }
    }

    #[test]
    fn validation_rejects_duplicate_tab_ids() {
        let terminal = TabKind::Terminal { command: None, claude_session: None };
        let dupes = Layout { active: 0, tabs: vec![tab("a", terminal.clone()), tab("a", terminal)] };
        assert!(dupes.validated().is_err());
    }

    #[test]
    fn tab_ids_are_plain_words() {
        assert!(TabId::parse("0f8fad5b-d9cb-469f-a165-70867728950e").is_ok());
        for bad in ["", "a b", "a\nb", "a\tb", "\u{7f}", &"x".repeat(129)] {
            assert!(TabId::parse(bad).is_err(), "{bad:?}");
        }
        let json = r#"{"tabId":"a\nb","kind":"terminal","command":null,"name":"x","renamed":false,"cwd":null}"#;
        assert!(serde_json::from_str::<Tab>(json).is_err(), "a saved layout cannot hold one either");
    }

    #[test]
    fn validation_clamps_the_active_tab() {
        let layout = Layout { active: 7, tabs: vec![tab("a", TabKind::Terminal { command: None, claude_session: None })] };
        assert_eq!(layout.validated().unwrap().active, 0);
        assert_eq!(Layout::default().validated().unwrap().active, 0);
    }

    #[test]
    fn parses_the_shared_fixture_the_frontend_also_uses() {
        let layout: Layout = serde_json::from_str(include_str!("../tests/fixtures/layout.json")).unwrap();
        assert_eq!(layout.active, 1);
        let Tab { tab_id, kind, name, renamed, .. } = &layout.tabs[0];
        assert_eq!((tab_id.as_str(), name.as_str(), *renamed), ("t1", "agent", true));
        assert_eq!(kind, &TabKind::Claude { claude_session: Some(SessionId::parse(SESSION).unwrap()) });
        assert_eq!(layout.tabs[1].kind, TabKind::Terminal { command: Some("tail -f syslog".into()), claude_session: None });
        let session = Some(SessionId::parse(SESSION).unwrap());
        assert_eq!(layout.tabs[2].kind, TabKind::Terminal { command: None, claude_session: session });
        let back = serde_json::to_value(&layout).unwrap();
        assert_eq!(back["tabs"][0]["claudeSession"], SESSION);
        assert_eq!(back["tabs"][0]["kind"], "claude");
        assert_eq!(back["tabs"][2]["claudeSession"], SESSION);
        // Saved by a misag from before terminal tabs could hold a session.
        let older = r#"{"tabId":"a","kind":"terminal","command":null,"name":"x","renamed":false,"cwd":null}"#;
        assert_eq!(serde_json::from_str::<Tab>(older).unwrap().kind, TabKind::Terminal { command: None, claude_session: None });
    }

    #[test]
    fn rejects_tabs_without_an_id_or_with_a_bad_session() {
        let no_id = r#"{"kind":"terminal","command":null,"name":"x","renamed":false,"cwd":null}"#;
        assert!(serde_json::from_str::<Tab>(no_id).is_err());
        let bad_session = r#"{"tabId":"a","kind":"claude","claudeSession":"; rm -rf ~","name":"x","renamed":false,"cwd":null}"#;
        assert!(serde_json::from_str::<Tab>(bad_session).is_err());
    }
}
