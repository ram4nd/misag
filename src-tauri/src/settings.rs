//! User settings, edited on the settings tab and stored by `db` in the `settings`
//! table. Mirrored by `Settings` in src/main.ts.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Start Claude tabs with `--dangerously-skip-permissions`.
    pub skip_permissions: bool,
}

impl Settings {
    /// The arguments these settings add to every Claude tab.
    pub fn claude_args(&self) -> Vec<String> {
        if self.skip_permissions { vec!["--dangerously-skip-permissions".into()] } else { vec![] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frontend_sees_camel_case_fields() {
        let s = Settings { skip_permissions: true };
        assert_eq!(serde_json::to_value(&s).unwrap(), serde_json::json!({ "skipPermissions": true }));
        assert_eq!(serde_json::from_str::<Settings>(r#"{"skipPermissions":true}"#).unwrap(), s);
    }

    #[test]
    fn skipping_permissions_adds_the_flag() {
        assert!(Settings::default().claude_args().is_empty());
        assert_eq!(Settings { skip_permissions: true }.claude_args(), ["--dangerously-skip-permissions"]);
    }
}
