use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, InputMode};
use crate::theme::Theme;

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let title = match app.input_mode() {
        InputMode::Normal => " Press 'i' to type ",
        InputMode::Insert => " Input (Esc to cancel) ",
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(title);

    if app.queued_count() > 0 {
        let badge = format!(" [+{} queued] ", app.queued_count());
        block = block.title_bottom(Span::styled(badge, theme.highlight));
    }

    let paragraph = Paragraph::new(app.input())
        .block(block)
        .style(theme.input_cursor)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);

    if matches!(app.input_mode(), InputMode::Insert) {
        let prefix: String = app.input().chars().take(app.cursor_position()).collect();
        let last_line = prefix.rsplit('\n').next().unwrap_or(&prefix);
        #[allow(clippy::cast_possible_truncation)]
        let cursor_x = area.x + last_line.width() as u16 + 1;
        let line_count = prefix.matches('\n').count();
        #[allow(clippy::cast_possible_truncation)]
        let cursor_y = area.y + 1 + line_count as u16;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}
