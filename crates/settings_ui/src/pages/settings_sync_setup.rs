use gpui::{Context, ScrollHandle, Window};
use ui::{Button, Color, Divider, Label, prelude::*};
use workspace::SettingsSyncState;

use crate::{
    SettingsWindow,
    components::{SettingsInputField, SettingsSectionHeader},
};

pub(crate) fn render_settings_sync_setup_page(
    _settings_window: &SettingsWindow,
    _scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let snapshot = SettingsSyncState::try_global(cx).map(|state| state.read(cx).snapshot(cx));

    let Some(snapshot) = snapshot else {
        return v_flex()
            .id("settings-sync-setup")
            .min_w_0()
            .pt_8()
            .gap_1p5()
            .child(SettingsSectionHeader::new("GitHub Settings Sync").no_padding(true))
            .child(div().px_8().child(
                Label::new("Settings sync is unavailable in this workspace.").color(Color::Muted),
            ))
            .into_any_element();
    };
    let synced_files = if snapshot.synced_files.is_empty() {
        "None".to_string()
    } else {
        snapshot.synced_files.join(", ")
    };

    v_flex()
        .id("settings-sync-setup")
        .min_w_0()
        .pt_8()
        .gap_1p5()
        .child(SettingsSectionHeader::new("GitHub Settings Sync").no_padding(true))
        .child(
            v_flex()
                .px_8()
                .gap_3()
                .child(
                    v_flex()
                        .gap_1()
                        .child(Label::new(format!(
                            "Current account: {}",
                            snapshot
                                .app_github_login
                                .clone()
                                .unwrap_or_else(|| "Not signed in".to_string())
                        )))
                        .child(Label::new(format!(
                            "Sync repository account: {}",
                            snapshot
                                .sync_owner_login
                                .clone()
                                .unwrap_or_else(|| "Not verified yet".to_string())
                        )))
                        .child(Label::new(format!(
                            "Stored token: {}",
                            if snapshot.token_available { "Available" } else { "Missing" }
                        )))
                        .child(Label::new(format!(
                            "Repository: {}",
                            snapshot.repo_name
                        )))
                        .child(Label::new(format!(
                            "Feature status: {} / Auto sync: {}",
                            if snapshot.enabled { "Enabled" } else { "Disabled" },
                            if snapshot.auto_sync_on_change {
                                "On"
                            } else {
                                "Off"
                            }
                        ))),
                )
                .child(Divider::horizontal())
                .child(
                    v_flex()
                        .gap_1()
                        .child(Label::new("GitHub Sync Token"))
                        .child(
                            Label::new(
                                "Paste a GitHub token with private repository read/write access. It will be saved to the system credential store and not to settings.json.",
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .child(
                            SettingsInputField::new()
                                .with_placeholder("ghp_xxx or fine-grained token")
                                .display_confirm_button()
                                .display_clear_button()
                                .on_confirm(|token, _window, cx| {
                                    if let Some(sync_state) = SettingsSyncState::try_global(cx) {
                                        sync_state.update(cx, |sync_state, cx| {
                                            sync_state.save_token(token, cx);
                                        });
                                    }
                                }),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("settings-sync-clear-stored-token", "Clear Stored Token")
                                        .on_click(|_, _window, cx| {
                                            if let Some(sync_state) = SettingsSyncState::try_global(cx)
                                            {
                                                sync_state.update(cx, |sync_state, cx| {
                                                    sync_state.save_token(None, cx);
                                                });
                                            }
                                        }),
                                )
                                .child(
                                    Button::new("settings-sync-push-now", "Push Now")
                                        .disabled(!snapshot.enabled || snapshot.is_syncing)
                                        .on_click(|_, _window, cx| {
                                            if let Some(sync_state) = SettingsSyncState::try_global(cx)
                                            {
                                                sync_state.update(cx, |sync_state, cx| {
                                                    sync_state.sync_now(cx);
                                                });
                                            }
                                        }),
                                )
                                .child(
                                    Button::new("settings-sync-pull-now", "Pull Now")
                                        .disabled(!snapshot.enabled || snapshot.is_syncing)
                                        .on_click(|_, _window, cx| {
                                            if let Some(sync_state) = SettingsSyncState::try_global(cx)
                                            {
                                                sync_state.update(cx, |sync_state, cx| {
                                                    sync_state.pull_now(cx);
                                                });
                                            }
                                        }),
                                ),
                        )
                        .child(
                            Label::new(
                                "Push uploads local files to GitHub. Pull replaces local files with the GitHub copy for the current platform.",
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
                .child(Divider::horizontal())
                .child(
                    v_flex()
                        .gap_1()
                        .child(Label::new(format!(
                            "Last action: {}",
                            snapshot
                                .last_action
                                .clone()
                                .unwrap_or_else(|| "None".to_string())
                        )))
                        .child(Label::new(format!(
                            "Last success: {}",
                            snapshot
                                .last_success_at
                                .clone()
                                .unwrap_or_else(|| "None".to_string())
                        )))
                        .child(Label::new(format!("Last files: {synced_files}")))
                        .when_some(snapshot.last_message.clone(), |this, message| {
                            this.child(Label::new(message).color(Color::Muted))
                        })
                        .when_some(snapshot.last_error.clone(), |this, error| {
                            this.child(Label::new(format!("Error: {error}")).color(Color::Error))
                        }),
                ),
        )
        .into_any_element()
}
