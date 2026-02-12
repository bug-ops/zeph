use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, MessageRole};
use crate::theme::Theme;

/// Returns the maximum scroll offset for the rendered content.
pub fn render(app: &App, frame: &mut Frame, area: Rect) -> usize {
    if area.width == 0 || area.height == 0 {
        return 0;
    }

    let theme = Theme::default();
    let inner_height = area.height.saturating_sub(2) as usize;
    let wrap_width = area.width.saturating_sub(2) as usize;

    let mut lines: Vec<Line<'_>> = Vec::new();

    for msg in app.messages() {
        let (prefix, base_style) = match msg.role {
            MessageRole::User => ("[user] ", theme.user_message),
            MessageRole::Assistant => ("[zeph] ", theme.assistant_message),
            MessageRole::System => ("[system] ", theme.system_message),
        };

        let indent = " ".repeat(prefix.len());
        let content_lines: Vec<&str> = msg.content.split('\n').collect();
        let is_assistant = msg.role == MessageRole::Assistant;
        let mut in_thinking = false;

        for (i, raw_line) in content_lines.iter().enumerate() {
            let (display, style) = if is_assistant {
                let mut display = (*raw_line).to_string();
                if display.contains("<think>") {
                    in_thinking = true;
                    display = display.replace("<think>", "");
                }
                if display.contains("</think>") {
                    display = display.replace("</think>", "");
                    in_thinking = false;
                }
                let style = if in_thinking {
                    theme.thinking_message
                } else {
                    base_style
                };
                (display, style)
            } else {
                ((*raw_line).to_string(), base_style)
            };

            let text = if i == 0 {
                if msg.streaming && content_lines.len() == 1 {
                    format!("{prefix}{display}\u{258c}")
                } else {
                    format!("{prefix}{display}")
                }
            } else if msg.streaming && i == content_lines.len() - 1 {
                format!("{indent}{display}\u{258c}")
            } else {
                format!("{indent}{display}")
            };

            // Pre-wrap long lines so lines.len() equals visual line count
            let wrapped = wrap_line(text, style, wrap_width);
            lines.extend(wrapped);
        }

        lines.push(Line::default());
    }

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
    let effective_offset = app.scroll_offset().min(max_scroll);
    let scroll = max_scroll - effective_offset;

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(" Chat "),
        )
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));

    frame.render_widget(paragraph, area);

    if total > inner_height {
        let indicator_x = area.x + area.width.saturating_sub(2);
        if scroll > 0 {
            let y = area.y + 1;
            frame
                .buffer_mut()
                .set_string(indicator_x, y, "\u{25b2}", Style::default());
        }
        if effective_offset > 0 {
            let y = area.y + area.height.saturating_sub(2);
            frame
                .buffer_mut()
                .set_string(indicator_x, y, "\u{25bc}", Style::default());
        }

        let track_height = inner_height.saturating_sub(2);
        if track_height > 0 {
            let thumb_size = (inner_height * track_height)
                .checked_div(total)
                .unwrap_or(track_height)
                .clamp(1, track_height);
            let thumb_pos = ((track_height - thumb_size) * scroll)
                .checked_div(max_scroll)
                .unwrap_or(0);
            let track_top = area.y + 2;
            let bar_x = area.x + area.width.saturating_sub(1);
            for row in 0..track_height {
                let ch = if row >= thumb_pos && row < thumb_pos + thumb_size {
                    "\u{2588}" // thumb
                } else {
                    "\u{2591}" // track
                };
                frame.buffer_mut().set_string(
                    bar_x,
                    track_top + row as u16,
                    ch,
                    Style::default().fg(ratatui::style::Color::DarkGray),
                );
            }
        }
    }

    max_scroll
}

fn wrap_line(text: String, style: Style, max_width: usize) -> Vec<Line<'static>> {
    if max_width == 0 {
        return vec![Line::from(Span::styled(text, style))];
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_width {
        return vec![Line::from(Span::styled(text, style))];
    }
    chars
        .chunks(max_width)
        .map(|chunk| {
            let s: String = chunk.iter().collect();
            Line::from(Span::styled(s, style))
        })
        .collect()
}
