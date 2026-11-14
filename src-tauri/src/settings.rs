//! User settings, edited on the settings tab and stored by `db` in the `settings`
//! table. Mirrored by `Settings` in src/main.ts.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The Claude flag that runs tools without asking.
pub const SKIP_PERMISSIONS_FLAG: &str = "--dangerously-skip-permissions";
/// The terminals' font size in pixels: the default, and the range allowed (also a
/// CHECK in the schema, see db.rs).
pub const DEFAULT_FONT_SIZE: u32 = 14;
const FONT_SIZES: std::ops::RangeInclusive<u32> = 8..=32;

fn default_font_size() -> u32 {
    DEFAULT_FONT_SIZE
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Start Claude tabs, and a `claude` started in a terminal tab, with
    /// `--dangerously-skip-permissions`.
    pub skip_permissions: bool,
    /// The workspace folder the sidebar shows; None for the home directory.
    pub workspace: Option<String>,
    /// When true, Claude tabs and a `claude` started in a terminal tab get
    /// `autoMemoryEnabled: false`; when false, nothing is passed and the user's own
    /// Claude configuration decides.
    pub disable_auto_memory: bool,
    /// The font size of every terminal, in pixels.
    #[serde(default = "default_font_size")]
    pub font_size: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { skip_permissions: false, workspace: None, disable_auto_memory: false, font_size: DEFAULT_FONT_SIZE }
    }
}

impl Settings {
    /// What Claude starts with when the stored settings cannot be read: the
    /// defaults (Claude asks before running tools), but with auto memory off, so a
    /// read error never turns memory back on for someone who disabled it.
    pub fn cautious() -> Self {
        Settings { disable_auto_memory: true, ..Settings::default() }
    }

    /// Checks settings before they replace `stored`. A changed workspace folder must be
    /// an existing directory (a leading `~` is kept, and expanded when used); blank
    /// means home. An unchanged one is not checked again: it may have gone since it
    /// was chosen, which must not block saving the other settings.
    pub fn validated(mut self, stored: &Settings) -> Result<Self, String> {
        if !FONT_SIZES.contains(&self.font_size) {
            return Err(format!("the font size must be {} to {}", FONT_SIZES.start(), FONT_SIZES.end()));
        }
        self.workspace = self.workspace.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty());
        if let Some(dir) = &self.workspace
            && self.workspace != stored.workspace
        {
            check_folder(dir)?;
        }
        Ok(self)
    }

    /// The arguments these settings add to every Claude misag starts.
    pub fn claude_args(&self) -> Vec<String> {
        if self.skip_permissions { vec![SKIP_PERMISSIONS_FLAG.into()] } else { vec![] }
    }

    /// The Claude settings keys these settings add to the `--settings` JSON of every
    /// Claude misag starts. A setting left at its default adds nothing, so the user's own Claude
    /// configuration decides.
    pub fn claude_settings(&self) -> Map<String, Value> {
        let mut keys = Map::new();
        if self.disable_auto_memory {
            keys.insert("autoMemoryEnabled".into(), Value::Bool(false));
        }
        keys
    }
}

/// Whether a Claude command line runs tools without asking: `SKIP_PERMISSIONS_FLAG`
/// or `--permission-mode bypassPermissions`, which Claude documents as equivalent.
/// A `permissions.defaultMode` in Claude's own settings files is not visible here.
pub fn skips_permissions(args: &[String]) -> bool {
    args.iter().enumerate().any(|(i, a)| {
        a == SKIP_PERMISSIONS_FLAG
            || a == "--permission-mode=bypassPermissions"
            || (a == "--permission-mode" && args.get(i + 1).is_some_and(|mode| mode == "bypassPermissions"))
    })
}

/// Fails unless `dir` is a directory misag can use, saying why.
fn check_folder(dir: &str) -> Result<(), String> {
    match std::fs::metadata(crate::session::expand_tilde(dir)) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(format!("{dir} is not a folder")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!("{dir} does not exist")),
        Err(e) => Err(format!("{dir} cannot be used: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frontend_sees_camel_case_fields() {
        let s = Settings { skip_permissions: true, workspace: Some("~/src".into()), disable_auto_memory: true, font_size: 16 };
        let json = serde_json::json!({ "skipPermissions": true, "workspace": "~/src", "disableAutoMemory": true, "fontSize": 16 });
        assert_eq!(serde_json::to_value(&s).unwrap(), json);
        assert_eq!(serde_json::from_value::<Settings>(json).unwrap(), s);
    }

    #[test]
    fn the_font_size_defaults_to_14_and_stays_in_range() {
        assert_eq!(Settings::default().font_size, 14);
        let missing = serde_json::json!({ "skipPermissions": false, "workspace": null, "disableAutoMemory": false });
        assert_eq!(serde_json::from_value::<Settings>(missing).unwrap().font_size, 14);
        let with = |size| Settings { font_size: size, ..Settings::default() }.validated(&Settings::default());
        assert_eq!(with(8).unwrap().font_size, 8);
        assert_eq!(with(32).unwrap().font_size, 32);
        assert_eq!(with(7).unwrap_err(), "the font size must be 8 to 32");
        assert_eq!(with(33).unwrap_err(), "the font size must be 8 to 32");
    }

    #[test]
    fn skipping_permissions_adds_the_flag() {
        assert!(Settings::default().claude_args().is_empty());
        let s = Settings { skip_permissions: true, ..Settings::default() };
        assert_eq!(s.claude_args(), ["--dangerously-skip-permissions"]);
    }

    #[test]
    fn tells_whether_a_command_line_skips_permission_prompts() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(skips_permissions(&args(&["claude", "--dangerously-skip-permissions"])));
        assert!(skips_permissions(&args(&["claude", "--permission-mode", "bypassPermissions", "-c"])));
        assert!(skips_permissions(&args(&["claude", "--permission-mode=bypassPermissions"])));
        assert!(!skips_permissions(&args(&["claude", "--permission-mode", "plan"])));
        assert!(!skips_permissions(&args(&["claude", "--allow-dangerously-skip-permissions"])), "only makes it available");
        assert!(!skips_permissions(&args(&["claude", "-p", "explain --dangerously-skip-permissions"])), "inside a prompt");
        assert!(!skips_permissions(&args(&["claude", "--permission-mode"])));
    }

    #[test]
    fn disabling_auto_memory_adds_its_claude_setting() {
        assert!(Settings::default().claude_settings().is_empty(), "auto leaves it to the user's config");
        let s = Settings { disable_auto_memory: true, ..Settings::default() };
        assert_eq!(Value::Object(s.claude_settings()), serde_json::json!({ "autoMemoryEnabled": false }));
    }

    #[test]
    fn a_new_workspace_folder_must_exist_and_blank_means_home() {
        let with = |dir: &str| Settings { workspace: Some(dir.into()), ..Settings::default() }.validated(&Settings::default());
        assert_eq!(with("  ").unwrap().workspace, None);
        assert_eq!(with(" ~ ").unwrap().workspace.as_deref(), Some("~"));
        assert_eq!(with("/no/such/dir").unwrap_err(), "/no/such/dir does not exist");
        let file = crate::test_support::temp_dir("settings-file").join("f");
        std::fs::write(&file, "").unwrap();
        assert!(with(file.to_str().unwrap()).unwrap_err().ends_with("is not a folder"));
    }

    #[test]
    fn an_unchanged_workspace_folder_is_not_checked_again() {
        // Chosen when it existed; since deleted or unmounted.
        let stored = Settings { workspace: Some("/no/such/dir".into()), ..Settings::default() };
        let toggled = Settings { skip_permissions: true, ..stored.clone() };
        assert_eq!(toggled.clone().validated(&stored), Ok(toggled));
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_read_says_why() {
        if crate::test_support::running_as_root() {
            return;
        }
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::temp_dir("settings-locked");
        std::fs::create_dir(dir.join("in")).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = check_folder(dir.join("in").to_str().unwrap()).unwrap_err();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.contains("cannot be used"), "{err}");
    }
}
