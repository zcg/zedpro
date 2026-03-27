use std::num::NonZeroUsize;

use crate::DockPosition;
use collections::HashMap;
use gpui::{App, Hsla, WindowBackgroundAppearance};
use serde::Deserialize;
pub use settings::{
    AutosaveSetting, BottomDockLayout, EncodingDisplayOptions, InactiveOpacity,
    PaneSplitDirectionHorizontal, PaneSplitDirectionVertical, RegisterSetting,
    RestoreOnStartupBehavior, Settings,
};
use theme::ActiveTheme;

#[derive(RegisterSetting)]
pub struct WorkspaceSettings {
    pub active_pane_modifiers: ActivePanelModifiers,
    pub bottom_dock_layout: settings::BottomDockLayout,
    pub pane_split_direction_horizontal: settings::PaneSplitDirectionHorizontal,
    pub pane_split_direction_vertical: settings::PaneSplitDirectionVertical,
    pub centered_layout: settings::CenteredLayoutSettings,
    pub confirm_quit: bool,
    pub show_call_status_icon: bool,
    pub autosave: AutosaveSetting,
    pub restore_on_startup: settings::RestoreOnStartupBehavior,
    pub restore_on_file_reopen: bool,
    pub drop_target_size: f32,
    pub use_system_path_prompts: bool,
    pub use_system_prompts: bool,
    pub command_aliases: HashMap<String, String>,
    pub max_tabs: Option<NonZeroUsize>,
    pub when_closing_with_no_tabs: settings::CloseWindowWhenNoItems,
    pub on_last_window_closed: settings::OnLastWindowClosed,
    pub text_rendering_mode: settings::TextRenderingMode,
    pub resize_all_panels_in_dock: Vec<DockPosition>,
    pub close_on_file_delete: bool,
    pub close_panel_on_toggle: bool,
    pub use_system_window_tabs: bool,
    pub zoomed_padding: bool,
    pub window_decorations: settings::WindowDecorations,
    pub window_background_material: settings::WindowBackgroundMaterial,
    pub window_background_material_opacity: settings::WindowBackgroundMaterialOpacity,
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub struct ActivePanelModifiers {
    /// Size of the border surrounding the active pane.
    /// When set to 0, the active pane doesn't have any border.
    /// The border is drawn inset.
    ///
    /// Default: `0.0`
    pub border_size: f32,
    /// Opacity of inactive panels.
    /// When set to 1.0, the inactive panes have the same opacity as the active one.
    /// If set to 0, the inactive panes content will not be visible at all.
    /// Values are clamped to the [0.0, 1.0] range.
    ///
    /// Default: `1.0`
    pub inactive_opacity: InactiveOpacity,
}

impl Default for ActivePanelModifiers {
    fn default() -> Self {
        Self {
            border_size: 0.0,
            inactive_opacity: InactiveOpacity::from(1.0),
        }
    }
}

#[derive(Deserialize, RegisterSetting)]
pub struct TabBarSettings {
    pub show: bool,
    pub show_nav_history_buttons: bool,
    pub show_tab_bar_buttons: bool,
    pub show_pinned_tabs_in_separate_row: bool,
}

impl Settings for WorkspaceSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let workspace = &content.workspace;
        Self {
            active_pane_modifiers: ActivePanelModifiers {
                border_size: workspace
                    .active_pane_modifiers
                    .unwrap()
                    .border_size
                    .unwrap(),
                inactive_opacity: workspace
                    .active_pane_modifiers
                    .unwrap()
                    .inactive_opacity
                    .unwrap(),
            },
            bottom_dock_layout: workspace.bottom_dock_layout.unwrap(),
            pane_split_direction_horizontal: workspace.pane_split_direction_horizontal.unwrap(),
            pane_split_direction_vertical: workspace.pane_split_direction_vertical.unwrap(),
            centered_layout: workspace.centered_layout.unwrap(),
            confirm_quit: workspace.confirm_quit.unwrap(),
            show_call_status_icon: workspace.show_call_status_icon.unwrap(),
            autosave: workspace.autosave.unwrap(),
            restore_on_startup: workspace.restore_on_startup.unwrap(),
            restore_on_file_reopen: workspace.restore_on_file_reopen.unwrap(),
            drop_target_size: workspace.drop_target_size.unwrap(),
            use_system_path_prompts: workspace.use_system_path_prompts.unwrap(),
            use_system_prompts: workspace.use_system_prompts.unwrap(),
            command_aliases: workspace.command_aliases.clone(),
            max_tabs: workspace.max_tabs,
            when_closing_with_no_tabs: workspace.when_closing_with_no_tabs.unwrap(),
            on_last_window_closed: workspace.on_last_window_closed.unwrap(),
            text_rendering_mode: workspace.text_rendering_mode.unwrap(),
            resize_all_panels_in_dock: workspace
                .resize_all_panels_in_dock
                .clone()
                .unwrap()
                .into_iter()
                .map(Into::into)
                .collect(),
            close_on_file_delete: workspace.close_on_file_delete.unwrap(),
            close_panel_on_toggle: workspace.close_panel_on_toggle.unwrap(),
            use_system_window_tabs: workspace.use_system_window_tabs.unwrap(),
            zoomed_padding: workspace.zoomed_padding.unwrap(),
            window_decorations: workspace.window_decorations.unwrap(),
            window_background_material: workspace.window_background_material.unwrap(),
            window_background_material_opacity: workspace
                .window_background_material_opacity
                .unwrap(),
        }
    }
}

pub fn effective_window_background_appearance(cx: &App) -> WindowBackgroundAppearance {
    #[cfg(target_os = "windows")]
    {
        let settings = WorkspaceSettings::get_global(cx);
        gpui::set_windows_window_background_material_opacity(
            settings.window_background_material_opacity.0,
        );

        match settings.window_background_material {
            settings::WindowBackgroundMaterial::Theme => cx.theme().window_background_appearance(),
            settings::WindowBackgroundMaterial::Acrylic => WindowBackgroundAppearance::Blurred,
            settings::WindowBackgroundMaterial::Mica => WindowBackgroundAppearance::MicaBackdrop,
            settings::WindowBackgroundMaterial::MicaAlt => {
                WindowBackgroundAppearance::MicaAltBackdrop
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        cx.theme().window_background_appearance()
    }
}

pub fn has_custom_window_background_material(cx: &App) -> bool {
    theme::has_custom_window_background_material(cx)
}

pub fn material_surface_color(color: Hsla, factor: f32, cx: &App) -> Hsla {
    theme::material_surface_color(color, factor, cx)
}

pub fn material_popup_surface_color(color: Hsla, factor: f32, cx: &App) -> Hsla {
    theme::material_popup_surface_color(color, factor, cx)
}

pub fn material_root_surface_color(color: Hsla, cx: &App) -> Hsla {
    if has_custom_window_background_material(cx) {
        color
    } else {
        material_surface_color(color, 0.72, cx)
    }
}

pub fn material_panel_shell_color(color: Hsla, cx: &App) -> Hsla {
    if !has_custom_window_background_material(cx) {
        return color;
    }

    if color.a < 0.995 {
        return material_popup_surface_color(color, 0.82, cx);
    }

    material_surface_color(color, 0.96, cx)
}

pub fn material_panel_backdrop_color(color: Hsla, cx: &App) -> Hsla {
    if !has_custom_window_background_material(cx) {
        return color;
    }

    let opacity = WorkspaceSettings::get_global(cx)
        .window_background_material_opacity
        .0
        .clamp(0.0, 1.0);
    let backdrop_alpha = if opacity <= 0.35 {
        lerp(0.10, 0.20, opacity / 0.35)
    } else {
        lerp(0.20, 0.34, (opacity - 0.35) / 0.65)
    };

    material_popup_surface_color(color, 0.66, cx).opacity(backdrop_alpha)
}

pub fn material_sticky_surface_color(color: Hsla, factor: f32, cx: &App) -> Hsla {
    if !has_custom_window_background_material(cx) {
        return color;
    }
    material_popup_surface_color(color, factor, cx)
}

pub fn material_workspace_wash_color(cx: &App) -> Option<Hsla> {
    if !has_custom_window_background_material(cx) {
        return None;
    }

    let settings = WorkspaceSettings::get_global(cx);
    if matches!(
        settings.window_background_material,
        settings::WindowBackgroundMaterial::Acrylic
    ) {
        return None;
    }

    let opacity = settings
        .window_background_material_opacity
        .0
        .clamp(0.0, 1.0);
    let wash_alpha = if opacity <= 0.35 {
        lerp(0.16, 0.28, opacity / 0.35)
    } else {
        lerp(0.28, 0.40, (opacity - 0.35) / 0.65)
    };

    Some(
        material_popup_surface_color(cx.theme().colors().panel_overlay_background, 0.72, cx)
            .opacity(wash_alpha),
    )
}

fn lerp(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t.clamp(0.0, 1.0)
}

impl Settings for TabBarSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let tab_bar = content.tab_bar.clone().unwrap();
        TabBarSettings {
            show: tab_bar.show.unwrap(),
            show_nav_history_buttons: tab_bar.show_nav_history_buttons.unwrap(),
            show_tab_bar_buttons: tab_bar.show_tab_bar_buttons.unwrap(),
            show_pinned_tabs_in_separate_row: tab_bar.show_pinned_tabs_in_separate_row.unwrap(),
        }
    }
}

#[derive(Deserialize, RegisterSetting)]
pub struct StatusBarSettings {
    pub show: bool,
    pub show_active_file: bool,
    pub active_language_button: bool,
    pub cursor_position_button: bool,
    pub line_endings_button: bool,
    pub active_encoding_button: EncodingDisplayOptions,
}

impl Settings for StatusBarSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let status_bar = content.status_bar.clone().unwrap();
        StatusBarSettings {
            show: status_bar.show.unwrap(),
            show_active_file: status_bar.show_active_file.unwrap(),
            active_language_button: status_bar.active_language_button.unwrap(),
            cursor_position_button: status_bar.cursor_position_button.unwrap(),
            line_endings_button: status_bar.line_endings_button.unwrap(),
            active_encoding_button: status_bar.active_encoding_button.unwrap(),
        }
    }
}
