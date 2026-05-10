// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use throbber_widgets_tui::BRAILLE_SIX;

use crate::app::{App, MessageRole, RenderCache, RenderCacheKey, content_hash};
use crate::highlight::SYNTAX_HIGHLIGHTER;
use crate::hyperlink;
use crate::theme::{SyntaxTheme, Theme};
use crate::widgets::tool_view::{ToolDensity, ToolKind, ToolStatus};

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
    let title = if let Some(ref name) = app.view_target().subagent_name().map(str::to_owned) {
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
        app.tool_density(),
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // function with many required inputs; a *Params struct would be more verbose without simplifying the call site
fn collect_message_lines_from(
    messages: &[crate::app::ChatMessage],
    truncation_info: Option<&str>,
    cache: &mut RenderCache,
    terminal_width: u16,
    wrap_width: usize,
    theme: &Theme,
    tool_expanded: bool,
    tool_density: ToolDensity,
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

    let mut prev_role: Option<MessageRole> = None;
    let groups = group_messages(messages);

    for (group_pos, group) in groups.iter().enumerate() {
        match group {
            MessageGroup::Single { idx, msg } => {
                let accent = match msg.role {
                    MessageRole::User => theme.user_message,
                    MessageRole::Assistant => theme.assistant_accent,
                    MessageRole::Tool => theme.tool_accent,
                    MessageRole::System => theme.system_message,
                };

                let role_changed = prev_role != Some(msg.role);
                if role_changed {
                    if group_pos > 0 {
                        lines.push(Line::default());
                    }
                    let role_label = match msg.role {
                        MessageRole::User => "You",
                        MessageRole::Assistant => "Agent",
                        MessageRole::Tool => "Tool",
                        MessageRole::System => "System",
                    };
                    let sep_text = format!("── {} {} ──", msg.timestamp, role_label);
                    lines.push(Line::from(Span::styled(sep_text, theme.turn_separator)));
                } else if group_pos > 0 {
                    lines.push(Line::default());
                }

                prev_role = Some(msg.role);

                let cache_key = RenderCacheKey {
                    content_hash: content_hash(&msg.content),
                    terminal_width,
                    tool_expanded,
                    tool_density,
                    show_labels,
                };

                // All messages (including streaming) use the render cache. For streaming
                // messages the cache key includes content_hash, so a new chunk causes a
                // cache miss and triggers re-render; unchanged content reuses the cached
                // result, eliminating redundant pulldown-cmark + tree-sitter work every frame.
                //
                // Note: tool messages with streaming=true use throbber_idx for the braille
                // spinner. Between content chunks the spinner freezes, but tool output
                // typically arrives in one batch, making this trade-off acceptable.
                let (msg_lines, msg_md_links) =
                    if let Some((cached_lines, cached_links)) = cache.get(*idx, &cache_key) {
                        (cached_lines.to_vec(), cached_links.to_vec())
                    } else {
                        let (rendered, extracted) = render_message_lines(
                            msg,
                            tool_expanded,
                            tool_density,
                            throbber_idx,
                            theme,
                            wrap_width,
                            show_labels,
                        );
                        cache.put(*idx, cache_key, rendered.clone(), extracted.clone());
                        (rendered, extracted)
                    };

                all_md_links.extend(msg_md_links);

                let is_user = msg.role == MessageRole::User;
                let user_bg = theme.user_message_bg;

                for mut line in msg_lines {
                    if is_user {
                        line.spans.insert(0, Span::styled("\u{258e} ", accent));
                        for span in &mut line.spans {
                            span.style = span.style.bg(user_bg);
                        }
                    } else {
                        line.spans.insert(0, Span::raw("  "));
                    }
                    lines.push(line);
                }
            }
            MessageGroup::Grouped {
                kind,
                start_idx,
                members,
            } => {
                let role_changed = prev_role != Some(MessageRole::Tool);
                if role_changed {
                    if group_pos > 0 {
                        lines.push(Line::default());
                    }
                    // invariant: group_messages guarantees len >= 2
                    let first_ts = &members[0].timestamp;
                    let sep_text = format!("── {first_ts} Tool ──");
                    lines.push(Line::from(Span::styled(sep_text, theme.turn_separator)));
                } else if group_pos > 0 {
                    lines.push(Line::default());
                }

                prev_role = Some(MessageRole::Tool);

                // XOR hash seeded with group size to prevent cancellation when two
                // members have identical content (a ^ a == 0 collapses without seed).
                let group_hash = members.iter().fold(members.len() as u64, |acc, m| {
                    acc ^ content_hash(&m.content)
                });
                let cache_key = RenderCacheKey {
                    content_hash: group_hash,
                    terminal_width,
                    tool_expanded,
                    tool_density,
                    show_labels,
                };

                let group_lines: Vec<Line<'static>> = if let Some((cached_lines, _)) =
                    cache.get(*start_idx, &cache_key)
                {
                    cached_lines.to_vec()
                } else {
                    let rendered =
                        render_grouped_tool_cell(*kind, members, tool_density, theme, wrap_width);
                    cache.put(*start_idx, cache_key, rendered.clone(), Vec::new());
                    rendered
                };

                for mut line in group_lines {
                    line.spans.insert(0, Span::raw("  "));
                    lines.push(line);
                }
            }
        }
    }
    (lines, all_md_links)
}

fn render_message_lines(
    msg: &crate::app::ChatMessage,
    tool_expanded: bool,
    tool_density: ToolDensity,
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
            tool_density,
            throbber_idx,
            theme,
            wrap_width,
            show_labels,
            &mut lines,
        );
        Vec::new()
    } else {
        render_chat_message(
            msg,
            tool_expanded,
            theme,
            wrap_width,
            show_labels,
            &mut lines,
        )
    };
    (lines, md_links)
}

fn render_chat_message(
    msg: &crate::app::ChatMessage,
    tool_expanded: bool,
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

    // Collapsible paste block: show first PASTE_COLLAPSED_LINES lines and an
    // expand hint when the message was submitted from a multiline paste and the
    // global expand toggle is off. This reuses tool_expanded deliberately —
    // 'e' means "show full content" for all collapsible blocks (paste and tool output).
    if let Some(total_lines) = msg.paste_line_count
        && !tool_expanded
        && total_lines > PASTE_COLLAPSED_LINES
    {
        let content_lines: Vec<&str> = msg.content.lines().collect();
        let visible: Vec<&str> = content_lines
            .iter()
            .take(PASTE_COLLAPSED_LINES)
            .copied()
            .collect();
        let preview = visible.join("\n");
        let (styled_lines, md_links) = render_md(&preview, base_style, theme);
        for (i, spans) in styled_lines.iter().enumerate() {
            let mut line_spans = Vec::with_capacity(spans.len() + 1);
            let pfx = if i == 0 {
                prefix.to_string()
            } else {
                indent.clone()
            };
            line_spans.push(Span::styled(pfx, base_style));
            line_spans.extend(spans.iter().cloned());
            lines.extend(wrap_spans(line_spans, wrap_width));
        }
        let hidden = total_lines - PASTE_COLLAPSED_LINES;
        let dim = Style::default().add_modifier(Modifier::DIM);
        lines.push(Line::from(Span::styled(
            format!("[... {hidden} more lines — press e to expand]"),
            dim,
        )));
        return md_links;
    }

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

/// Number of lines shown in a collapsed paste block before the expand hint.
const PASTE_COLLAPSED_LINES: usize = 3;

/// Maximum sub-list entries shown per group in Inline density.
const GROUP_INLINE_CAP: usize = 8;

/// A contiguous run of groupable tool messages folded into one visual cell,
/// or a single message rendered individually.
enum MessageGroup<'a> {
    /// A single message rendered normally.
    Single {
        idx: usize,
        msg: &'a crate::app::ChatMessage,
    },
    /// Two or more consecutive groupable tool messages of the same kind.
    Grouped {
        kind: ToolKind,
        /// Index of the first member, used as the cache slot.
        ///
        /// Orphaned cache slots at indices `start_idx+1` … `start_idx+members.len()-1`
        /// are expected and harmless — their `content_hash` XOR won't match after
        /// the next `RenderCache::shift`, so they evict naturally.
        start_idx: usize,
        members: Vec<&'a crate::app::ChatMessage>,
    },
}

/// Build a flat list of `MessageGroup` from a message slice in one linear pass.
fn group_messages(messages: &[crate::app::ChatMessage]) -> Vec<MessageGroup<'_>> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let msg = &messages[i];
        if msg.role != MessageRole::Tool || msg.streaming {
            groups.push(MessageGroup::Single { idx: i, msg });
            i += 1;
            continue;
        }
        // tool_name: None maps to ToolKind::Other which is not groupable —
        // intentionally keeping nameless tool messages as individual cells.
        let kind = ToolKind::classify(
            msg.tool_name
                .as_ref()
                .map_or("tool", zeph_common::ToolName::as_str),
        );
        if !kind.is_groupable() {
            groups.push(MessageGroup::Single { idx: i, msg });
            i += 1;
            continue;
        }
        let start_idx = i;
        let mut members: Vec<&crate::app::ChatMessage> = vec![msg];
        i += 1;
        while i < messages.len() {
            let next = &messages[i];
            if next.role != MessageRole::Tool || next.streaming {
                break;
            }
            let next_kind = ToolKind::classify(
                next.tool_name
                    .as_ref()
                    .map_or("tool", zeph_common::ToolName::as_str),
            );
            if next_kind != kind {
                break;
            }
            members.push(next);
            i += 1;
        }
        if members.len() == 1 {
            groups.push(MessageGroup::Single {
                idx: start_idx,
                msg: members[0],
            });
        } else {
            groups.push(MessageGroup::Grouped {
                kind,
                start_idx,
                members,
            });
        }
    }
    groups
}

/// Extract the primary argument string from a tool message for sub-list display.
fn extract_primary_arg(msg: &crate::app::ChatMessage, kind: ToolKind) -> String {
    let first_line = msg.content.lines().next().unwrap_or("");
    match kind {
        ToolKind::Run => first_line
            .strip_prefix("$ ")
            .unwrap_or(first_line)
            .to_string(),
        _ => first_line.to_string(),
    }
}

/// Render a grouped tool cell — summary line + optional sub-list of primary args.
fn render_grouped_tool_cell(
    kind: ToolKind,
    members: &[&crate::app::ChatMessage],
    tool_density: ToolDensity,
    theme: &Theme,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    debug_assert!(
        members.len() >= 2,
        "render_grouped_tool_cell requires at least 2 members"
    );
    let n = members.len();
    let any_failure = members.iter().any(|m| m.success == Some(false));
    let bullet_style = if any_failure {
        theme.tool_failure
    } else {
        theme.tool_success
    };
    let bullet = "\u{25cf} "; // ●

    let (verb, noun_singular, noun_plural) = match kind {
        ToolKind::Explore => ("Explored", "file", "files"),
        ToolKind::Run => ("Ran", "command", "commands"),
        ToolKind::Web => ("Fetched", "page", "pages"),
        ToolKind::Mcp => ("Called", "tool", "tools"),
        _ => ("Processed", "item", "items"),
    };
    let noun = if n == 1 { noun_singular } else { noun_plural };
    let summary_text = format!("{verb} {n} {noun}");

    let summary_spans: Vec<Span<'static>> = vec![
        Span::styled(bullet.to_string(), bullet_style),
        Span::styled(summary_text, bullet_style),
    ];

    let mut lines = wrap_spans(summary_spans, wrap_width);

    if tool_density == ToolDensity::Compact {
        return lines;
    }

    let dim = Style::default().add_modifier(Modifier::DIM);
    let cap = if tool_density == ToolDensity::Inline {
        GROUP_INLINE_CAP
    } else {
        members.len()
    };
    let visible = members.iter().take(cap);
    for m in visible {
        let arg = extract_primary_arg(m, kind);
        let span_text = format!("  {arg}");
        lines.extend(wrap_spans(vec![Span::styled(span_text, dim)], wrap_width));
    }
    if members.len() > cap {
        let remaining = members.len() - cap;
        lines.extend(wrap_spans(
            vec![Span::styled(format!("  ... and {remaining} more"), dim)],
            wrap_width,
        ));
    }

    lines
}

/// Head lines shown in Inline density before the ellipsis.
const TOOL_OUTPUT_HEAD: usize = 2;
/// Tail lines shown in Inline density after the ellipsis.
const TOOL_OUTPUT_TAIL: usize = 2;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // complex algorithm function; both suppressions justified until the function is decomposed in a future refactor
fn render_tool_message(
    msg: &crate::app::ChatMessage,
    tool_expanded: bool,
    tool_density: ToolDensity,
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

    let status = ToolStatus::from_streaming_and_success(msg.streaming, msg.success);
    let status_span = match status {
        ToolStatus::Running => {
            let symbol = BRAILLE_SIX.symbols[throbber_idx];
            Span::styled(format!("{symbol} "), theme.streaming_cursor)
        }
        ToolStatus::Success => Span::styled("\u{25cf} ", theme.tool_success),
        ToolStatus::Failure => Span::styled("\u{25cf} ", theme.tool_failure),
    };
    let cmd_spans: Vec<Span<'static>> = vec![
        status_span,
        Span::styled(format!("{name} "), dim),
        Span::styled(cmd_line.to_string(), dim),
    ];
    lines.extend(wrap_spans(cmd_spans, wrap_width));
    let indent = "  ";

    // Diff rendering for write/edit tools (always shown unless Compact without expand)
    if let Some(ref diff_data) = msg.diff_data {
        if tool_density == ToolDensity::Compact && !tool_expanded {
            // Compact: show a one-liner summary for diffs too
            let diff_lines_count = diff_data.new_content.lines().count();
            let noun = if diff_lines_count == 1 {
                "line"
            } else {
                "lines"
            };
            lines.push(Line::from(Span::styled(
                format!("{indent}-- diff ({diff_lines_count} {noun})"),
                dim,
            )));
        } else {
            let diff_lines =
                super::diff::compute_diff(&diff_data.old_content, &diff_data.new_content);
            let rendered = super::diff::render_diff_lines(&diff_lines, &diff_data.file_path, theme);
            let mut wrapped: Vec<Line<'static>> = Vec::new();
            for line in rendered {
                let mut prefixed_spans = vec![Span::styled(indent.to_string(), Style::default())];
                prefixed_spans.extend(line.spans);
                wrapped.push(Line::from(prefixed_spans));
            }
            let total_visual = wrapped.len();
            let threshold = TOOL_OUTPUT_HEAD + TOOL_OUTPUT_TAIL + 2;
            let show_all =
                tool_expanded || tool_density == ToolDensity::Block || total_visual <= threshold;
            if show_all {
                lines.extend(wrapped);
            } else {
                let head = wrapped[..TOOL_OUTPUT_HEAD].to_vec();
                let tail = wrapped[total_visual - TOOL_OUTPUT_TAIL..].to_vec();
                let hidden = total_visual - TOOL_OUTPUT_HEAD - TOOL_OUTPUT_TAIL;
                lines.extend(head);
                lines.push(Line::from(Span::styled(
                    format!("{indent}... ({hidden} lines hidden, press 'e' to expand)"),
                    dim,
                )));
                lines.extend(tail);
            }
        }
        return;
    }

    // Output lines (everything after the command header)
    if content_lines.len() > 1 {
        let output_lines = &content_lines[1..];

        if tool_density == ToolDensity::Compact && !tool_expanded {
            let line_count = output_lines.len();
            let noun = if line_count == 1 { "line" } else { "lines" };
            lines.push(Line::from(Span::styled(
                format!("{indent}-- {line_count} {noun}"),
                dim,
            )));
            return;
        }

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
            let threshold = TOOL_OUTPUT_HEAD + TOOL_OUTPUT_TAIL + 2;
            let show_all =
                tool_expanded || tool_density == ToolDensity::Block || total_visual <= threshold;

            if show_all {
                lines.extend(wrapped);
            } else {
                let head = wrapped[..TOOL_OUTPUT_HEAD].to_vec();
                let tail = wrapped[total_visual - TOOL_OUTPUT_TAIL..].to_vec();
                let hidden = total_visual - TOOL_OUTPUT_HEAD - TOOL_OUTPUT_TAIL;
                let stats_style = Style::default().fg(ratatui::style::Color::Indexed(243));
                lines.extend(head);
                let mut ellipsis_spans = vec![Span::styled(
                    format!("{indent}... ({hidden} lines hidden, press 'e' to expand)"),
                    dim,
                )];
                if let Some(ref stats) = msg.filter_stats {
                    ellipsis_spans.push(Span::styled(format!(" | {stats}"), stats_style));
                }
                lines.push(Line::from(ellipsis_spans));
                lines.extend(tail);
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

    fn make_paste_msg(content: &str, paste_line_count: Option<usize>) -> crate::app::ChatMessage {
        let mut msg = crate::app::ChatMessage::new(crate::app::MessageRole::User, content);
        msg.paste_line_count = paste_line_count;
        msg
    }

    fn lines_text(lines: &[ratatui::text::Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn paste_collapsed_shows_first_3_lines() {
        let theme = Theme::default();
        let content = "alpha\nbeta\ngamma\ndelta\nepsilon";
        let msg = make_paste_msg(content, Some(5));
        let mut lines = Vec::new();
        render_chat_message(&msg, false, &theme, 80, false, &mut lines);
        let text = lines_text(&lines);
        assert!(text.contains("alpha"), "first line must be visible");
        assert!(text.contains("beta"), "second line must be visible");
        assert!(text.contains("gamma"), "third line must be visible");
        assert!(
            !text.contains("delta"),
            "fourth line must be hidden when collapsed"
        );
        assert!(
            text.contains("more lines"),
            "expand hint must be present: {text}"
        );
    }

    #[test]
    fn paste_collapsed_no_hint_for_3_or_fewer_lines() {
        let theme = Theme::default();
        // 2 lines — no hint (nothing hidden beyond PASTE_COLLAPSED_LINES=3)
        let content_2 = "one\ntwo";
        let msg_2 = make_paste_msg(content_2, Some(2));
        let mut lines_2 = Vec::new();
        render_chat_message(&msg_2, false, &theme, 80, false, &mut lines_2);
        assert!(
            !lines_text(&lines_2).contains("more lines"),
            "no hint for 2-line paste"
        );
        // Exactly 3 lines — still no hint (collapse only triggers when > 3)
        let content_3 = "one\ntwo\nthree";
        let msg_3 = make_paste_msg(content_3, Some(3));
        let mut lines_3 = Vec::new();
        render_chat_message(&msg_3, false, &theme, 80, false, &mut lines_3);
        assert!(
            !lines_text(&lines_3).contains("more lines"),
            "no hint for 3-line paste"
        );
    }

    #[test]
    fn paste_expanded_shows_all_lines() {
        let theme = Theme::default();
        let content = "alpha\nbeta\ngamma\ndelta\nepsilon";
        let msg = make_paste_msg(content, Some(5));
        let mut lines = Vec::new();
        render_chat_message(&msg, true, &theme, 80, false, &mut lines);
        let text = lines_text(&lines);
        assert!(
            text.contains("delta"),
            "fourth line must be visible when expanded"
        );
        assert!(
            text.contains("epsilon"),
            "fifth line must be visible when expanded"
        );
        assert!(
            !text.contains("more lines"),
            "no hint when expanded: {text}"
        );
    }

    fn make_chat_msg(role: crate::app::MessageRole, content: &str) -> crate::app::ChatMessage {
        let mut msg = crate::app::ChatMessage::new(role, content);
        msg.timestamp = "14:30".to_owned();
        msg
    }

    fn make_tool_msg(content: &str) -> crate::app::ChatMessage {
        let mut msg =
            crate::app::ChatMessage::new(crate::app::MessageRole::Tool, content.to_owned());
        msg.tool_name = Some("bash".into());
        msg.success = Some(true);
        msg
    }

    #[test]
    fn turn_separator_emitted_on_role_change() {
        let theme = Theme::default();
        let messages = vec![
            make_chat_msg(crate::app::MessageRole::User, "Hello"),
            make_chat_msg(crate::app::MessageRole::Assistant, "Hi"),
        ];
        let mut cache = crate::app::RenderCache::default();
        let (lines, _) = collect_message_lines_from(
            &messages,
            None,
            &mut cache,
            80,
            76,
            &theme,
            false,
            ToolDensity::Inline,
            false,
            0,
        );
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            all_text.contains("── "),
            "turn separator must be present; got: {all_text:?}"
        );
    }

    #[test]
    fn tool_density_compact_shows_summary_only() {
        let theme = Theme::default();
        // "$ cmd\n" header + 10 output lines
        let content =
            "$ cmd\nline1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10";
        let msg = make_tool_msg(content);
        let mut lines = Vec::new();
        render_tool_message(
            &msg,
            false,
            ToolDensity::Compact,
            0,
            &theme,
            200,
            false,
            &mut lines,
        );
        let text = lines_text(&lines);
        // Compact: only the header + one-liner count, no output lines
        assert!(text.contains("10 lines"), "compact must show line count");
        assert!(
            !text.contains("line1"),
            "compact must not show output content"
        );
    }

    #[test]
    fn user_message_bg_tint_applied() {
        use ratatui::style::Color;
        let theme = Theme::default();
        let messages = vec![make_chat_msg(crate::app::MessageRole::User, "Hello world")];
        let mut cache = crate::app::RenderCache::default();
        let (lines, _) = collect_message_lines_from(
            &messages,
            None,
            &mut cache,
            80,
            76,
            &theme,
            false,
            ToolDensity::Inline,
            false,
            0,
        );
        let has_bg = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.bg == Some(Color::Rgb(20, 25, 35)));
        assert!(
            has_bg,
            "at least one span must have user_message_bg applied"
        );
    }

    #[test]
    fn tool_density_block_shows_all_lines() {
        let theme = Theme::default();
        let content = "$ cmd\nline1\nline2\nline3\nline4\nline5\nline6\nline7\nline8";
        let msg = make_tool_msg(content);
        let mut lines = Vec::new();
        render_tool_message(
            &msg,
            false,
            ToolDensity::Block,
            0,
            &theme,
            200,
            false,
            &mut lines,
        );
        let text = lines_text(&lines);
        assert!(text.contains("line8"), "block must show all output lines");
        assert!(!text.contains("hidden"), "block must not show ellipsis");
    }

    #[test]
    fn tool_density_inline_truncates_long_output() {
        let theme = Theme::default();
        // 10 output lines — threshold is HEAD+TAIL+2=6, so truncation fires
        let content = "$ cmd\nL1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nL9\nL10";
        let msg = make_tool_msg(content);
        let mut lines = Vec::new();
        render_tool_message(
            &msg,
            false,
            ToolDensity::Inline,
            0,
            &theme,
            200,
            false,
            &mut lines,
        );
        let text = lines_text(&lines);
        assert!(text.contains("L1"), "inline: head line 1 must be visible");
        assert!(text.contains("L2"), "inline: head line 2 must be visible");
        assert!(
            text.contains("hidden"),
            "inline: ellipsis must appear for long output"
        );
        assert!(text.contains("L9"), "inline: tail line must be visible");
        assert!(
            text.contains("L10"),
            "inline: last tail line must be visible"
        );
        assert!(!text.contains("L3"), "inline: middle lines must be hidden");
    }

    #[test]
    fn tool_density_inline_short_output_shows_all() {
        let theme = Theme::default();
        // 4 output lines — below threshold, show all
        let content = "$ cmd\nA\nB\nC\nD";
        let msg = make_tool_msg(content);
        let mut lines = Vec::new();
        render_tool_message(
            &msg,
            false,
            ToolDensity::Inline,
            0,
            &theme,
            200,
            false,
            &mut lines,
        );
        let text = lines_text(&lines);
        assert!(text.contains('D'), "short output: all lines visible");
        assert!(!text.contains("hidden"), "short output: no ellipsis");
    }

    #[test]
    fn tool_density_inline_at_threshold_no_ellipsis() {
        // 6 output lines = HEAD(2) + TAIL(2) + 2 = threshold; show_all must be true
        let theme = Theme::default();
        let content = "$ cmd\nL1\nL2\nL3\nL4\nL5\nL6";
        let msg = make_tool_msg(content);
        let mut lines = Vec::new();
        render_tool_message(
            &msg,
            false,
            ToolDensity::Inline,
            0,
            &theme,
            200,
            false,
            &mut lines,
        );
        let text = lines_text(&lines);
        assert!(text.contains("L6"), "all lines visible at threshold");
        assert!(!text.contains("hidden"), "no ellipsis exactly at threshold");
    }

    #[test]
    fn tool_density_inline_one_over_threshold_shows_ellipsis() {
        // 7 output lines > threshold(6): ellipsis fires, 3 hidden
        let theme = Theme::default();
        let content = "$ cmd\nL1\nL2\nL3\nL4\nL5\nL6\nL7";
        let msg = make_tool_msg(content);
        let mut lines = Vec::new();
        render_tool_message(
            &msg,
            false,
            ToolDensity::Inline,
            0,
            &theme,
            200,
            false,
            &mut lines,
        );
        let text = lines_text(&lines);
        assert!(text.contains("L1"), "head line 1 visible");
        assert!(text.contains("L2"), "head line 2 visible");
        assert!(text.contains("L6"), "tail line 6 visible");
        assert!(text.contains("L7"), "tail line 7 visible");
        assert!(
            text.contains("hidden"),
            "ellipsis present for 7-line output"
        );
        assert!(text.contains('3'), "3 lines hidden");
        assert!(!text.contains("L3"), "middle line L3 hidden");
    }

    #[test]
    fn tool_density_inline_one_output_line_no_ellipsis() {
        let theme = Theme::default();
        let content = "$ cmd\nonly line";
        let msg = make_tool_msg(content);
        let mut lines = Vec::new();
        render_tool_message(
            &msg,
            false,
            ToolDensity::Inline,
            0,
            &theme,
            200,
            false,
            &mut lines,
        );
        let text = lines_text(&lines);
        assert!(text.contains("only line"), "single line visible");
        assert!(!text.contains("hidden"), "no ellipsis for 1-line output");
    }

    #[test]
    fn tool_density_inline_no_output_no_panic() {
        // Header only ("$ cmd\n" with no following lines) — must not panic
        let theme = Theme::default();
        let content = "$ cmd\n";
        let msg = make_tool_msg(content);
        let mut lines = Vec::new();
        render_tool_message(
            &msg,
            false,
            ToolDensity::Inline,
            0,
            &theme,
            200,
            false,
            &mut lines,
        );
        // Just verify it doesn't panic and renders the header
        assert!(!lines.is_empty(), "header line must be rendered");
    }

    // ── grouping pre-pass tests ──────────────────────────────────────────────

    fn make_read_msg(path: &str) -> crate::app::ChatMessage {
        let mut msg = crate::app::ChatMessage::new(
            crate::app::MessageRole::Tool,
            format!("{path}\nfile content"),
        );
        msg.tool_name = Some("read_file".into());
        msg.success = Some(true);
        msg.timestamp = "10:00".to_owned();
        msg
    }

    fn make_shell_msg(cmd: &str) -> crate::app::ChatMessage {
        let mut msg =
            crate::app::ChatMessage::new(crate::app::MessageRole::Tool, format!("$ {cmd}\noutput"));
        msg.tool_name = Some("bash".into());
        msg.success = Some(true);
        msg.timestamp = "10:00".to_owned();
        msg
    }

    fn make_streaming_read_msg(path: &str) -> crate::app::ChatMessage {
        let mut msg = make_read_msg(path);
        msg.streaming = true;
        msg
    }

    fn make_write_msg(path: &str) -> crate::app::ChatMessage {
        let mut msg =
            crate::app::ChatMessage::new(crate::app::MessageRole::Tool, format!("{path}\ncontent"));
        msg.tool_name = Some("write_file".into());
        msg.success = Some(true);
        msg.timestamp = "10:00".to_owned();
        msg
    }

    #[test]
    fn group_messages_folds_five_reads() {
        let msgs: Vec<_> = (0..5)
            .map(|i| make_read_msg(&format!("src/f{i}.rs")))
            .collect();
        let groups = group_messages(&msgs);
        assert_eq!(
            groups.len(),
            1,
            "five read_file msgs must fold into one group"
        );
        match &groups[0] {
            MessageGroup::Grouped {
                kind,
                members,
                start_idx,
            } => {
                assert_eq!(*kind, ToolKind::Explore);
                assert_eq!(members.len(), 5);
                assert_eq!(*start_idx, 0);
            }
            MessageGroup::Single { .. } => panic!("expected Grouped"),
        }
    }

    #[test]
    fn group_messages_shell_between_reads_breaks_group() {
        let msgs = vec![
            make_read_msg("a.rs"),
            make_read_msg("b.rs"),
            make_shell_msg("ls"),
            make_read_msg("c.rs"),
        ];
        let groups = group_messages(&msgs);
        // [Grouped(read×2), Single(shell), Single(read)]
        assert_eq!(groups.len(), 3);
        match &groups[0] {
            MessageGroup::Grouped { kind, members, .. } => {
                assert_eq!(*kind, ToolKind::Explore);
                assert_eq!(members.len(), 2);
            }
            MessageGroup::Single { .. } => panic!("expected Grouped at index 0"),
        }
        assert!(matches!(groups[1], MessageGroup::Single { .. }));
        assert!(matches!(groups[2], MessageGroup::Single { .. }));
    }

    #[test]
    fn group_messages_streaming_breaks_group() {
        let msgs = vec![
            make_read_msg("a.rs"),
            make_streaming_read_msg("b.rs"),
            make_read_msg("c.rs"),
        ];
        let groups = group_messages(&msgs);
        // streaming msg can't be grouped → [Single, Single, Single]
        assert_eq!(groups.len(), 3);
        assert!(
            groups
                .iter()
                .all(|g| matches!(g, MessageGroup::Single { .. }))
        );
    }

    #[test]
    fn group_messages_single_not_grouped() {
        let msgs = vec![make_read_msg("only.rs")];
        let groups = group_messages(&msgs);
        assert_eq!(groups.len(), 1);
        assert!(matches!(groups[0], MessageGroup::Single { .. }));
    }

    #[test]
    fn group_messages_non_groupable_stays_single() {
        let msgs = vec![
            make_write_msg("a.rs"),
            make_write_msg("b.rs"),
            make_write_msg("c.rs"),
        ];
        let groups = group_messages(&msgs);
        assert_eq!(groups.len(), 3, "write_file is Edit — not groupable");
        assert!(
            groups
                .iter()
                .all(|g| matches!(g, MessageGroup::Single { .. }))
        );
    }

    #[test]
    fn group_messages_empty_slice() {
        let groups = group_messages(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn group_messages_group_at_end_of_slice() {
        let msgs = vec![
            make_chat_msg(crate::app::MessageRole::User, "hello"),
            make_read_msg("x.rs"),
            make_read_msg("y.rs"),
        ];
        let groups = group_messages(&msgs);
        assert_eq!(groups.len(), 2);
        assert!(matches!(groups[0], MessageGroup::Single { .. }));
        match &groups[1] {
            MessageGroup::Grouped { kind, members, .. } => {
                assert_eq!(*kind, ToolKind::Explore);
                assert_eq!(members.len(), 2);
            }
            MessageGroup::Single { .. } => panic!("expected Grouped at index 1"),
        }
    }

    #[test]
    fn render_grouped_cell_explore_summary() {
        let theme = Theme::default();
        let msgs: Vec<_> = (0..5)
            .map(|i| make_read_msg(&format!("src/f{i}.rs")))
            .collect();
        let refs: Vec<&crate::app::ChatMessage> = msgs.iter().collect();
        let lines =
            render_grouped_tool_cell(ToolKind::Explore, &refs, ToolDensity::Inline, &theme, 200);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("Explored"), "summary must say Explored");
        assert!(text.contains('5'), "summary must include count 5");
        assert!(text.contains("files"), "summary must say files");
    }

    #[test]
    fn render_grouped_cell_run_summary() {
        let theme = Theme::default();
        let msgs: Vec<_> = (0..3).map(|i| make_shell_msg(&format!("cmd{i}"))).collect();
        let refs: Vec<&crate::app::ChatMessage> = msgs.iter().collect();
        let lines =
            render_grouped_tool_cell(ToolKind::Run, &refs, ToolDensity::Inline, &theme, 200);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("Ran"), "summary must say Ran");
        assert!(text.contains('3'), "summary must include count 3");
        assert!(text.contains("commands"), "summary must say commands");
    }

    #[test]
    fn render_grouped_cell_compact_no_sublist() {
        let theme = Theme::default();
        let msgs: Vec<_> = (0..3).map(|i| make_read_msg(&format!("f{i}.rs"))).collect();
        let refs: Vec<&crate::app::ChatMessage> = msgs.iter().collect();
        let lines =
            render_grouped_tool_cell(ToolKind::Explore, &refs, ToolDensity::Compact, &theme, 200);
        // Compact: summary only, no sub-list items
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("Explored"), "compact: summary present");
        assert!(!text.contains("f0.rs"), "compact: no sub-list items");
    }

    #[test]
    fn render_grouped_cell_inline_caps_at_8() {
        let theme = Theme::default();
        let msgs: Vec<_> = (0..10)
            .map(|i| make_read_msg(&format!("f{i}.rs")))
            .collect();
        let refs: Vec<&crate::app::ChatMessage> = msgs.iter().collect();
        let lines =
            render_grouped_tool_cell(ToolKind::Explore, &refs, ToolDensity::Inline, &theme, 200);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("f0.rs"), "inline: first item visible");
        assert!(text.contains("f7.rs"), "inline: 8th item visible");
        assert!(
            !text.contains("f8.rs"),
            "inline: 9th item hidden beyond cap"
        );
        assert!(text.contains("more"), "inline: overflow hint shown");
    }

    #[test]
    fn collect_message_lines_groups_reads_end_to_end() {
        let theme = Theme::default();
        let messages: Vec<_> = (0..5)
            .map(|i| make_read_msg(&format!("src/f{i}.rs")))
            .collect();
        let mut cache = crate::app::RenderCache::default();
        let (lines, _) = collect_message_lines_from(
            &messages,
            None,
            &mut cache,
            80,
            76,
            &theme,
            false,
            ToolDensity::Inline,
            false,
            0,
        );
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        assert!(
            text.contains("Explored"),
            "5 read_file calls must render as grouped 'Explored N files'; got: {text:?}"
        );
        assert!(text.contains('5'), "group must mention count 5");
    }
}
