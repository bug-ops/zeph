// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use throbber_widgets_tui::{BRAILLE_SIX, Throbber, WhichUse};

use crate::app::{App, MessageRole, RenderCache, RenderCacheKey, content_hash};
use crate::highlight::SYNTAX_HIGHLIGHTER;
use crate::hyperlink;
use crate::theme::{SyntaxTheme, Theme};

/// A markdown link extracted during rendering: visible display text and target URL.
#[derive(Clone, Debug)]
pub struct MdLink {
    pub text: String,
    pub url: String,
}

/// Returns the maximum scroll offset for the rendered content.
pub fn render(app: &mut App, frame: &mut Frame, area: Rect, cache: &mut RenderCache) -> usize {
    if area.width == 0 || area.height == 0 {
        return 0;
    }

    let theme = Theme::default();
    let inner_height = area.height.saturating_sub(2) as usize;
    // 2 for block borders + 2 for accent prefix ("▎ ") added per line
    let wrap_width = area.width.saturating_sub(4) as usize;

    // Use visible_messages() to support subagent transcript view.
    let messages = app.visible_messages();
    let truncation_info = app.transcript_truncation_info();
    let title = if let Some(ref name) = app.view_target.subagent_name().map(str::to_owned) {
        format!(" Subagent: {name} ")
    } else {
        " Chat ".to_owned()
    };

    let (mut lines, all_md_links) = collect_message_lines_from(
        &messages,
        truncation_info.as_deref(),
        cache,
        area.width,
        wrap_width,
        &theme,
        app.tool_expanded(),
        app.compact_tools(),
        app.show_source_labels(),
        usize::try_from(
            app.throbber_state()
                .index()
                .rem_euclid(i8::try_from(BRAILLE_SIX.symbols.len()).unwrap_or(i8::MAX)),
        )
        .unwrap_or(0),
    );

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
                .title(title),
        )
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));

    frame.render_widget(paragraph, area);

    app.set_hyperlinks(hyperlink::collect_from_buffer_with_md_links(
        frame.buffer_mut(),
        area,
        &all_md_links,
    ));

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

    max_scroll
}

#[allow(clippy::too_many_arguments)]
fn collect_message_lines_from(
    messages: &[crate::app::ChatMessage],
    truncation_info: Option<&str>,
    cache: &mut RenderCache,
    terminal_width: u16,
    wrap_width: usize,
    theme: &Theme,
    tool_expanded: bool,
    compact_tools: bool,
    show_labels: bool,
    throbber_idx: usize,
) -> (Vec<Line<'static>>, Vec<MdLink>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut all_md_links: Vec<MdLink> = Vec::new();

    // Show truncation marker at the top when transcript was truncated (W4).
    if let Some(info) = truncation_info {
        lines.push(Line::from(Span::styled(
            format!("  {info}"),
            theme.system_message,
        )));
        lines.push(Line::default());
    }

    for (idx, msg) in messages.iter().enumerate() {
        let accent = match msg.role {
            MessageRole::User => theme.user_message,
            MessageRole::Assistant => theme.assistant_accent,
            MessageRole::Tool => theme.tool_accent,
            MessageRole::System => theme.system_message,
        };

        if idx > 0 {
            lines.push(Line::default());
        }

        let cache_key = RenderCacheKey {
            content_hash: content_hash(&msg.content),
            terminal_width,
            tool_expanded,
            compact_tools,
            show_labels,
        };

        // Streaming messages are never cached.
        let (msg_lines, msg_md_links) = if msg.streaming {
            render_message_lines(
                msg,
                tool_expanded,
                compact_tools,
                throbber_idx,
                theme,
                wrap_width,
                show_labels,
            )
        } else if let Some((cached_lines, cached_links)) = cache.get(idx, &cache_key) {
            (cached_lines.to_vec(), cached_links.to_vec())
        } else {
            let (rendered, extracted) = render_message_lines(
                msg,
                tool_expanded,
                compact_tools,
                throbber_idx,
                theme,
                wrap_width,
                show_labels,
            );
            cache.put(idx, cache_key, rendered.clone(), extracted.clone());
            (rendered, extracted)
        };

        all_md_links.extend(msg_md_links);

        let time_str = &msg.timestamp;
        for (i, mut line) in msg_lines.into_iter().enumerate() {
            if msg.role == MessageRole::User {
                line.spans.insert(0, Span::styled("\u{258e} ", accent));
            } else {
                line.spans.insert(0, Span::raw("  "));
            }
            if i == 0 {
                let content_width: usize =
                    line.spans.iter().map(|s| s.content.chars().count()).sum();
                let pad = wrap_width
                    .saturating_sub(content_width)
                    .saturating_sub(time_str.len());
                if pad > 0 {
                    line.spans.push(Span::raw(" ".repeat(pad)));
                    line.spans
                        .push(Span::styled(time_str.clone(), theme.system_message));
                }
            }
            lines.push(line);
        }
    }

    (lines, all_md_links)
}

pub fn render_activity(app: &mut App, frame: &mut Frame, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = Theme::default();

    // Primary status label (tool running, status update, etc.).
    if let Some(label) = app.status_label() {
        let label = format!(" {label}");
        let throbber = Throbber::default()
            .label(label)
            .style(theme.assistant_message)
            .throbber_style(theme.highlight)
            .throbber_set(BRAILLE_SIX)
            .use_type(WhichUse::Spin);
        frame.render_stateful_widget(throbber, area, app.throbber_state_mut());
        return;
    }

    // Fallback: show active TaskSupervisor tasks with a braille spinner.
    if let Some(task_label) = app.supervisor_activity_label() {
        let label = format!(" {task_label}");
        let throbber = Throbber::default()
            .label(label)
            .style(theme.assistant_message)
            .throbber_style(theme.highlight)
            .throbber_set(BRAILLE_SIX)
            .use_type(WhichUse::Spin);
        frame.render_stateful_widget(throbber, area, app.throbber_state_mut());
    }
}

fn render_message_lines(
    msg: &crate::app::ChatMessage,
    tool_expanded: bool,
    compact_tools: bool,
    throbber_idx: usize,
    theme: &Theme,
    wrap_width: usize,
    show_labels: bool,
) -> (Vec<Line<'static>>, Vec<MdLink>) {
    let mut lines = Vec::new();
    let md_links = if msg.role == MessageRole::Tool {
        render_tool_message(
            msg,
            tool_expanded,
            compact_tools,
            throbber_idx,
            theme,
            wrap_width,
            show_labels,
            &mut lines,
        );
        Vec::new()
    } else {
        render_chat_message(msg, theme, wrap_width, show_labels, &mut lines)
    };
    (lines, md_links)
}

fn render_chat_message(
    msg: &crate::app::ChatMessage,
    theme: &Theme,
    wrap_width: usize,
    _show_labels: bool,
    lines: &mut Vec<Line<'static>>,
) -> Vec<MdLink> {
    let base_style = match msg.role {
        MessageRole::User => theme.user_message,
        MessageRole::Assistant => theme.assistant_message,
        MessageRole::System => theme.system_message,
        MessageRole::Tool => unreachable!(),
    };
    let prefix = "";

    let indent = " ".repeat(prefix.len());
    let is_assistant = msg.role == MessageRole::Assistant;

    let (styled_lines, md_links) = if is_assistant {
        render_with_thinking(&msg.content, base_style, theme)
    } else {
        render_md(&msg.content, base_style, theme)
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
            line_spans.push(Span::styled("\u{2502}".to_string(), theme.streaming_cursor));
        }

        lines.extend(wrap_spans(line_spans, wrap_width));
    }

    if styled_lines.is_empty() {
        let mut pfx_spans = vec![Span::styled(prefix.to_string(), base_style)];
        if msg.streaming {
            pfx_spans.push(Span::styled("\u{2502}".to_string(), theme.streaming_cursor));
        }
        lines.extend(wrap_spans(pfx_spans, wrap_width));
    }

    md_links
}

fn render_scrollbar(
    frame: &mut Frame,
    area: Rect,
    inner_height: usize,
    total: usize,
    scroll: usize,
    _effective_offset: usize,
    max_scroll: usize,
) {
    let track_height = inner_height;
    if track_height == 0 {
        return;
    }
    let thumb_size = (inner_height * track_height)
        .checked_div(total)
        .unwrap_or(track_height)
        .clamp(1, track_height);
    let thumb_pos = ((track_height - thumb_size) * scroll)
        .checked_div(max_scroll)
        .unwrap_or(0);
    let track_top = area.y + 1;
    let bar_x = area.x + area.width.saturating_sub(1);
    let dim = Style::default().fg(ratatui::style::Color::DarkGray);
    for row in 0..track_height {
        let ch = if row >= thumb_pos && row < thumb_pos + thumb_size {
            "\u{2502}"
        } else {
            " "
        };
        let row_y = u16::try_from(row).unwrap_or(u16::MAX);
        frame
            .buffer_mut()
            .set_string(bar_x, track_top + row_y, ch, dim);
    }
}

const TOOL_OUTPUT_COLLAPSED_LINES: usize = 3;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_tool_message(
    msg: &crate::app::ChatMessage,
    tool_expanded: bool,
    compact_tools: bool,
    throbber_idx: usize,
    theme: &Theme,
    wrap_width: usize,
    _show_labels: bool,
    lines: &mut Vec<Line<'static>>,
) {
    let name = msg
        .tool_name
        .as_ref()
        .map_or("tool", zeph_common::ToolName::as_str);
    let content_lines: Vec<&str> = msg.content.lines().collect();
    let cmd_line = content_lines.first().copied().unwrap_or("");
    let dim = Style::default().add_modifier(Modifier::DIM);

    let status_span = if msg.streaming {
        let symbol = BRAILLE_SIX.symbols[throbber_idx];
        Span::styled(format!("{symbol} "), theme.streaming_cursor)
    } else {
        Span::styled("\u{2714} ", dim)
    };
    let cmd_spans: Vec<Span<'static>> = vec![
        status_span,
        Span::styled(format!("{name} "), dim),
        Span::styled(cmd_line.to_string(), dim),
    ];
    lines.extend(wrap_spans(cmd_spans, wrap_width));
    let indent = "  ";

    // Diff rendering for write/edit tools
    if let Some(ref diff_data) = msg.diff_data {
        let diff_lines = super::diff::compute_diff(&diff_data.old_content, &diff_data.new_content);
        let rendered = super::diff::render_diff_lines(&diff_lines, &diff_data.file_path, theme);
        let mut wrapped: Vec<Line<'static>> = Vec::new();
        for line in rendered {
            let mut prefixed_spans = vec![Span::styled(indent.to_string(), Style::default())];
            prefixed_spans.extend(line.spans);
            wrapped.push(Line::from(prefixed_spans));
        }
        let total_visual = wrapped.len();
        let show_all = tool_expanded || total_visual <= TOOL_OUTPUT_COLLAPSED_LINES;
        if show_all {
            lines.extend(wrapped);
        } else {
            lines.extend(wrapped.into_iter().take(TOOL_OUTPUT_COLLAPSED_LINES));
            let remaining = total_visual - TOOL_OUTPUT_COLLAPSED_LINES;
            let dim = Style::default().add_modifier(Modifier::DIM);
            lines.push(Line::from(Span::styled(
                format!(
                    "{indent}... ({remaining} hidden, {total_visual} total, press 'e' to expand)"
                ),
                dim,
            )));
        }
        return;
    }

    // Output lines (everything after the command)
    if content_lines.len() > 1 {
        if compact_tools {
            let line_count = content_lines.len() - 1;
            let noun = if line_count == 1 { "line" } else { "lines" };
            let summary = format!("{indent}-- {line_count} {noun}");
            lines.push(Line::from(Span::styled(
                summary,
                Style::default().add_modifier(Modifier::DIM),
            )));
        } else {
            let output_lines = &content_lines[1..];
            let has_diagnostics = tool_expanded
                && msg.kept_lines.is_some()
                && !msg.kept_lines.as_ref().unwrap().is_empty();

            let mut wrapped: Vec<Line<'static>> = Vec::new();
            if has_diagnostics {
                let kept_set: std::collections::HashSet<usize> =
                    msg.kept_lines.as_ref().unwrap().iter().copied().collect();
                for (raw_idx, line) in output_lines.iter().enumerate() {
                    let is_kept = kept_set.contains(&raw_idx);
                    let line_style = if is_kept {
                        theme.code_block
                    } else {
                        theme.code_block.add_modifier(Modifier::DIM)
                    };
                    let spans = vec![
                        Span::styled(indent.to_string(), Style::default()),
                        Span::styled((*line).to_string(), line_style),
                    ];
                    wrapped.extend(wrap_spans(spans, wrap_width));
                }
                lines.extend(wrapped);
                let legend_style = Style::default()
                    .fg(ratatui::style::Color::Indexed(243))
                    .add_modifier(Modifier::ITALIC);
                lines.push(Line::from(Span::styled(
                    format!("{indent}[filter diagnostics: highlighted = kept, dim = filtered out]"),
                    legend_style,
                )));
            } else {
                for line in output_lines {
                    let spans = vec![
                        Span::styled(indent.to_string(), Style::default()),
                        Span::styled((*line).to_string(), theme.code_block),
                    ];
                    wrapped.extend(wrap_spans(spans, wrap_width));
                }

                let total_visual = wrapped.len();
                let show_all = tool_expanded || total_visual <= TOOL_OUTPUT_COLLAPSED_LINES;

                if show_all {
                    lines.extend(wrapped);
                } else {
                    lines.extend(wrapped.into_iter().take(TOOL_OUTPUT_COLLAPSED_LINES));
                    let remaining = total_visual - TOOL_OUTPUT_COLLAPSED_LINES;
                    let dim = Style::default().add_modifier(Modifier::DIM);
                    let stats_style = Style::default().fg(ratatui::style::Color::Indexed(243));
                    let mut spans = vec![Span::styled(
                        format!(
                            "{indent}... ({remaining} hidden, {total_visual} total, press 'e' to expand)"
                        ),
                        dim,
                    )];
                    if let Some(ref stats) = msg.filter_stats {
                        spans.push(Span::styled(format!(" | {stats}"), stats_style));
                    }
                    lines.push(Line::from(spans));
                }
            }
        }
    }
}

fn render_with_thinking(
    content: &str,
    base_style: Style,
    theme: &Theme,
) -> (Vec<Vec<Span<'static>>>, Vec<MdLink>) {
    let mut all_lines = Vec::new();
    let mut md_links_buf: Vec<MdLink> = Vec::new();
    let mut remaining = content;
    let mut in_thinking = false;

    while !remaining.is_empty() {
        if in_thinking {
            if let Some(end) = remaining.find("</think>") {
                let segment = &remaining[..end];
                if !segment.trim().is_empty() {
                    let (rendered, collected) = render_md(segment, theme.thinking_message, theme);
                    all_lines.extend(rendered);
                    md_links_buf.extend(collected);
                }
                remaining = &remaining[end + "</think>".len()..];
                in_thinking = false;
            } else {
                if !remaining.trim().is_empty() {
                    let (rendered, collected) = render_md(remaining, theme.thinking_message, theme);
                    all_lines.extend(rendered);
                    md_links_buf.extend(collected);
                }
                break;
            }
        } else if let Some(start) = remaining.find("<think>") {
            let segment = &remaining[..start];
            if !segment.trim().is_empty() {
                let (rendered, collected) = render_md(segment, base_style, theme);
                all_lines.extend(rendered);
                md_links_buf.extend(collected);
            }
            remaining = &remaining[start + "<think>".len()..];
            in_thinking = true;
        } else {
            let (rendered, collected) = render_md(remaining, base_style, theme);
            all_lines.extend(rendered);
            md_links_buf.extend(collected);
            break;
        }
    }

    (all_lines, md_links_buf)
}

fn render_md(
    content: &str,
    base_style: Style,
    theme: &Theme,
) -> (Vec<Vec<Span<'static>>>, Vec<MdLink>) {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
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
    code_lang: Option<String>,
    link_url: Option<String>,
    /// Accumulated text content of the current link being parsed.
    link_text_buf: String,
    /// Collected markdown links for this render pass.
    md_links: Vec<MdLink>,
    in_table: bool,
    table_rows: Vec<Vec<String>>,
    current_cell: String,
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
            code_lang: None,
            link_url: None,
            link_text_buf: String::new(),
            md_links: Vec::new(),
            in_table: false,
            table_rows: Vec::new(),
            current_cell: String::new(),
        }
    }

    fn push_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                self.push_style(self.theme.highlight.add_modifier(Modifier::BOLD));
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
                        self.code_lang = Some(lang.to_string());
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
                self.code_lang = None;
                self.newline();
            }
            Event::Code(text) => {
                if self.link_url.is_some() {
                    self.link_text_buf.push_str(&text);
                }
                self.current
                    .push(Span::styled(text.to_string(), self.theme.code_inline));
            }
            Event::Text(text) => self.push_text_event(&text),
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
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.link_url = Some(dest_url.to_string());
                self.link_text_buf.clear();
                self.push_style(self.theme.link);
            }
            Event::End(TagEnd::Link) => self.end_link(),
            Event::Start(Tag::BlockQuote(_)) => {
                self.current.push(Span::styled(
                    "\u{2502} ".to_string(),
                    self.base_style.add_modifier(Modifier::DIM),
                ));
            }
            Event::Start(Tag::Table(_)) => {
                self.in_table = true;
                self.table_rows.clear();
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                self.table_rows.push(Vec::new());
            }
            Event::Start(Tag::TableCell) => {
                self.current_cell.clear();
            }
            Event::End(TagEnd::TableCell) => self.push_table_cell(),
            Event::End(TagEnd::Table) => {
                self.emit_table();
                self.in_table = false;
            }
            _ => {}
        }
    }

    fn end_link(&mut self) {
        if let Some(url) = self.link_url.take() {
            let text = std::mem::take(&mut self.link_text_buf);
            if !text.is_empty() {
                self.md_links.push(MdLink { text, url });
            }
        } else {
            self.link_text_buf.clear();
        }
        self.pop_style();
    }

    fn push_table_cell(&mut self) {
        let cell = self.current_cell.clone();
        if let Some(row) = self.table_rows.last_mut() {
            row.push(cell);
        }
    }

    fn push_text_event(&mut self, text: &str) {
        if self.in_table && !self.in_code_block {
            self.current_cell.push_str(text);
        } else if self.in_code_block {
            self.push_code_block_text(text);
        } else {
            if self.link_url.is_some() {
                self.link_text_buf.push_str(text);
            }
            let style = self.current_style();
            for (i, segment) in text.split('\n').enumerate() {
                if i > 0 {
                    self.newline();
                }
                if !segment.is_empty() {
                    self.current.push(Span::styled(segment.to_string(), style));
                }
            }
        }
    }

    fn emit_table(&mut self) {
        if self.table_rows.is_empty() {
            return;
        }
        let col_count = self.table_rows.iter().map(Vec::len).max().unwrap_or(0);
        if col_count == 0 {
            return;
        }

        if !self.current.is_empty() {
            self.newline();
        }

        let mut col_widths = vec![3usize; col_count];
        for row in &self.table_rows {
            for (ci, cell) in row.iter().enumerate() {
                col_widths[ci] = col_widths[ci].max(cell.chars().count());
            }
        }

        let border_style = self.theme.table_border;
        let base_style = self.base_style;

        let top = {
            let mut spans = Vec::new();
            spans.push(Span::styled("\u{250c}".to_string(), border_style));
            for (ci, &w) in col_widths.iter().enumerate() {
                spans.push(Span::styled("\u{2500}".repeat(w + 2), border_style));
                if ci + 1 < col_count {
                    spans.push(Span::styled("\u{252c}".to_string(), border_style));
                }
            }
            spans.push(Span::styled("\u{2510}".to_string(), border_style));
            spans
        };
        self.current = top;
        self.newline();

        let sep = {
            let mut spans = Vec::new();
            spans.push(Span::styled("\u{251c}".to_string(), border_style));
            for (ci, &w) in col_widths.iter().enumerate() {
                spans.push(Span::styled("\u{2500}".repeat(w + 2), border_style));
                if ci + 1 < col_count {
                    spans.push(Span::styled("\u{253c}".to_string(), border_style));
                }
            }
            spans.push(Span::styled("\u{2524}".to_string(), border_style));
            spans
        };

        let bottom = {
            let mut spans = Vec::new();
            spans.push(Span::styled("\u{2514}".to_string(), border_style));
            for (ci, &w) in col_widths.iter().enumerate() {
                spans.push(Span::styled("\u{2500}".repeat(w + 2), border_style));
                if ci + 1 < col_count {
                    spans.push(Span::styled("\u{2534}".to_string(), border_style));
                }
            }
            spans.push(Span::styled("\u{2518}".to_string(), border_style));
            spans
        };

        let rows = std::mem::take(&mut self.table_rows);
        for (ri, row) in rows.iter().enumerate() {
            let cell_style = if ri == 0 {
                base_style.add_modifier(Modifier::BOLD)
            } else {
                base_style
            };
            let mut spans = Vec::new();
            spans.push(Span::styled("\u{2502}".to_string(), border_style));
            for (ci, &w) in col_widths.iter().enumerate() {
                let text = row.get(ci).map_or("", String::as_str);
                let padded = format!(" {text:<w$} ");
                spans.push(Span::styled(padded, cell_style));
                spans.push(Span::styled("\u{2502}".to_string(), border_style));
            }
            self.current = spans;
            self.newline();

            if ri == 0 {
                self.current.clone_from(&sep);
                self.newline();
            }
        }

        self.current = bottom;
        self.newline();
    }

    fn push_code_block_text(&mut self, text: &str) {
        let syntax_theme = SyntaxTheme::default();
        let highlighted = self
            .code_lang
            .as_deref()
            .and_then(|lang| SYNTAX_HIGHLIGHTER.highlight(lang, text, &syntax_theme));

        if let Some(spans) = highlighted {
            let prefix = Span::styled("  ".to_string(), self.theme.code_block);
            self.current.push(prefix.clone());
            for span in spans {
                let parts: Vec<&str> = span.content.split('\n').collect();
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        self.newline();
                        self.current.push(prefix.clone());
                    }
                    if !part.is_empty() {
                        self.current
                            .push(Span::styled((*part).to_string(), span.style));
                    }
                }
            }
        } else {
            let style = self.theme.code_block;
            for (i, segment) in text.split('\n').enumerate() {
                if i > 0 {
                    self.newline();
                }
                self.current
                    .push(Span::styled(format!("  {segment}"), style));
            }
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

    fn finish(mut self) -> (Vec<Vec<Span<'static>>>, Vec<MdLink>) {
        if !self.current.is_empty() {
            self.newline();
        }
        // Remove trailing empty lines
        while self.lines.last().is_some_and(Vec::is_empty) {
            self.lines.pop();
        }
        (self.lines, self.md_links)
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
        let (rendered_lines, link_refs) = render_md("hello world", theme.assistant_message, &theme);
        assert_eq!(rendered_lines.len(), 1);
        assert_eq!(rendered_lines[0][0].content, "hello world");
        assert!(link_refs.is_empty());
    }

    #[test]
    fn render_md_bold() {
        let theme = Theme::default();
        let base = theme.assistant_message;
        let (lines, _) = render_md("say **hello** now", base, &theme);
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
        let (lines, _) = render_md("use `foo` here", theme.assistant_message, &theme);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][1].content, "foo");
        assert_eq!(lines[0][1].style, theme.code_inline);
    }

    #[test]
    fn render_md_code_block() {
        let theme = Theme::default();
        let (lines, _) = render_md("```rust\nlet x = 1;\n```", theme.assistant_message, &theme);
        assert!(lines.len() >= 2);
        // Language tag line
        assert!(lines[0][0].content.contains("rust"));
        // Code content — with syntax highlighting, spans are split by token
        let code_line = &lines[1];
        let full_text: String = code_line.iter().map(|s| s.content.as_ref()).collect();
        assert!(full_text.contains("let x = 1"));
    }

    #[test]
    fn render_md_list() {
        let theme = Theme::default();
        let (lines, _) = render_md("- first\n- second", theme.assistant_message, &theme);
        assert!(lines.len() >= 2);
        assert!(lines[0].iter().any(|s| s.content.contains('\u{2022}')));
    }

    #[test]
    fn render_md_heading() {
        let theme = Theme::default();
        let base = theme.assistant_message;
        let (lines, _) = render_md("# Title", base, &theme);
        assert!(!lines.is_empty());
        let heading_span = &lines[0][0];
        assert_eq!(heading_span.content, "Title");
        assert_eq!(
            heading_span.style,
            theme.highlight.add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn render_md_link_single() {
        let theme = Theme::default();
        let (rendered_lines, link_refs) =
            render_md("[click](https://x.com)", theme.assistant_message, &theme);
        assert!(!rendered_lines.is_empty());
        assert_eq!(link_refs.len(), 1);
        assert_eq!(link_refs[0].text, "click");
        assert_eq!(link_refs[0].url, "https://x.com");
    }

    #[test]
    fn render_md_link_bold_text() {
        let theme = Theme::default();
        let (rendered_lines, link_refs) =
            render_md("[**bold**](https://x.com)", theme.assistant_message, &theme);
        assert!(!rendered_lines.is_empty());
        assert_eq!(link_refs.len(), 1);
        assert_eq!(link_refs[0].text, "bold");
        assert_eq!(link_refs[0].url, "https://x.com");
    }

    #[test]
    fn render_md_link_no_links() {
        let theme = Theme::default();
        let (_, links) = render_md("no links here", theme.assistant_message, &theme);
        assert!(links.is_empty());
    }

    #[test]
    fn render_md_link_multiple() {
        let theme = Theme::default();
        let (_, links) = render_md(
            "[a](https://url1.com) and [b](https://url2.com)",
            theme.assistant_message,
            &theme,
        );
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].text, "a");
        assert_eq!(links[0].url, "https://url1.com");
        assert_eq!(links[1].text, "b");
        assert_eq!(links[1].url, "https://url2.com");
    }

    #[test]
    fn render_md_link_empty_text() {
        // [](url) — empty display text should produce no MdLink entry.
        let theme = Theme::default();
        let (_, links) = render_md("[](https://x.com)", theme.assistant_message, &theme);
        assert!(links.is_empty());
    }

    #[test]
    fn render_with_thinking_segments() {
        let theme = Theme::default();
        let content = "<think>reasoning</think>result";
        let (lines, _) = render_with_thinking(content, theme.assistant_message, &theme);
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
        let (lines, _) = render_with_thinking(content, theme.assistant_message, &theme);
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

    #[test]
    fn render_md_table_basic() {
        let theme = Theme::default();
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let (lines, _) = render_md(md, theme.assistant_message, &theme);
        // top border + header + separator + data row + bottom border = 5 lines
        assert_eq!(lines.len(), 5);
        let top: String = lines[0].iter().map(|s| s.content.as_ref()).collect();
        assert!(top.starts_with('\u{250c}'));
        assert!(top.ends_with('\u{2510}'));
        let sep: String = lines[2].iter().map(|s| s.content.as_ref()).collect();
        assert!(sep.starts_with('\u{251c}'));
        let bottom: String = lines[4].iter().map(|s| s.content.as_ref()).collect();
        assert!(bottom.starts_with('\u{2514}'));
        assert!(bottom.ends_with('\u{2518}'));
    }

    #[test]
    fn render_md_table_header_bold() {
        let theme = Theme::default();
        let md = "| Col |\n|-----|\n| val |";
        let (lines, _) = render_md(md, theme.assistant_message, &theme);
        // header row is lines[1]; cell text span should be bold
        let header_line = &lines[1];
        let cell_span = header_line
            .iter()
            .find(|s| s.content.contains("Col"))
            .expect("Col span not found");
        assert!(cell_span.style.add_modifier == Modifier::BOLD);
    }

    #[test]
    fn render_md_table_header_only() {
        let theme = Theme::default();
        let md = "| X | Y |\n|---|---|";
        let (lines, _) = render_md(md, theme.assistant_message, &theme);
        // top + header + separator + bottom = 4 lines (no data rows)
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn render_md_table_single_column() {
        let theme = Theme::default();
        let md = "| Name |\n|------|\n| Alice |\n| Bob |";
        let (lines, _) = render_md(md, theme.assistant_message, &theme);
        // top + header + sep + 2 data rows + bottom = 6 lines
        assert_eq!(lines.len(), 6);
        let top: String = lines[0].iter().map(|s| s.content.as_ref()).collect();
        // single column: top border has no ┬ in the middle
        assert!(!top.contains('\u{252c}'));
        assert!(top.starts_with('\u{250c}'));
        assert!(top.ends_with('\u{2510}'));
        // verify data row contains cell text
        let row1: String = lines[3].iter().map(|s| s.content.as_ref()).collect();
        assert!(row1.contains("Alice"));
    }

    #[test]
    fn render_md_table_many_columns() {
        let theme = Theme::default();
        let md = "| A | B | C | D | E |\n|---|---|---|---|---|\n| 1 | 2 | 3 | 4 | 5 |";
        let (lines, _) = render_md(md, theme.assistant_message, &theme);
        // top + header + sep + data + bottom = 5 lines
        assert_eq!(lines.len(), 5);
        let header: String = lines[1].iter().map(|s| s.content.as_ref()).collect();
        assert!(header.contains('A'));
        assert!(header.contains('E'));
        let data: String = lines[3].iter().map(|s| s.content.as_ref()).collect();
        assert!(data.contains('1'));
        assert!(data.contains('5'));
    }

    #[test]
    fn render_md_table_column_width_alignment() {
        // Cells with varying widths — column should expand to widest cell
        let theme = Theme::default();
        let md = "| Short | LongerHeader |\n|-------|--------|\n| x | y |";
        let (lines, _) = render_md(md, theme.assistant_message, &theme);
        assert_eq!(lines.len(), 5);
        // Header row: "LongerHeader" cell must appear in full
        let header: String = lines[1].iter().map(|s| s.content.as_ref()).collect();
        assert!(header.contains("LongerHeader"));
        // Data row: cell content must be padded to same column width
        let data: String = lines[3].iter().map(|s| s.content.as_ref()).collect();
        // "x" cell is padded to width of "Short" (5 chars)
        assert!(data.contains(" x     ") || data.contains(" x    "));
    }

    #[test]
    fn render_md_table_empty_data_cells() {
        // Row with missing cells — should not panic, missing cells render as empty
        let theme = Theme::default();
        let md = "| A | B | C |\n|---|---|---|\n| 1 |   |   |";
        let (lines, _) = render_md(md, theme.assistant_message, &theme);
        assert_eq!(lines.len(), 5);
        let data: String = lines[3].iter().map(|s| s.content.as_ref()).collect();
        assert!(data.contains('1'));
    }
}
