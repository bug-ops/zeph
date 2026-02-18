use ratatui::style::{Color, Modifier, Style};

pub struct SyntaxTheme {
    pub keyword: Style,
    pub string: Style,
    pub comment: Style,
    pub function: Style,
    pub r#type: Style,
    pub number: Style,
    pub operator: Style,
    pub variable: Style,
    pub attribute: Style,
    pub punctuation: Style,
    pub constant: Style,
    pub default: Style,
}

pub struct Theme {
    pub user_message: Style,
    pub assistant_message: Style,
    pub system_message: Style,
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
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_message: Style::default().fg(Color::Cyan),
            assistant_message: Style::default().fg(Color::White),
            system_message: Style::default().fg(Color::DarkGray),
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
            streaming_cursor: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::SLOW_BLINK),
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
