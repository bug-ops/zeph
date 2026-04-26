// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::metrics::{MetricsSnapshot, SecurityEventCategory};
use crate::theme::Theme;

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();
    let block = Block::default()
        .title(" Security ")
        .borders(Borders::ALL)
        .style(theme.panel_border);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let all_zero = metrics.sanitizer_runs == 0
        && metrics.sanitizer_injection_flags == 0
        && metrics.sanitizer_truncations == 0
        && metrics.quarantine_invocations == 0
        && metrics.quarantine_failures == 0
        && metrics.exfiltration_images_blocked == 0
        && metrics.exfiltration_tool_urls_flagged == 0
        && metrics.exfiltration_memory_guards == 0
        && metrics.pre_execution_blocks == 0
        && metrics.pre_execution_warnings == 0
        && metrics.egress_requests_total == 0
        && metrics.egress_blocked_total == 0
        && metrics.security_events.is_empty();

    if all_zero {
        let msg = Paragraph::new("No security events.").style(theme.system_message);
        frame.render_widget(msg, inner);
        return;
    }

    let base = theme.system_message;
    let flag_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let block_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);

    let mut items = build_metric_items(metrics, base, flag_style, block_style);
    append_event_items(metrics, &mut items, base, flag_style, block_style);

    let list = List::new(items);
    frame.render_widget(list, inner);
}

/// Build a `ListItem` with a plain styled label and value, using `base` style for both.
fn plain_metric_item(
    label: &'static str,
    value: impl std::fmt::Display,
    base: Style,
) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(format!("{label}{value}"), base)))
}

/// Build a `ListItem` whose value span switches to `alert_style` when the value is non-zero.
fn styled_counter_item<'a>(
    label: &'static str,
    value: u64,
    base: Style,
    alert_style: Style,
) -> ListItem<'a> {
    ListItem::new(Line::from(vec![
        Span::styled(label, base),
        Span::styled(
            value.to_string(),
            if value > 0 { alert_style } else { base },
        ),
    ]))
}

fn build_sanitizer_items<'a>(
    metrics: &MetricsSnapshot,
    base: Style,
    flag_style: Style,
) -> Vec<ListItem<'a>> {
    vec![
        plain_metric_item("Sanitizer runs:    ", metrics.sanitizer_runs, base),
        styled_counter_item(
            "Inj flags:         ",
            metrics.sanitizer_injection_flags,
            base,
            flag_style,
        ),
        plain_metric_item("Truncations:       ", metrics.sanitizer_truncations, base),
        plain_metric_item("Quarantine calls:  ", metrics.quarantine_invocations, base),
        plain_metric_item("Quarantine fails:  ", metrics.quarantine_failures, base),
    ]
}

fn build_exfiltration_items<'a>(
    metrics: &MetricsSnapshot,
    base: Style,
    block_style: Style,
) -> Vec<ListItem<'a>> {
    vec![
        styled_counter_item(
            "Exfil images:      ",
            metrics.exfiltration_images_blocked,
            base,
            block_style,
        ),
        styled_counter_item(
            "Exfil URLs:        ",
            metrics.exfiltration_tool_urls_flagged,
            base,
            block_style,
        ),
        plain_metric_item(
            "Memory guards:     ",
            metrics.exfiltration_memory_guards,
            base,
        ),
    ]
}

fn build_pre_execution_items<'a>(
    metrics: &MetricsSnapshot,
    base: Style,
    flag_style: Style,
    block_style: Style,
) -> Vec<ListItem<'a>> {
    vec![
        styled_counter_item(
            "Verify blocks:     ",
            metrics.pre_execution_blocks,
            base,
            block_style,
        ),
        styled_counter_item(
            "Verify warnings:   ",
            metrics.pre_execution_warnings,
            base,
            flag_style,
        ),
    ]
}

fn build_egress_items<'a>(
    metrics: &MetricsSnapshot,
    base: Style,
    flag_style: Style,
    block_style: Style,
) -> Vec<ListItem<'a>> {
    vec![
        plain_metric_item("Egress requests:   ", metrics.egress_requests_total, base),
        styled_counter_item(
            "Egress blocked:    ",
            metrics.egress_blocked_total,
            base,
            block_style,
        ),
        styled_counter_item(
            "Egress dropped:    ",
            metrics.egress_dropped_total,
            base,
            flag_style,
        ),
    ]
}

fn build_metric_items<'a>(
    metrics: &MetricsSnapshot,
    base: Style,
    flag_style: Style,
    block_style: Style,
) -> Vec<ListItem<'a>> {
    let mut items = build_sanitizer_items(metrics, base, flag_style);
    items.extend(build_exfiltration_items(metrics, base, block_style));
    items.extend(build_pre_execution_items(
        metrics,
        base,
        flag_style,
        block_style,
    ));
    items.extend(build_egress_items(metrics, base, flag_style, block_style));
    items
}

fn append_event_items<'a>(
    metrics: &'a MetricsSnapshot,
    items: &mut Vec<ListItem<'a>>,
    base: Style,
    flag_style: Style,
    block_style: Style,
) {
    if metrics.security_events.is_empty() {
        return;
    }
    items.push(ListItem::new(Line::from(Span::styled(
        "Recent events:",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::UNDERLINED),
    ))));

    // Show last 5 events (most recent last).
    let start = metrics.security_events.len().saturating_sub(5);
    for ev in metrics.security_events.range(start..) {
        let (cat_str, cat_style) = match ev.category {
            SecurityEventCategory::InjectionFlag => ("[inj]  ", flag_style),
            SecurityEventCategory::InjectionBlocked => ("[injb] ", block_style),
            SecurityEventCategory::ExfiltrationBlock => ("[exfil]", block_style),
            SecurityEventCategory::Quarantine => ("[quar] ", Style::default().fg(Color::Cyan)),
            SecurityEventCategory::Truncation => ("[trunc]", Style::default().fg(Color::DarkGray)),
            SecurityEventCategory::RateLimit => ("[rlim] ", Style::default().fg(Color::Yellow)),
            SecurityEventCategory::MemoryValidation => {
                ("[mval] ", Style::default().fg(Color::Magenta))
            }
            SecurityEventCategory::PreExecutionBlock => ("[pexb] ", block_style),
            SecurityEventCategory::PreExecutionWarn => ("[pexw] ", flag_style),
            SecurityEventCategory::ResponseVerification => ("[rver] ", flag_style),
            SecurityEventCategory::CausalIpiFlag => ("[cipi] ", flag_style),
            SecurityEventCategory::CrossBoundaryMcpToAcp => {
                ("[xbnd] ", Style::default().fg(Color::Red))
            }
            SecurityEventCategory::VigilFlag => ("[vigi] ", block_style),
        };
        let hm = format_hm(ev.timestamp);
        items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{hm} "), Style::default().fg(Color::DarkGray)),
            Span::styled(cat_str, cat_style),
            Span::styled(format!(" {}", ev.source), base),
        ])));
        items.push(ListItem::new(Line::from(Span::styled(
            format!("  {}", ev.detail),
            Style::default().fg(Color::DarkGray),
        ))));
    }
}

fn format_hm(ts: u64) -> String {
    #[allow(clippy::cast_possible_wrap)]
    chrono::DateTime::from_timestamp(ts as i64, 0).map_or_else(
        || "??:??".to_owned(),
        |dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use zeph_core::metrics::{SecurityEvent, SecurityEventCategory};

    use super::*;
    use crate::test_utils::render_to_string;

    #[test]
    fn renders_no_events_message_when_all_zero() {
        let metrics = MetricsSnapshot::default();
        let output = render_to_string(40, 10, |frame, area| {
            render(&metrics, frame, area);
        });
        assert!(output.contains("No security events."));
    }

    #[test]
    fn renders_injection_flag_count() {
        let metrics = MetricsSnapshot {
            sanitizer_injection_flags: 3,
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(40, 12, |frame, area| {
            render(&metrics, frame, area);
        });
        assert!(output.contains('3'));
    }

    #[test]
    fn renders_recent_events() {
        let mut events = VecDeque::new();
        events.push_back(SecurityEvent::new(
            SecurityEventCategory::InjectionFlag,
            "web_scrape",
            "Detected pattern: ignore previous",
        ));
        let metrics = MetricsSnapshot {
            sanitizer_injection_flags: 1,
            security_events: events,
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(50, 25, |frame, area| {
            render(&metrics, frame, area);
        });
        assert!(output.contains("web_scrape") || output.contains("inj"));
    }

    #[test]
    fn renders_exfiltration_block_category() {
        let mut events = VecDeque::new();
        events.push_back(SecurityEvent::new(
            SecurityEventCategory::ExfiltrationBlock,
            "llm_output",
            "1 markdown image(s) blocked",
        ));
        let metrics = MetricsSnapshot {
            exfiltration_images_blocked: 1,
            security_events: events,
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(50, 25, |frame, area| {
            render(&metrics, frame, area);
        });
        assert!(
            output.contains("Exfil") || output.contains("exfil") || output.contains("llm_output")
        );
    }

    #[test]
    fn renders_quarantine_category() {
        let mut events = VecDeque::new();
        events.push_back(SecurityEvent::new(
            SecurityEventCategory::Quarantine,
            "web_scrape",
            "Content quarantined, facts extracted",
        ));
        let metrics = MetricsSnapshot {
            quarantine_invocations: 1,
            security_events: events,
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(50, 25, |frame, area| {
            render(&metrics, frame, area);
        });
        assert!(output.contains("quar") || output.contains("web_scrape"));
    }

    #[test]
    fn renders_only_last_5_events_when_more_exist() {
        let mut metrics = MetricsSnapshot {
            sanitizer_injection_flags: 8,
            ..MetricsSnapshot::default()
        };
        for i in 0..8u64 {
            metrics.security_events.push_back(SecurityEvent::new(
                SecurityEventCategory::InjectionFlag,
                format!("source_{i}"),
                format!("detail_{i}"),
            ));
        }
        let output = render_to_string(60, 30, |frame, area| {
            render(&metrics, frame, area);
        });
        // Last 5: sources 3..7 should appear, first 3 should not.
        assert!(output.contains("source_7"), "last event must be rendered");
        assert!(
            output.contains("source_3"),
            "5th-from-last must be rendered"
        );
        assert!(
            !output.contains("source_2"),
            "6th-from-last must NOT be rendered"
        );
    }

    #[test]
    fn renders_without_panic_on_zero_height() {
        let metrics = MetricsSnapshot::default();
        // height=0 means inner area is zero — must not panic
        render_to_string(40, 0, |frame, area| {
            render(&metrics, frame, area);
        });
    }
}
