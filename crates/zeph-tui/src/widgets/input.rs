use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, InputMode};
use crate::theme::Theme;

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let title = match app.input_mode() {
        InputMode::Normal => " Press 'i' to type ",
        InputMode::Insert => " Input (Esc to cancel) ",
    };

    let paragraph = Paragraph::new(app.input())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(title),
        )
        .style(theme.input_cursor);

    frame.render_widget(paragraph, area);

    if matches!(app.input_mode(), InputMode::Insert) {
        // Use unicode display width for correct cursor placement with CJK/emoji
        let prefix: String = app.input().chars().take(app.cursor_position()).collect();
        #[allow(clippy::cast_possible_truncation)]
        let cursor_x = area.x + prefix.width() as u16 + 1;
        let cursor_y = area.y + 1;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}
