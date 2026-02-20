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

    if app.editing_queued() {
        block = block.title_bottom(Span::styled(" [editing queued] ", theme.highlight));
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

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use tokio::sync::mpsc;

    use crate::app::App;
    use crate::test_utils::render_to_string;

    fn make_app() -> App {
        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        App::new(user_tx, agent_rx)
    }

    #[test]
    fn input_insert_mode() {
        let app = make_app();
        let output = render_to_string(40, 5, |frame, area| {
            super::render(&app, frame, area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn input_normal_mode() {
        let mut app = make_app();
        app.handle_event(crate::event::AppEvent::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let output = render_to_string(40, 5, |frame, area| {
            super::render(&app, frame, area);
        });
        assert_snapshot!(output);
    }
}
