use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, MessageRole};
use crate::theme::Theme;

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let mut lines: Vec<Line<'_>> = Vec::new();

    for msg in app.messages() {
        let (prefix, style) = match msg.role {
            MessageRole::User => ("[user] ", theme.user_message),
            MessageRole::Assistant => ("[assistant] ", theme.assistant_message),
            MessageRole::System => ("[system] ", theme.system_message),
        };

        let content = if msg.streaming {
            format!("{}{}\u{258c}", prefix, msg.content)
        } else {
            format!("{}{}", prefix, msg.content)
        };

        lines.push(Line::from(Span::styled(content, style)));
    }

    // Calculate visible height for scroll
    let inner_height = area.height.saturating_sub(2) as usize;
    let total = lines.len();
    let scroll = if total > inner_height {
        (total - inner_height).saturating_sub(app.scroll_offset())
    } else {
        0
    };

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(" Chat "),
        )
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));

    frame.render_widget(paragraph, area);
}
