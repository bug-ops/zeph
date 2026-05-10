// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::style::{Color, Modifier, Style};

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
/// # Examples
///
/// ```rust
/// use zeph_tui::theme::Theme;
///
/// let theme = Theme::default();
/// // User and assistant messages use distinct colours.
/// assert_ne!(theme.user_message, theme.assistant_message);
/// ```
pub struct Theme {
    pub user_message: Style,
    pub assistant_message: Style,
    pub system_message: Style,
    pub input_text: Style,
    pub input_cursor: Style,
    pub status_bar: Style,
    pub header: Style,
    pub panel_border: Style,
    pub panel_title: Style,
    pub highlight: Style,
    pub error: Style,
    pub thinking_message: Style,
    pub code_inline: Style,
    pub code_block: Style,
    pub streaming_cursor: Style,
    pub tool_command: Style,
    pub assistant_accent: Style,
    pub tool_accent: Style,
    pub diff_added_bg: Color,
    pub diff_removed_bg: Color,
    pub diff_word_added_bg: Color,
    pub diff_word_removed_bg: Color,
    pub diff_gutter_add: Style,
    pub diff_gutter_remove: Style,
    pub diff_header: Style,
    pub link: Style,
    pub table_border: Style,
    /// Background tint applied to user message lines.
    pub user_message_bg: Color,
    /// Style for turn-separator lines between role changes.
    pub turn_separator: Style,
    /// Style for the bullet of a successfully completed tool call (green).
    pub tool_success: Style,
    /// Style for the bullet of a failed tool call (red).
    pub tool_failure: Style,
}

impl Default for Theme {
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
            code_block: Style::default().fg(Color::Rgb(190, 175, 145)),
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
}
