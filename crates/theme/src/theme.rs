#![deny(missing_docs)]

//! # Theme
//!
//! This crate provides the theme system for Zed.
//!
//! ## Overview
//!
//! A theme is a collection of colors used to build a consistent appearance for UI components across the application.

mod default_colors;
mod fallback_themes;
mod font_family_cache;
mod icon_theme;
mod icon_theme_schema;
mod registry;
mod scale;
mod schema;
mod styles;

use std::sync::Arc;

use ::settings::{Settings, SettingsContent};
use derive_more::{Deref, DerefMut};
use gpui::BorrowAppContext;
use gpui::Global;
use gpui::{
    App, AssetSource, Hsla, Pixels, SharedString, WindowAppearance, WindowBackgroundAppearance, px,
};
use serde::Deserialize;

pub use crate::default_colors::*;
pub use crate::fallback_themes::{apply_status_color_defaults, apply_theme_color_defaults};
pub use crate::font_family_cache::*;
pub use crate::icon_theme::*;
pub use crate::icon_theme_schema::*;
pub use crate::registry::*;
pub use crate::scale::*;
pub use crate::schema::*;
pub use crate::styles::*;

/// The name of the default dark theme.
pub const DEFAULT_DARK_THEME: &str = "One Dark";

/// Defines window border radius for platforms that use client side decorations.
pub const CLIENT_SIDE_DECORATION_ROUNDING: Pixels = px(10.0);
/// Defines window shadow size for platforms that use client side decorations.
pub const CLIENT_SIDE_DECORATION_SHADOW: Pixels = px(10.0);

/// The appearance of the theme.
#[derive(Debug, PartialEq, Clone, Copy, Deserialize)]
pub enum Appearance {
    /// A light appearance.
    Light,
    /// A dark appearance.
    Dark,
}

impl Appearance {
    /// Returns whether the appearance is light.
    pub fn is_light(&self) -> bool {
        match self {
            Self::Light => true,
            Self::Dark => false,
        }
    }
}

impl From<WindowAppearance> for Appearance {
    fn from(value: WindowAppearance) -> Self {
        match value {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
        }
    }
}

/// Which themes should be loaded. This is used primarily for testing.
pub enum LoadThemes {
    /// Only load the base theme.
    ///
    /// No user themes will be loaded.
    JustBase,

    /// Load all of the built-in themes.
    All(Box<dyn AssetSource>),
}

/// Initialize the theme system with default themes.
///
/// This sets up the [`ThemeRegistry`], [`FontFamilyCache`], [`SystemAppearance`],
/// and [`GlobalTheme`] with the default dark theme. It does NOT load bundled
/// themes from JSON or integrate with settings — use `theme_settings::init` for that.
pub fn init(themes_to_load: LoadThemes, cx: &mut App) {
    SystemAppearance::init(cx);
    let assets = match themes_to_load {
        LoadThemes::JustBase => Box::new(()) as Box<dyn AssetSource>,
        LoadThemes::All(assets) => assets,
    };
    ThemeRegistry::set_global(assets, cx);
    FontFamilyCache::init_global(cx);
    WindowMaterialThemeSettings::register(cx);

    let themes = ThemeRegistry::default_global(cx);
    let theme = themes.get(DEFAULT_DARK_THEME).unwrap_or_else(|_| {
        themes
            .list()
            .into_iter()
            .next()
            .map(|m| themes.get(&m.name).unwrap())
            .unwrap()
    });
    let icon_theme = themes.default_icon_theme().unwrap();
    cx.set_global(GlobalTheme::new(theme, icon_theme));
}

/// Implementing this trait allows accessing the active theme.
pub trait ActiveTheme {
    /// Returns the active theme.
    fn theme(&self) -> &Arc<Theme>;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Arc<Theme> {
        GlobalTheme::theme(self)
    }
}

#[derive(Clone, Copy, PartialEq)]
struct WindowMaterialThemeSettings {
    window_background_material: ::settings::WindowBackgroundMaterial,
    window_background_material_opacity: ::settings::WindowBackgroundMaterialOpacity,
}

impl Settings for WindowMaterialThemeSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            window_background_material: content.workspace.window_background_material.unwrap(),
            window_background_material_opacity: content
                .workspace
                .window_background_material_opacity
                .unwrap(),
        }
    }
}

/// The appearance of the system.
#[derive(Debug, Clone, Copy, Deref)]
pub struct SystemAppearance(pub Appearance);

impl Default for SystemAppearance {
    fn default() -> Self {
        Self(Appearance::Dark)
    }
}

#[derive(Deref, DerefMut, Default)]
struct GlobalSystemAppearance(SystemAppearance);

impl Global for GlobalSystemAppearance {}

impl SystemAppearance {
    /// Initializes the [`SystemAppearance`] for the application.
    pub fn init(cx: &mut App) {
        *cx.default_global::<GlobalSystemAppearance>() =
            GlobalSystemAppearance(SystemAppearance(cx.window_appearance().into()));
    }

    /// Returns the global [`SystemAppearance`].
    pub fn global(cx: &App) -> Self {
        cx.global::<GlobalSystemAppearance>().0
    }

    /// Returns a mutable reference to the global [`SystemAppearance`].
    pub fn global_mut(cx: &mut App) -> &mut Self {
        cx.global_mut::<GlobalSystemAppearance>()
    }
}

/// A theme family is a grouping of themes under a single name.
///
/// For example, the "One" theme family contains the "One Light" and "One Dark" themes.
///
/// It can also be used to package themes with many variants.
///
/// For example, the "Atelier" theme family contains "Cave", "Dune", "Estuary", "Forest", "Heath", etc.
pub struct ThemeFamily {
    /// The unique identifier for the theme family.
    pub id: String,
    /// The name of the theme family. This will be displayed in the UI, such as when adding or removing a theme family.
    pub name: SharedString,
    /// The author of the theme family.
    pub author: SharedString,
    /// The [Theme]s in the family.
    pub themes: Vec<Theme>,
    /// The color scales used by the themes in the family.
    /// Note: This will be removed in the future.
    pub scales: ColorScales,
}

/// A theme is the primary mechanism for defining the appearance of the UI.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// The unique identifier for the theme.
    pub id: String,
    /// The name of the theme.
    pub name: SharedString,
    /// The appearance of the theme (light or dark).
    pub appearance: Appearance,
    /// The colors and other styles for the theme.
    pub styles: ThemeStyles,
}

impl Theme {
    /// Returns the [`SystemColors`] for the theme.
    #[inline(always)]
    pub fn system(&self) -> &SystemColors {
        &self.styles.system
    }

    /// Returns the [`AccentColors`] for the theme.
    #[inline(always)]
    pub fn accents(&self) -> &AccentColors {
        &self.styles.accents
    }

    /// Returns the [`PlayerColors`] for the theme.
    #[inline(always)]
    pub fn players(&self) -> &PlayerColors {
        &self.styles.player
    }

    /// Returns the [`ThemeColors`] for the theme.
    #[inline(always)]
    pub fn colors(&self) -> &ThemeColors {
        &self.styles.colors
    }

    /// Returns the [`SyntaxTheme`] for the theme.
    #[inline(always)]
    pub fn syntax(&self) -> &Arc<SyntaxTheme> {
        &self.styles.syntax
    }

    /// Returns the [`StatusColors`] for the theme.
    #[inline(always)]
    pub fn status(&self) -> &StatusColors {
        &self.styles.status
    }

    /// Returns the [`Appearance`] for the theme.
    #[inline(always)]
    pub fn appearance(&self) -> Appearance {
        self.appearance
    }

    /// Returns the [`WindowBackgroundAppearance`] for the theme.
    #[inline(always)]
    pub fn window_background_appearance(&self) -> WindowBackgroundAppearance {
        self.styles.window_background_appearance
    }

    /// Darkens the color by reducing its lightness.
    /// The resulting lightness is clamped to ensure it doesn't go below 0.0.
    ///
    /// The first value darkens light appearance mode, the second darkens appearance dark mode.
    ///
    /// Note: This is a tentative solution and may be replaced with a more robust color system.
    pub fn darken(&self, color: Hsla, light_amount: f32, dark_amount: f32) -> Hsla {
        let amount = match self.appearance {
            Appearance::Light => light_amount,
            Appearance::Dark => dark_amount,
        };
        let mut hsla = color;
        hsla.l = (hsla.l - amount).max(0.0);
        hsla
    }
}

/// Deserializes an icon theme from the given bytes.
pub fn deserialize_icon_theme(bytes: &[u8]) -> anyhow::Result<IconThemeFamilyContent> {
    let icon_theme_family: IconThemeFamilyContent = serde_json_lenient::from_slice(bytes)?;

    Ok(icon_theme_family)
}

/// The active theme.
pub struct GlobalTheme {
    theme: Arc<Theme>,
    icon_theme: Arc<IconTheme>,
}
impl Global for GlobalTheme {}

impl GlobalTheme {
    /// Creates a new [`GlobalTheme`] with the given theme and icon theme.
    pub fn new(theme: Arc<Theme>, icon_theme: Arc<IconTheme>) -> Self {
        Self { theme, icon_theme }
    }

    /// Updates the active theme.
    pub fn update_theme(cx: &mut App, theme: Arc<Theme>) {
        cx.update_global::<Self, _>(|this, _| this.theme = theme);
    }

    /// Updates the active icon theme.
    pub fn update_icon_theme(cx: &mut App, icon_theme: Arc<IconTheme>) {
        cx.update_global::<Self, _>(|this, _| this.icon_theme = icon_theme);
    }

    /// Returns the active theme.
    pub fn theme(cx: &App) -> &Arc<Theme> {
        &cx.global::<Self>().theme
    }

    /// Returns the active icon theme.
    pub fn icon_theme(cx: &App) -> &Arc<IconTheme> {
        &cx.global::<Self>().icon_theme
    }
}

/// Applies the active Windows-specific window material override to a theme.
///
/// On non-Windows platforms this returns the theme unchanged.
pub fn apply_system_window_material_overrides(theme: Arc<Theme>, cx: &App) -> Arc<Theme> {
    #[cfg(target_os = "windows")]
    {
        let settings = *WindowMaterialThemeSettings::get_global(cx);
        apply_window_material_theme_overrides(
            theme,
            settings.window_background_material,
            settings.window_background_material_opacity.0,
        )
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cx;
        theme
    }
}

/// Returns the current window material override state used by theme rendering.
pub fn window_material_theme_overrides(
    cx: &App,
) -> (
    ::settings::WindowBackgroundMaterial,
    ::settings::WindowBackgroundMaterialOpacity,
) {
    #[cfg(target_os = "windows")]
    {
        let settings = *WindowMaterialThemeSettings::get_global(cx);
        (
            settings.window_background_material,
            settings.window_background_material_opacity,
        )
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cx;
        (
            ::settings::WindowBackgroundMaterial::Theme,
            Default::default(),
        )
    }
}

#[cfg(target_os = "windows")]
fn apply_window_material_theme_overrides(
    theme: Arc<Theme>,
    material: ::settings::WindowBackgroundMaterial,
    opacity: f32,
) -> Arc<Theme> {
    use ::settings::WindowBackgroundMaterial;

    if material == WindowBackgroundMaterial::Theme {
        return theme;
    }

    let mut theme = (*theme).clone();
    theme.styles.window_background_appearance = match material {
        WindowBackgroundMaterial::Theme => theme.styles.window_background_appearance,
        WindowBackgroundMaterial::Acrylic => WindowBackgroundAppearance::Blurred,
        WindowBackgroundMaterial::Mica => WindowBackgroundAppearance::MicaBackdrop,
        WindowBackgroundMaterial::MicaAlt => WindowBackgroundAppearance::MicaAltBackdrop,
    };

    let colors = &mut theme.styles.colors;
    match material {
        // Keep Acrylic materially translucent across the whole workspace
        // instead of converging on near-opaque surfaces.
        WindowBackgroundMaterial::Acrylic => {
            let opacity = opacity.clamp(0.0, 1.0);
            // Tune Acrylic toward a denser Windows Terminal-like material so
            // bright desktop content does not wash out sidebars and editors.
            let chrome_alpha = material_density(opacity, 0.08, 0.34, 0.72);
            let surface_alpha = material_density(opacity, 0.04, 0.24, 0.58);
            let elevated_alpha = material_density(opacity, 0.06, 0.30, 0.66);
            let content_alpha = material_density(opacity, 0.03, 0.22, 0.56);
            let overlay_alpha = material_density(opacity, 0.08, 0.30, 0.72);
            let title_bar_background = material_blend(
                colors.title_bar_background,
                colors.background,
                0.08,
                chrome_alpha,
            );
            let title_bar_inactive_background = material_blend(
                colors.title_bar_inactive_background,
                colors.surface_background,
                0.08,
                chrome_alpha,
            );
            let shared_surface = set_alpha(
                title_bar_background.blend(colors.panel_background.opacity(0.30)),
                surface_alpha,
            );
            let shared_elevated = set_alpha(
                title_bar_background.blend(colors.elevated_surface_background.opacity(0.36)),
                elevated_alpha,
            );
            let shared_content = set_alpha(
                // Keep editor-like regions closer to sidebars and panels so
                // Acrylic does not fragment into visibly darker slabs.
                shared_surface.blend(colors.editor_background.opacity(0.04)),
                surface_alpha.max(content_alpha),
            );
            let hover_alpha = (overlay_alpha + 0.01_f32).min(0.94_f32);
            let active_alpha = (overlay_alpha + 0.03_f32).min(0.96_f32);
            let selected_alpha = (overlay_alpha + 0.05_f32).min(0.98_f32);
            let popup_alpha = (elevated_alpha + 0.04_f32).min(0.97_f32);
            let popup_hover_alpha = (popup_alpha + 0.02_f32).min(0.99_f32);

            colors.background = shared_surface;
            colors.surface_background = shared_surface;
            colors.elevated_surface_background = shared_elevated;
            colors.element_background = shared_elevated;
            colors.element_hover =
                material_blend(shared_surface, colors.element_hover, 0.10, hover_alpha);
            colors.element_active =
                material_blend(shared_surface, colors.element_active, 0.14, active_alpha);
            colors.element_selected = material_blend(
                shared_elevated,
                colors.element_selected,
                0.16,
                selected_alpha,
            );
            colors.element_disabled =
                material_blend(shared_surface, colors.element_disabled, 0.08, surface_alpha);
            colors.ghost_element_background = set_alpha(shared_surface, 0.0);
            colors.ghost_element_hover = material_blend(
                shared_surface,
                colors.ghost_element_hover,
                0.10,
                hover_alpha,
            );
            colors.ghost_element_active = material_blend(
                shared_surface,
                colors.ghost_element_active,
                0.14,
                active_alpha,
            );
            colors.ghost_element_selected = material_blend(
                shared_elevated,
                colors.ghost_element_selected,
                0.16,
                selected_alpha,
            );
            colors.ghost_element_disabled = set_alpha(shared_surface, 0.0);
            colors.title_bar_background = title_bar_background;
            colors.title_bar_inactive_background = title_bar_inactive_background;
            colors.toolbar_background = shared_surface;
            colors.tab_bar_background = shared_surface;
            colors.tab_inactive_background = material_blend(
                shared_surface,
                colors.tab_inactive_background,
                0.05,
                surface_alpha,
            );
            colors.tab_active_background = material_blend(
                shared_elevated,
                colors.tab_active_background,
                0.06,
                elevated_alpha,
            );
            colors.status_bar_background = shared_surface;
            colors.panel_background = shared_content;
            colors.panel_overlay_background = material_blend(
                shared_elevated,
                colors.panel_overlay_background,
                0.06,
                popup_alpha,
            );
            colors.panel_overlay_hover = material_blend(
                shared_elevated,
                colors.panel_overlay_hover,
                0.10,
                popup_hover_alpha,
            );
            colors.scrollbar_track_background = shared_surface;
            colors.editor_background = shared_content;
            colors.editor_gutter_background = shared_content;
            colors.editor_subheader_background = material_blend(
                shared_elevated,
                colors.editor_subheader_background,
                0.05,
                elevated_alpha,
            );
            colors.editor_active_line_background = material_blend(
                shared_content,
                colors.editor_active_line_background,
                0.08,
                hover_alpha,
            );
            colors.editor_highlighted_line_background = material_blend(
                shared_content,
                colors.editor_highlighted_line_background,
                0.12,
                active_alpha,
            );
            colors.editor_debugger_active_line_background = material_blend(
                shared_content,
                colors.editor_debugger_active_line_background,
                0.22,
                active_alpha,
            );
            colors.terminal_background = shared_content;
            colors.terminal_ansi_background = shared_content;
            colors.drop_target_background = cap_alpha(colors.drop_target_background, overlay_alpha);
            colors.search_match_background =
                cap_alpha(colors.search_match_background, elevated_alpha);
            colors.search_active_match_background =
                cap_alpha(colors.search_active_match_background, elevated_alpha);
            colors.vim_yank_background = cap_alpha(colors.vim_yank_background, overlay_alpha);
            colors.editor_document_highlight_read_background = cap_alpha(
                colors.editor_document_highlight_read_background,
                overlay_alpha,
            );
            colors.editor_document_highlight_write_background = cap_alpha(
                colors.editor_document_highlight_write_background,
                overlay_alpha,
            );
            colors.editor_document_highlight_bracket_background = cap_alpha(
                colors.editor_document_highlight_bracket_background,
                overlay_alpha,
            );
        }
        WindowBackgroundMaterial::Mica | WindowBackgroundMaterial::MicaAlt => {
            let opacity = opacity.clamp(0.0, 1.0);
            let (
                chrome_alpha,
                background_alpha,
                surface_alpha,
                elevated_alpha,
                content_alpha,
                overlay_alpha,
                background_tint,
                surface_tint,
                elevated_tint,
                content_tint,
            ) = match material {
                WindowBackgroundMaterial::Mica => (
                    material_density(opacity, 0.04, 0.14, 0.30),
                    material_density(opacity, 0.02, 0.07, 0.16),
                    material_density(opacity, 0.04, 0.09, 0.22),
                    material_density(opacity, 0.06, 0.12, 0.28),
                    material_density(opacity, 0.04, 0.08, 0.22),
                    material_density(opacity, 0.08, 0.16, 0.34),
                    0.01,
                    0.025,
                    0.04,
                    0.02,
                ),
                WindowBackgroundMaterial::MicaAlt => (
                    material_density(opacity, 0.06, 0.20, 0.40),
                    material_density(opacity, 0.03, 0.09, 0.20),
                    material_density(opacity, 0.05, 0.12, 0.26),
                    material_density(opacity, 0.08, 0.16, 0.34),
                    material_density(opacity, 0.05, 0.10, 0.24),
                    material_density(opacity, 0.10, 0.20, 0.38),
                    0.015,
                    0.035,
                    0.055,
                    0.025,
                ),
                WindowBackgroundMaterial::Acrylic | WindowBackgroundMaterial::Theme => {
                    unreachable!()
                }
            };

            let title_bar_background = material_blend(
                colors.title_bar_background,
                colors.background,
                0.035,
                chrome_alpha,
            );
            let title_bar_inactive_background = material_blend(
                colors.title_bar_inactive_background,
                colors.surface_background,
                0.035,
                chrome_alpha,
            );
            let background = material_blend(
                colors.background,
                title_bar_background,
                background_tint,
                background_alpha,
            );
            let surface = material_blend(
                colors.surface_background,
                title_bar_background,
                surface_tint,
                surface_alpha,
            );
            let elevated = material_blend(
                colors.elevated_surface_background,
                title_bar_background,
                elevated_tint,
                elevated_alpha,
            );
            let content = material_blend(
                colors.editor_background,
                surface,
                content_tint,
                content_alpha,
            );
            let hover_alpha = (overlay_alpha + 0.025_f32).min(0.60_f32);
            let active_alpha = (overlay_alpha + 0.05_f32).min(0.68_f32);
            let selected_alpha = (overlay_alpha + 0.08_f32).min(0.76_f32);
            let popup_alpha = (overlay_alpha + 0.06_f32).min(0.78_f32);
            let popup_hover_alpha = (popup_alpha + 0.035_f32).min(0.84_f32);

            colors.background = background;
            colors.surface_background = surface;
            colors.elevated_surface_background = elevated;
            colors.element_background =
                material_blend(colors.element_background, elevated, 0.10, elevated_alpha);
            colors.element_hover = material_blend(surface, colors.element_hover, 0.08, hover_alpha);
            colors.element_active =
                material_blend(surface, colors.element_active, 0.10, active_alpha);
            colors.element_selected =
                material_blend(elevated, colors.element_selected, 0.12, selected_alpha);
            colors.element_disabled =
                material_blend(surface, colors.element_disabled, 0.06, surface_alpha);
            colors.ghost_element_background = set_alpha(surface, 0.0);
            colors.ghost_element_hover =
                material_blend(surface, colors.ghost_element_hover, 0.08, hover_alpha);
            colors.ghost_element_active =
                material_blend(surface, colors.ghost_element_active, 0.10, active_alpha);
            colors.ghost_element_selected = material_blend(
                elevated,
                colors.ghost_element_selected,
                0.12,
                selected_alpha,
            );
            colors.ghost_element_disabled = set_alpha(surface, 0.0);
            colors.title_bar_background = title_bar_background;
            colors.title_bar_inactive_background = title_bar_inactive_background;
            colors.toolbar_background =
                material_blend(colors.toolbar_background, surface, 0.06, surface_alpha);
            colors.tab_bar_background = surface;
            colors.tab_inactive_background =
                material_blend(colors.tab_inactive_background, surface, 0.05, surface_alpha);
            colors.tab_active_background =
                material_blend(colors.tab_active_background, elevated, 0.08, elevated_alpha);
            colors.status_bar_background = material_blend(
                colors.status_bar_background,
                background,
                0.04,
                background_alpha,
            );
            colors.panel_background =
                material_blend(colors.panel_background, surface, 0.05, surface_alpha);
            colors.panel_overlay_background =
                material_blend(colors.panel_overlay_background, elevated, 0.10, popup_alpha);
            colors.panel_overlay_hover = material_blend(
                colors.panel_overlay_hover,
                elevated,
                0.12,
                popup_hover_alpha,
            );
            colors.scrollbar_track_background = background;
            colors.editor_background = content;
            colors.editor_gutter_background = content;
            colors.editor_subheader_background = material_blend(
                colors.editor_subheader_background,
                elevated,
                0.06,
                elevated_alpha,
            );
            colors.editor_active_line_background = material_blend(
                content,
                colors.editor_active_line_background,
                0.08,
                hover_alpha,
            );
            colors.editor_highlighted_line_background = material_blend(
                content,
                colors.editor_highlighted_line_background,
                0.10,
                active_alpha,
            );
            colors.editor_debugger_active_line_background = material_blend(
                content,
                colors.editor_debugger_active_line_background,
                0.20,
                active_alpha,
            );
            colors.terminal_background = content;
            colors.terminal_ansi_background = content;
            colors.drop_target_background = cap_alpha(colors.drop_target_background, overlay_alpha);
            colors.search_match_background =
                cap_alpha(colors.search_match_background, elevated_alpha);
            colors.search_active_match_background =
                cap_alpha(colors.search_active_match_background, elevated_alpha);
            colors.vim_yank_background = cap_alpha(colors.vim_yank_background, overlay_alpha);
            colors.editor_document_highlight_read_background = cap_alpha(
                colors.editor_document_highlight_read_background,
                overlay_alpha,
            );
            colors.editor_document_highlight_write_background = cap_alpha(
                colors.editor_document_highlight_write_background,
                overlay_alpha,
            );
            colors.editor_document_highlight_bracket_background = cap_alpha(
                colors.editor_document_highlight_bracket_background,
                overlay_alpha,
            );
        }
        WindowBackgroundMaterial::Theme => unreachable!(),
    }

    Arc::new(theme)
}

#[cfg(target_os = "windows")]
fn current_window_background_material(cx: &App) -> ::settings::WindowBackgroundMaterial {
    WindowMaterialThemeSettings::get_global(cx).window_background_material
}

#[cfg(not(target_os = "windows"))]
fn current_window_background_material(_: &App) -> ::settings::WindowBackgroundMaterial {
    ::settings::WindowBackgroundMaterial::Theme
}

/// Returns whether a Windows-specific window material override is active.
pub fn has_custom_window_background_material(cx: &App) -> bool {
    #[cfg(target_os = "windows")]
    {
        WindowMaterialThemeSettings::get_global(cx).window_background_material
            != ::settings::WindowBackgroundMaterial::Theme
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cx;
        false
    }
}

/// Blends a surface color toward the title bar material when a custom Windows
/// window material override is active.
pub fn material_surface_color(color: Hsla, factor: f32, cx: &App) -> Hsla {
    match current_window_background_material(cx) {
        ::settings::WindowBackgroundMaterial::Acrylic => {
            // `apply_window_material_theme_overrides` already converts the shared
            // workspace surfaces into translucent material colors. Re-applying the
            // title bar blend to those colors makes docked panels trend back
            // toward an opaque fill, which is especially visible in project,
            // terminal, agent, and debugger panels.
            if color.a < 0.995 {
                return color;
            }

            let base = cx.theme().colors().title_bar_background;
            base.blend(color.opacity(factor.clamp(0.0, 0.88)))
        }
        ::settings::WindowBackgroundMaterial::Mica
        | ::settings::WindowBackgroundMaterial::MicaAlt => {
            if color.a < 0.995 {
                return color;
            }

            let base = cx.theme().colors().title_bar_background;
            base.blend(color.opacity(factor.clamp(0.0, 0.88)))
        }
        ::settings::WindowBackgroundMaterial::Theme => color,
    }
}

/// Produces a denser popup surface that still matches the active Windows
/// material, avoiding the "see-through text" effect for menus and dialogs
/// rendered inside the same window.
pub fn material_popup_surface_color(color: Hsla, factor: f32, cx: &App) -> Hsla {
    match current_window_background_material(cx) {
        ::settings::WindowBackgroundMaterial::Acrylic
        | ::settings::WindowBackgroundMaterial::Mica
        | ::settings::WindowBackgroundMaterial::MicaAlt => {
            let chrome = Hsla {
                a: 1.0,
                ..cx.theme().colors().title_bar_background
            };
            let panel = Hsla {
                a: 1.0,
                ..cx.theme().colors().panel_background
            };
            let tint = Hsla { a: 1.0, ..color };
            let editor = Hsla {
                a: 1.0,
                ..cx.theme().colors().editor_background
            };
            let base = chrome
                .blend(panel.opacity(0.68))
                .blend(editor.opacity(0.24));

            base.blend(tint.opacity(factor.clamp(0.0, 0.96)))
                .opacity(0.998)
        }
        ::settings::WindowBackgroundMaterial::Theme => color,
    }
}

#[cfg(target_os = "windows")]
fn set_alpha(color: Hsla, alpha: f32) -> Hsla {
    Hsla { a: alpha, ..color }
}

#[cfg(target_os = "windows")]
fn cap_alpha(color: Hsla, max_alpha: f32) -> Hsla {
    if color.a <= max_alpha {
        color
    } else {
        Hsla {
            a: max_alpha,
            ..color
        }
    }
}

#[cfg(target_os = "windows")]
fn material_blend(base: Hsla, tint: Hsla, tint_strength: f32, alpha: f32) -> Hsla {
    set_alpha(base.blend(tint.opacity(tint_strength)), alpha)
}

#[cfg(target_os = "windows")]
const WINDOW_BACKGROUND_MATERIAL_DEFAULT_OPACITY: f32 = 0.35;

#[cfg(target_os = "windows")]
fn material_density(opacity: f32, min: f32, current: f32, max: f32) -> f32 {
    let opacity = opacity.clamp(0.0, 1.0);
    if opacity <= WINDOW_BACKGROUND_MATERIAL_DEFAULT_OPACITY {
        lerp(
            min,
            current,
            opacity / WINDOW_BACKGROUND_MATERIAL_DEFAULT_OPACITY,
        )
    } else {
        lerp(
            current,
            max,
            (opacity - WINDOW_BACKGROUND_MATERIAL_DEFAULT_OPACITY)
                / (1.0 - WINDOW_BACKGROUND_MATERIAL_DEFAULT_OPACITY),
        )
    }
}

#[cfg(target_os = "windows")]
fn lerp(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t.clamp(0.0, 1.0)
}
