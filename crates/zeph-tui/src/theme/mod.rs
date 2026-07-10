// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! TUI visual theme system.
//!
//! A [`Theme`] is a flat collection of [`Style`] and
//! [`Color`] values consumed by every widget render function.
//!
//! The palette-driven workflow:
//! 1. Load a [`SemanticPalette`] — from a built-in preset, user file, or default.
//! 2. Detect or override terminal colour capability via [`resolve_color_mode`].
//! 3. Call [`Theme::from_palette_with_mode`] **once** at startup.
//! 4. Store the result in [`crate::App`] and thread `&theme` into every render call.
//!
//! [`Theme::default()`] is byte-identical to the pre-2.0 hardcoded styles and is kept
//! for backward compatibility and tests. Use [`Theme::from_palette`] for new code.

pub mod color_mode;
pub mod palette;
pub mod presets;

pub use color_mode::{
    EffectiveColorMode, apply_mode, detect_unicode_capable, map_color, resolve_color_mode,
};
pub use palette::{ExtendedRoles, Rgb, SemanticPalette};
pub use presets::{Preset, ThemeLoadError, resolve_palette};

use ratatui::style::{Color, Modifier, Style};

use crate::theme::color_mode::EffectiveColorMode as Ecm;
use crate::theme::palette::SemanticPalette as Palette;

/// Ratatui [`Style`] mappings for tree-sitter syntax-highlight capture groups.
///
/// Each field corresponds to a tree-sitter capture name (e.g. `"keyword"`,
/// `"string"`, `"comment"`). The [`crate::highlight::SyntaxHighlighter`] uses
/// this struct to map highlight events to terminal styles.
///
/// The [`Default`] implementation provides a dark One Dark-inspired palette.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::theme::SyntaxTheme;
///
/// let theme = SyntaxTheme::default();
/// // Keywords are rendered bold.
/// use ratatui::style::Modifier;
/// assert!(theme.keyword.add_modifier.contains(Modifier::BOLD));
/// ```
pub struct SyntaxTheme {
    /// Style for language keywords (e.g. `fn`, `let`, `if`).
    pub keyword: Style,
    /// Style for string literals.
    pub string: Style,
    /// Style for comments.
    pub comment: Style,
    /// Style for function names.
    pub function: Style,
    /// Style for type names and constructors.
    pub r#type: Style,
    /// Style for numeric literals.
    pub number: Style,
    /// Style for operators.
    pub operator: Style,
    /// Style for variable names and parameters.
    pub variable: Style,
    /// Style for attributes and annotations.
    pub attribute: Style,
    /// Style for punctuation tokens.
    pub punctuation: Style,
    /// Style for constants and built-in values.
    pub constant: Style,
    /// Fallback style for unstyled source text.
    pub default: Style,
}

/// Visual theme for the TUI dashboard widgets.
///
/// Contains [`Style`] values for every distinct UI element — message roles,
/// input fields, borders, diff gutters, hyperlinks, and status elements.
/// The [`Default`] implementation provides a dark blue colour scheme.
///
/// Build a configured theme with [`Theme::from_palette_with_mode`] at startup and store it
/// in the TUI `App`; widget render functions receive `&Theme` as a parameter.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::theme::{Theme, SemanticPalette, resolve_color_mode};
/// use zeph_config::ColorMode;
///
/// let palette = SemanticPalette::zephyr();
/// // Use Truecolor so the test is deterministic regardless of the CI environment.
/// let mode = resolve_color_mode(ColorMode::Truecolor);
/// let theme = Theme::from_palette_with_mode(&palette, mode);
/// assert_ne!(theme.user_message, theme.assistant_message);
/// ```
pub struct Theme {
    /// Style applied to user-role chat messages.
    pub user_message: Style,
    /// Style applied to assistant-role chat messages.
    pub assistant_message: Style,
    /// Style applied to system-role chat messages.
    pub system_message: Style,
    /// Style for the text body of the input field.
    pub input_text: Style,
    /// Style for the blinking cursor in the input field.
    pub input_cursor: Style,
    /// Style for the status bar at the bottom of the screen.
    pub status_bar: Style,
    /// Style for the top header bar (provider / model info).
    pub header: Style,
    /// Style for panel border lines.
    pub panel_border: Style,
    /// Style for panel title labels.
    pub panel_title: Style,
    /// Style for highlighted / selected items.
    pub highlight: Style,
    /// Style for error messages and indicators.
    pub error: Style,
    /// Style for thinking / reasoning messages.
    pub thinking_message: Style,
    /// Style for inline code spans within chat messages.
    pub code_inline: Style,
    /// Style for multi-line code blocks.
    pub code_block: Style,
    /// Style for the streaming cursor shown while the model is generating.
    pub streaming_cursor: Style,
    /// Style for tool command lines (shell commands, etc.).
    pub tool_command: Style,
    /// Accent style for assistant messages (orange / warm tone).
    pub assistant_accent: Style,
    /// Accent style for tool output messages (olive / warm tone).
    pub tool_accent: Style,
    /// Background colour for added lines in diffs.
    pub diff_added_bg: Color,
    /// Background colour for removed lines in diffs.
    pub diff_removed_bg: Color,
    /// Background colour for word-level added regions in diffs.
    pub diff_word_added_bg: Color,
    /// Background colour for word-level removed regions in diffs.
    pub diff_word_removed_bg: Color,
    /// Style for the `+` gutter marker in diffs.
    pub diff_gutter_add: Style,
    /// Style for the `-` gutter marker in diffs.
    pub diff_gutter_remove: Style,
    /// Style for diff file/hunk headers.
    pub diff_header: Style,
    /// Style for hyperlinks.
    pub link: Style,
    /// Style for table border lines.
    pub table_border: Style,
    /// Background tint applied to user message lines.
    pub user_message_bg: Color,
    /// Style for turn-separator lines between role changes.
    pub turn_separator: Style,
    /// Style for the bullet of a successfully completed tool call.
    pub tool_success: Style,
    /// Style for the bullet of a failed tool call.
    pub tool_failure: Style,
    /// Syntax-highlight styles for code blocks and diffs.
    ///
    /// Initialised once at startup; passed as `&theme.syntax_theme` to `render_diff_lines`
    /// and the syntax highlighter so that `SyntaxTheme::default()` is never called per frame.
    pub syntax_theme: SyntaxTheme,
}

impl Theme {
    /// Derive all widget styles from a [`SemanticPalette`] at truecolor fidelity.
    ///
    /// Equivalent to `from_palette_with_mode(p, EffectiveColorMode::Truecolor)`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::theme::{Theme, SemanticPalette};
    ///
    /// let theme = Theme::from_palette(&SemanticPalette::zephyr());
    /// assert_ne!(theme.user_message, theme.assistant_message);
    /// ```
    #[must_use]
    pub fn from_palette(p: &SemanticPalette) -> Self {
        Self::from_palette_with_mode(p, Ecm::Truecolor)
    }

    /// Derive all widget styles from a [`SemanticPalette`], downgrading colours for the
    /// given terminal capability.
    ///
    /// This is the single derivation path — all `Rgb` values produced here pass through
    /// [`apply_mode`] (for `Style` fields) and [`map_color`] (for bare `Color` fields).
    /// The downgrade happens exactly once at startup, never per frame.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_tui::theme::{Theme, SemanticPalette, EffectiveColorMode};
    ///
    /// let t = Theme::from_palette_with_mode(&SemanticPalette::zephyr(), EffectiveColorMode::Ansi256);
    /// // In Ansi256 mode, colours are indexed — no Rgb variants in fg/bg.
    /// if let Some(fg) = t.user_message.fg {
    ///     assert!(!matches!(fg, ratatui::style::Color::Rgb(..)));
    /// }
    /// ```
    #[must_use]
    pub fn from_palette_with_mode(p: &Palette, mode: Ecm) -> Self {
        let am = |style: Style| apply_mode(style, mode);
        let mc = |color: Color| map_color(color, mode);

        let fg = |c: Rgb| Color::from(c);

        Self {
            user_message: am(Style::default().fg(fg(p.accent))),
            assistant_message: am(Style::default().fg(fg(p.text))),
            system_message: am(Style::default().fg(fg(p.muted))),
            input_text: am(Style::default().fg(fg(p.accent))),
            input_cursor: am(Style::default()
                .fg(fg(p.warning))
                .add_modifier(Modifier::BOLD)),
            status_bar: am(Style::default().fg(fg(p.text)).bg(fg(p.surface))),
            header: am(Style::default()
                .fg(fg(p.text))
                .bg(fg(p.extended.header_bg))
                .add_modifier(Modifier::BOLD)),
            panel_border: am(Style::default().fg(fg(p.border))),
            panel_title: am(Style::default().fg(fg(p.text)).add_modifier(Modifier::BOLD)),
            highlight: am(Style::default().fg(fg(p.extended.highlight))),
            error: am(Style::default().fg(fg(p.error))),
            thinking_message: am(Style::default().fg(fg(p.muted))),
            code_inline: am(Style::default()
                .fg(fg(p.info))
                .bg(fg(p.surface))
                .add_modifier(Modifier::BOLD)),
            code_block: am(Style::default().fg(fg(p.text)).bg(fg(p.surface))),
            streaming_cursor: am(Style::default().fg(fg(p.muted))),
            tool_command: am(Style::default()
                .fg(fg(p.warning))
                .add_modifier(Modifier::BOLD)),
            assistant_accent: am(Style::default().fg(fg(p.extended.accent_alt))),
            tool_accent: am(Style::default().fg(fg(p.extended.accent_alt))),
            // S3: bare Color fields go through map_color, not apply_mode.
            diff_added_bg: mc(Color::Rgb(0, 40, 0)),
            diff_removed_bg: mc(Color::Rgb(40, 0, 0)),
            diff_word_added_bg: mc(Color::Rgb(0, 80, 0)),
            diff_word_removed_bg: mc(Color::Rgb(80, 0, 0)),
            diff_gutter_add: am(Style::default().fg(fg(p.success))),
            diff_gutter_remove: am(Style::default().fg(fg(p.error))),
            diff_header: am(Style::default().fg(fg(p.muted))),
            link: am(Style::default()
                .fg(fg(p.accent))
                .add_modifier(Modifier::UNDERLINED)),
            table_border: am(Style::default().fg(fg(p.muted))),
            user_message_bg: mc(fg(p.surface)),
            turn_separator: am(Style::default().fg(fg(p.muted)).add_modifier(Modifier::DIM)),
            tool_success: am(Style::default().fg(fg(p.success))),
            tool_failure: am(Style::default().fg(fg(p.error))),
            syntax_theme: SyntaxTheme::default(),
        }
    }
}

impl Default for Theme {
    /// Returns the legacy hardcoded dark-blue colour scheme.
    ///
    /// This implementation is byte-identical to the pre-2.0 default and is kept for
    /// backward compatibility. For a configurable theme, use [`Theme::from_palette_with_mode`].
    fn default() -> Self {
        Self {
            user_message: Style::default().fg(Color::Cyan),
            assistant_message: Style::default().fg(Color::Rgb(200, 200, 210)),
            system_message: Style::default().fg(Color::DarkGray),
            input_text: Style::default().fg(Color::Cyan),
            input_cursor: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            status_bar: Style::default().fg(Color::White).bg(Color::DarkGray),
            header: Style::default()
                .fg(Color::Rgb(200, 220, 255))
                .bg(Color::Rgb(20, 40, 80))
                .add_modifier(Modifier::BOLD),
            panel_border: Style::default().fg(Color::Gray),
            panel_title: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            highlight: Style::default().fg(Color::Rgb(215, 150, 60)),
            error: Style::default().fg(Color::Red),
            thinking_message: Style::default().fg(Color::DarkGray),
            code_inline: Style::default()
                .fg(Color::Rgb(100, 180, 255))
                .bg(Color::Rgb(15, 30, 55))
                .add_modifier(Modifier::BOLD),
            code_block: Style::default()
                .fg(Color::Rgb(190, 175, 145))
                .bg(Color::Rgb(20, 25, 35)),
            streaming_cursor: Style::default().fg(Color::DarkGray),
            tool_command: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            assistant_accent: Style::default().fg(Color::Rgb(185, 85, 25)),
            tool_accent: Style::default().fg(Color::Rgb(140, 120, 50)),
            diff_added_bg: Color::Rgb(0, 40, 0),
            diff_removed_bg: Color::Rgb(40, 0, 0),
            diff_word_added_bg: Color::Rgb(0, 80, 0),
            diff_word_removed_bg: Color::Rgb(80, 0, 0),
            diff_gutter_add: Style::default().fg(Color::Green),
            diff_gutter_remove: Style::default().fg(Color::Red),
            diff_header: Style::default().fg(Color::DarkGray),
            link: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
            table_border: Style::default().fg(Color::DarkGray),
            user_message_bg: Color::Rgb(20, 25, 35),
            turn_separator: Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
            tool_success: Style::default().fg(Color::Green),
            tool_failure: Style::default().fg(Color::Red),
            syntax_theme: SyntaxTheme::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_has_distinct_message_styles() {
        let theme = Theme::default();
        assert_ne!(theme.user_message, theme.assistant_message);
        assert_ne!(theme.assistant_message, theme.system_message);
    }

    #[test]
    fn default_theme_status_bar_has_background() {
        let theme = Theme::default();
        assert_eq!(theme.status_bar.bg, Some(Color::DarkGray));
    }

    /// S4: set a sentinel palette and assert a non-default theme changes rendered output.
    #[test]
    fn from_palette_changes_user_message_fg() {
        let mut p = SemanticPalette::zephyr();
        // Use a distinctive colour that is not Color::Cyan (the default).
        p.accent = Rgb(0xAB, 0xCD, 0xEF);
        let theme = Theme::from_palette(&p);
        assert_ne!(
            theme.user_message.fg,
            Some(Color::Cyan),
            "palette-derived theme must differ from Theme::default() user_message"
        );
        assert_eq!(theme.user_message.fg, Some(Color::Rgb(0xAB, 0xCD, 0xEF)));
    }

    #[test]
    fn from_palette_never_strips_fg() {
        let theme = Theme::from_palette_with_mode(&SemanticPalette::zephyr(), Ecm::Never);
        // All Style fg/bg must be None in Never mode.
        assert_eq!(theme.user_message.fg, None);
        assert_eq!(theme.status_bar.bg, None);
        // Modifiers must be preserved.
        assert!(theme.header.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn from_palette_ansi256_no_rgb_fg() {
        let theme = Theme::from_palette_with_mode(&SemanticPalette::zephyr(), Ecm::Ansi256);
        // In Ansi256 mode the fg must NOT be Rgb — it must be Indexed.
        if let Some(fg) = theme.user_message.fg {
            assert!(
                !matches!(fg, Color::Rgb(..)),
                "Ansi256 mode must downgrade Rgb: {fg:?}"
            );
        }
    }

    /// S6: WCAG contrast smoke test — text/bg pair must be ≥ 4.5:1.
    #[test]
    fn zephyr_text_bg_contrast_aa() {
        let p = SemanticPalette::zephyr();
        let text_l = relative_luminance(p.text.0, p.text.1, p.text.2);
        let bg_l = relative_luminance(p.bg.0, p.bg.1, p.bg.2);
        let ratio = contrast_ratio(text_l, bg_l);
        assert!(
            ratio >= 4.5,
            "zephyr text/bg WCAG contrast {ratio:.2}:1 is below AA threshold 4.5:1"
        );
    }

    #[test]
    fn high_contrast_text_bg_contrast_aaa() {
        let p = presets::Preset::HighContrast.palette();
        let text_l = relative_luminance(p.text.0, p.text.1, p.text.2);
        let bg_l = relative_luminance(p.bg.0, p.bg.1, p.bg.2);
        let ratio = contrast_ratio(text_l, bg_l);
        assert!(
            ratio >= 7.0,
            "high-contrast text/bg WCAG contrast {ratio:.2}:1 is below AAA threshold 7.0:1"
        );
    }

    fn srgb_linearize(u: u8) -> f64 {
        let c = f64::from(u) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
        0.2126 * srgb_linearize(r) + 0.7152 * srgb_linearize(g) + 0.0722 * srgb_linearize(b)
    }

    fn contrast_ratio(l1: f64, l2: f64) -> f64 {
        let (lighter, darker) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
        (lighter + 0.05) / (darker + 0.05)
    }
}
