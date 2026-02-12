use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use throbber_widgets_tui::{BRAILLE_SIX, Throbber, WhichUse};

use crate::app::{App, MessageRole};
use crate::theme::Theme;

/// Returns the maximum scroll offset for the rendered content.
pub fn render(app: &mut App, frame: &mut Frame, area: Rect) -> usize {
    if area.width == 0 || area.height == 0 {
        return 0;
    }

    let theme = Theme::default();
    let inner_height = area.height.saturating_sub(2) as usize;
    let wrap_width = area.width.saturating_sub(2) as usize;

    let mut lines: Vec<Line<'_>> = Vec::new();

    for msg in app.messages() {
        if msg.role == MessageRole::Tool {
            render_tool_message(msg, app, &theme, wrap_width, &mut lines);
            lines.push(Line::default());
            continue;
        }

        let (prefix, base_style) = match msg.role {
            MessageRole::User => ("[user] ", theme.user_message),
            MessageRole::Assistant => ("[zeph] ", theme.assistant_message),
            MessageRole::System => ("[system] ", theme.system_message),
            MessageRole::Tool => unreachable!(),
        };

        let indent = " ".repeat(prefix.len());
        let is_assistant = msg.role == MessageRole::Assistant;

        let styled_lines = if is_assistant {
            render_with_thinking(&msg.content, base_style, &theme)
        } else {
            render_md(&msg.content, base_style, &theme)
        };

        for (i, spans) in styled_lines.iter().enumerate() {
            let mut line_spans = Vec::with_capacity(spans.len() + 1);
            let pfx = if i == 0 {
                prefix.to_string()
            } else {
                indent.clone()
            };
            let pfx_style = if is_assistant && !spans.is_empty() {
                spans[0].style
            } else {
                base_style
            };
            line_spans.push(Span::styled(pfx, pfx_style));
            line_spans.extend(spans.iter().cloned());

            let is_last_line = i == styled_lines.len() - 1;
            if msg.streaming && is_last_line {
                line_spans.push(Span::styled("\u{258c}".to_string(), theme.streaming_cursor));
            }

            lines.extend(wrap_spans(line_spans, wrap_width));
        }

        if styled_lines.is_empty() {
            let mut pfx_spans = vec![Span::styled(prefix.to_string(), base_style)];
            if msg.streaming {
                pfx_spans.push(Span::styled("\u{258c}".to_string(), theme.streaming_cursor));
            }
            lines.extend(wrap_spans(pfx_spans, wrap_width));
        }

        lines.push(Line::default());
    }

    let total = lines.len();

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
        render_scrollbar(
            frame,
            area,
            inner_height,
            total,
            scroll,
            effective_offset,
            max_scroll,
        );
    }

    render_thinking(app, frame, area, &theme);

    max_scroll
}

fn render_thinking(app: &mut App, frame: &mut Frame, area: Rect, theme: &Theme) {
    let Some(label) = app.status_label() else {
        return;
    };
    if area.height <= 3 {
        return;
    }
    let label = format!(" {label}");
    let y = area.y + area.height.saturating_sub(2);
    let throbber_area = Rect::new(area.x + 1, y, area.width.saturating_sub(2), 1);
    let throbber = Throbber::default()
        .label(label)
        .style(theme.assistant_message)
        .throbber_style(theme.highlight)
        .throbber_set(BRAILLE_SIX)
        .use_type(WhichUse::Spin);
    frame.render_stateful_widget(throbber, throbber_area, app.throbber_state_mut());
}

fn render_scrollbar(
    frame: &mut Frame,
    area: Rect,
    inner_height: usize,
    total: usize,
    scroll: usize,
    effective_offset: usize,
    max_scroll: usize,
) {
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
                "\u{2588}"
            } else {
                "\u{2591}"
            };
            let row_y = u16::try_from(row).unwrap_or(u16::MAX);
            frame.buffer_mut().set_string(
                bar_x,
                track_top + row_y,
                ch,
                Style::default().fg(ratatui::style::Color::DarkGray),
            );
        }
    }
}

const TOOL_OUTPUT_COLLAPSED_LINES: usize = 3;

fn render_tool_message(
    msg: &crate::app::ChatMessage,
    app: &App,
    theme: &Theme,
    wrap_width: usize,
    lines: &mut Vec<Line<'static>>,
) {
    let name = msg.tool_name.as_deref().unwrap_or("tool");
    let prefix = format!("[{name}] ");
    let content_lines: Vec<&str> = msg.content.lines().collect();

    // First line is always the command ($ ...)
    let cmd_line = content_lines.first().copied().unwrap_or("");
    let status_span = if msg.streaming {
        let len = BRAILLE_SIX.symbols.len();
        let idx = usize::try_from(
            app.throbber_state()
                .index()
                .rem_euclid(i8::try_from(len).unwrap_or(i8::MAX)),
        )
        .unwrap_or(0);
        let symbol = BRAILLE_SIX.symbols[idx];
        Span::styled(format!("{symbol} "), theme.streaming_cursor)
    } else {
        Span::styled("\u{2714} ".to_string(), theme.highlight)
    };
    let indent = " ".repeat(prefix.len());
    let cmd_spans: Vec<Span<'static>> = vec![
        Span::styled(prefix, theme.highlight),
        status_span,
        Span::styled(cmd_line.to_string(), theme.tool_command),
    ];
    lines.extend(wrap_spans(cmd_spans, wrap_width));

    // Output lines (everything after the command)
    if content_lines.len() > 1 {
        let output_lines = &content_lines[1..];
        let total = output_lines.len();
        let show_all = app.tool_expanded() || total <= TOOL_OUTPUT_COLLAPSED_LINES;
        let visible = if show_all {
            output_lines
        } else {
            &output_lines[..TOOL_OUTPUT_COLLAPSED_LINES]
        };

        for line in visible {
            let spans = vec![
                Span::styled(indent.clone(), Style::default()),
                Span::styled((*line).to_string(), theme.code_block),
            ];
            lines.extend(wrap_spans(spans, wrap_width));
        }

        if !show_all {
            let remaining = total - TOOL_OUTPUT_COLLAPSED_LINES;
            let hint = format!("{indent}... ({remaining} more lines, press 'e' to expand)");
            lines.push(Line::from(Span::styled(
                hint,
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
    }
}

fn render_with_thinking(
    content: &str,
    base_style: Style,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    let mut all_lines = Vec::new();
    let mut remaining = content;
    let mut in_thinking = false;

    while !remaining.is_empty() {
        if in_thinking {
            if let Some(end) = remaining.find("</think>") {
                let segment = &remaining[..end];
                if !segment.trim().is_empty() {
                    all_lines.extend(render_md(segment, theme.thinking_message, theme));
                }
                remaining = &remaining[end + "</think>".len()..];
                in_thinking = false;
            } else {
                if !remaining.trim().is_empty() {
                    all_lines.extend(render_md(remaining, theme.thinking_message, theme));
                }
                break;
            }
        } else if let Some(start) = remaining.find("<think>") {
            let segment = &remaining[..start];
            if !segment.trim().is_empty() {
                all_lines.extend(render_md(segment, base_style, theme));
            }
            remaining = &remaining[start + "<think>".len()..];
            in_thinking = true;
        } else {
            all_lines.extend(render_md(remaining, base_style, theme));
            break;
        }
    }

    all_lines
}

fn render_md(content: &str, base_style: Style, theme: &Theme) -> Vec<Vec<Span<'static>>> {
    let options = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(content, options);
    let mut renderer = MdRenderer::new(base_style, theme);
    for event in parser {
        renderer.push_event(event);
    }
    renderer.finish()
}

struct MdRenderer<'t> {
    lines: Vec<Vec<Span<'static>>>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    base_style: Style,
    theme: &'t Theme,
    in_code_block: bool,
}

impl<'t> MdRenderer<'t> {
    fn new(base_style: Style, theme: &'t Theme) -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            style_stack: vec![base_style],
            base_style,
            theme,
            in_code_block: false,
        }
    }

    fn push_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                self.push_style(
                    self.base_style
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                );
            }
            Event::End(TagEnd::Heading { .. }) => {
                self.pop_style();
                self.newline();
            }
            Event::Start(Tag::Strong) => {
                let s = self.current_style().add_modifier(Modifier::BOLD);
                self.push_style(s);
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
                self.pop_style();
            }
            Event::Start(Tag::Emphasis) => {
                let s = self.current_style().add_modifier(Modifier::ITALIC);
                self.push_style(s);
            }
            Event::Start(Tag::Strikethrough) => {
                let s = self.current_style().add_modifier(Modifier::CROSSED_OUT);
                self.push_style(s);
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                self.in_code_block = true;
                if let CodeBlockKind::Fenced(lang) = kind {
                    let lang = lang.trim();
                    if !lang.is_empty() {
                        self.current.push(Span::styled(
                            format!(" {lang} "),
                            self.base_style.add_modifier(Modifier::DIM),
                        ));
                        self.newline();
                    }
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                self.in_code_block = false;
                self.newline();
            }
            Event::Code(text) => {
                self.current
                    .push(Span::styled(text.to_string(), self.theme.code_inline));
            }
            Event::Text(text) => {
                let style = if self.in_code_block {
                    self.theme.code_block
                } else {
                    self.current_style()
                };
                let prefix = if self.in_code_block { "  " } else { "" };
                for (i, segment) in text.split('\n').enumerate() {
                    if i > 0 {
                        self.newline();
                    }
                    if !segment.is_empty() || !prefix.is_empty() {
                        self.current
                            .push(Span::styled(format!("{prefix}{segment}"), style));
                    }
                }
            }
            Event::Start(Tag::Item) => {
                self.current
                    .push(Span::styled("\u{2022} ".to_string(), self.theme.highlight));
            }
            Event::End(TagEnd::Item | TagEnd::Paragraph) | Event::SoftBreak | Event::HardBreak => {
                self.newline();
            }
            Event::Rule => {
                self.current.push(Span::styled(
                    "\u{2500}".repeat(20),
                    self.base_style.add_modifier(Modifier::DIM),
                ));
                self.newline();
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.current.push(Span::styled(
                    "\u{2502} ".to_string(),
                    self.base_style.add_modifier(Modifier::DIM),
                ));
            }
            _ => {}
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or(self.base_style)
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn newline(&mut self) {
        let line = std::mem::take(&mut self.current);
        self.lines.push(line);
    }

    fn finish(mut self) -> Vec<Vec<Span<'static>>> {
        if !self.current.is_empty() {
            self.newline();
        }
        // Remove trailing empty lines
        while self.lines.last().is_some_and(Vec::is_empty) {
            self.lines.pop();
        }
        self.lines
    }
}

fn wrap_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Line<'static>> {
    if max_width == 0 {
        return vec![Line::from(spans)];
    }

    let total: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if total <= max_width {
        return vec![Line::from(spans)];
    }

    let mut result: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut width = 0;

    for span in spans {
        let chars: Vec<char> = span.content.chars().collect();
        let mut pos = 0;

        while pos < chars.len() {
            let space = max_width.saturating_sub(width);
            if space == 0 {
                result.push(Line::from(std::mem::take(&mut current)));
                width = 0;
                continue;
            }
            let take = space.min(chars.len() - pos);
            let chunk: String = chars[pos..pos + take].iter().collect();
            current.push(Span::styled(chunk, span.style));
            width += take;
            pos += take;

            if width >= max_width && pos < chars.len() {
                result.push(Line::from(std::mem::take(&mut current)));
                width = 0;
            }
        }
    }

    if !current.is_empty() {
        result.push(Line::from(current));
    }

    if result.is_empty() {
        result.push(Line::default());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_md_plain() {
        let theme = Theme::default();
        let lines = render_md("hello world", theme.assistant_message, &theme);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][0].content, "hello world");
    }

    #[test]
    fn render_md_bold() {
        let theme = Theme::default();
        let base = theme.assistant_message;
        let lines = render_md("say **hello** now", base, &theme);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 3);
        assert_eq!(lines[0][0].content, "say ");
        assert_eq!(lines[0][1].content, "hello");
        assert_eq!(lines[0][1].style, base.add_modifier(Modifier::BOLD));
        assert_eq!(lines[0][2].content, " now");
    }

    #[test]
    fn render_md_inline_code() {
        let theme = Theme::default();
        let lines = render_md("use `foo` here", theme.assistant_message, &theme);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][1].content, "foo");
        assert_eq!(lines[0][1].style, theme.code_inline);
    }

    #[test]
    fn render_md_code_block() {
        let theme = Theme::default();
        let lines = render_md("```rust\nlet x = 1;\n```", theme.assistant_message, &theme);
        assert!(lines.len() >= 2);
        // Language tag line
        assert!(lines[0][0].content.contains("rust"));
        // Code content
        let code_line = &lines[1];
        assert!(code_line.iter().any(|s| s.content.contains("let x = 1")));
        assert!(code_line.iter().any(|s| s.style == theme.code_block));
    }

    #[test]
    fn render_md_list() {
        let theme = Theme::default();
        let lines = render_md("- first\n- second", theme.assistant_message, &theme);
        assert!(lines.len() >= 2);
        assert!(lines[0].iter().any(|s| s.content.contains('\u{2022}')));
    }

    #[test]
    fn render_md_heading() {
        let theme = Theme::default();
        let base = theme.assistant_message;
        let lines = render_md("# Title", base, &theme);
        assert!(!lines.is_empty());
        let heading_span = &lines[0][0];
        assert_eq!(heading_span.content, "Title");
        assert_eq!(
            heading_span.style,
            base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        );
    }

    #[test]
    fn render_with_thinking_segments() {
        let theme = Theme::default();
        let content = "<think>reasoning</think>result";
        let lines = render_with_thinking(content, theme.assistant_message, &theme);
        assert!(lines.len() >= 2);
        // Thinking segment uses thinking style
        assert_eq!(lines[0][0].style, theme.thinking_message);
        // Result uses normal style
        let last = lines.last().unwrap();
        assert_eq!(last[0].style, theme.assistant_message);
    }

    #[test]
    fn render_with_thinking_streaming() {
        let theme = Theme::default();
        let content = "<think>still thinking";
        let lines = render_with_thinking(content, theme.assistant_message, &theme);
        assert!(!lines.is_empty());
        assert_eq!(lines[0][0].style, theme.thinking_message);
    }

    #[test]
    fn wrap_spans_no_wrap() {
        let spans = vec![Span::raw("short")];
        let result = wrap_spans(spans, 80);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn wrap_spans_splits() {
        let spans = vec![Span::raw("abcdef".to_string())];
        let result = wrap_spans(spans, 3);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].spans[0].content, "abc");
        assert_eq!(result[1].spans[0].content, "def");
    }
}
