use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct SettingsSyncSettingsContent {
    /// Whether the GitHub-backed settings sync feature is enabled.
    ///
    /// Default: false
    pub enabled: Option<bool>,
    /// The private repository used to store synced settings.
    ///
    /// Default: "zed_settings"
    pub repo_name: Option<String>,
    /// Whether local settings changes automatically trigger a sync push.
    ///
    /// Default: false
    pub auto_sync_on_change: Option<bool>,
    /// Whether Windows should participate in sync.
    ///
    /// Default: true
    pub sync_windows: Option<bool>,
    /// Whether macOS should participate in sync.
    ///
    /// Default: true
    pub sync_macos: Option<bool>,
    /// Whether Linux should participate in sync.
    ///
    /// Default: true
    pub sync_linux: Option<bool>,
    /// Whether to sync the user settings file.
    ///
    /// Default: true
    pub include_settings: Option<bool>,
    /// Whether to sync the user keymap file.
    ///
    /// Default: true
    pub include_keymap: Option<bool>,
}
