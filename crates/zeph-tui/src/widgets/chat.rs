use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, MessageRole};
use crate::theme::Theme;

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let theme = Theme::default();

    let mut lines: Vec<Line<'_>> = Vec::new();

    for msg in app.messages() {
        let (prefix, style) = match msg.role {
            MessageRole::User => ("[user] ", theme.user_message),
            MessageRole::Assistant => ("[zeph] ", theme.assistant_message),
            MessageRole::System => ("[system] ", theme.system_message),
        };

        let indent = " ".repeat(prefix.len());
        let content_lines: Vec<&str> = msg.content.split('\n').collect();

        for (i, line) in content_lines.iter().enumerate() {
            let text = if i == 0 {
                if msg.streaming && content_lines.len() == 1 {
                    format!("{prefix}{line}\u{258c}")
                } else {
                    format!("{prefix}{line}")
                }
            } else if msg.streaming && i == content_lines.len() - 1 {
                format!("{indent}{line}\u{258c}")
            } else {
                format!("{indent}{line}")
            };
            lines.push(Line::from(Span::styled(text, style)));
        }

        lines.push(Line::default());
    }

    let inner_height = area.height.saturating_sub(2) as usize;
    let total = lines.len();

    // Push messages to the bottom when content doesn't fill viewport
    if total < inner_height {
        let padding = inner_height - total;
        let mut padded = vec![Line::default(); padding];
        padded.append(&mut lines);
        lines = padded;
    }

    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_height);
    let scroll = max_scroll.saturating_sub(app.scroll_offset());

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

    if total > inner_height {
        let indicator_x = area.x + area.width.saturating_sub(2);
        // Content overflows above visible area
        if scroll > 0 {
            let y = area.y + 1;
            frame
                .buffer_mut()
                .set_string(indicator_x, y, "\u{25b2}", Style::default());
        }
        // Content overflows below (user scrolled up)
        if app.scroll_offset() > 0 {
            let y = area.y + area.height.saturating_sub(2);
            frame
                .buffer_mut()
                .set_string(indicator_x, y, "\u{25bc}", Style::default());
        }
    }
}
