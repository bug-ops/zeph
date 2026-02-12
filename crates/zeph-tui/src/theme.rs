use ratatui::style::{Color, Modifier, Style};

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
            header: Style::default().fg(Color::White).bg(Color::Blue),
            panel_border: Style::default().fg(Color::Gray),
            panel_title: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            highlight: Style::default().fg(Color::Green),
            error: Style::default().fg(Color::Red),
            thinking_message: Style::default().fg(Color::DarkGray),
            code_inline: Style::default().fg(Color::Yellow),
            code_block: Style::default().fg(Color::Green),
            streaming_cursor: Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::SLOW_BLINK),
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
