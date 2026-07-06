// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;
use zeph_common::format_tokens;

use crate::app::{App, InputMode};
use crate::layout::truncate_to_width;
use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;
use crate::widgets::spinner::breeze_frame;
use crate::widgets::status_verbs::humanize;

/// Priority level for a status bar segment.
///
/// Lower numeric value = higher importance. `Critical` segments are never dropped;
/// lower-priority segments are dropped LIFO (last pushed = first dropped) within
/// a priority level when the status bar width is exceeded.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Priority {
    Critical = 1,
    High = 2,
    Medium = 3,
    Low = 4,
}

struct Segment {
    spans: Vec<Span<'static>>,
    /// Abbreviated form shown under space pressure. `None` = no short form (full or drop).
    short_spans: Option<Vec<Span<'static>>>,
    priority: Priority,
    width: u16,
    /// Pre-computed display width of [`Segment::short_spans`], or 0 if absent.
    short_width: u16,
}

struct SegmentList {
    segments: Vec<Segment>,
}

impl SegmentList {
    fn new() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    fn push(&mut self, priority: Priority, spans: Vec<Span<'static>>) {
        let width: u16 = spans.iter().fold(0u16, |acc, s| {
            acc.saturating_add(u16::try_from(s.content.width()).unwrap_or(u16::MAX))
        });
        self.segments.push(Segment {
            spans,
            short_spans: None,
            priority,
            width,
            short_width: 0,
        });
    }

    /// Push a segment with an abbreviated form used under space pressure.
    ///
    /// When `total > max_width`, Phase A of [`SegmentList::layout`] abbreviates `full_spans`
    /// to `short_spans` before Phase B drops segments entirely. Critical segments with a short
    /// form are abbreviated but never dropped.
    fn push_abbrev(
        &mut self,
        priority: Priority,
        full_spans: Vec<Span<'static>>,
        short_spans: Vec<Span<'static>>,
    ) {
        let width: u16 = full_spans.iter().fold(0u16, |acc, s| {
            acc.saturating_add(u16::try_from(s.content.width()).unwrap_or(u16::MAX))
        });
        let short_width: u16 = short_spans.iter().fold(0u16, |acc, s| {
            acc.saturating_add(u16::try_from(s.content.width()).unwrap_or(u16::MAX))
        });
        self.segments.push(Segment {
            spans: full_spans,
            short_spans: Some(short_spans),
            priority,
            width,
            short_width,
        });
    }

    /// Convenience for the common single-span case.
    fn push_abbrev_styled(
        &mut self,
        priority: Priority,
        full: String,
        short: String,
        style: ratatui::style::Style,
    ) {
        self.push_abbrev(
            priority,
            vec![Span::styled(full, style)],
            vec![Span::styled(short, style)],
        );
    }

    /// Apply the three-rung pressure ladder and return a flat span list.
    ///
    /// - **Phase A** — abbreviate: while over budget, switch the worst-priority, last-pushed
    ///   segment that still shows its full form AND has a short form to its abbreviated
    ///   version. Segments where `short_width >= width` (no savings) are skipped — this
    ///   prevents an infinite loop when a short form is the same width as the full form.
    /// - **Phase B** — drop: if still over budget, drop segments LIFO by worst priority
    ///   (Critical exempt).
    /// - **Phase C** — truncate: if Critical segments alone overflow, truncate the last span.
    fn layout(mut self, max_width: u16) -> Vec<Span<'static>> {
        // Phase A — abbreviate under pressure.
        loop {
            let total: u16 = self
                .segments
                .iter()
                .fold(0u16, |a, s| a.saturating_add(s.width));
            if total <= max_width {
                break;
            }
            // Find the worst priority among segments that can be abbreviated (have a short
            // form that actually saves space — skip if short_width >= width).
            let worst = self
                .segments
                .iter()
                .filter(|s| s.short_spans.is_some() && s.short_width < s.width)
                .map(|s| s.priority)
                .max();
            let Some(worst_priority) = worst else {
                break;
            };
            // Abbreviate the last-pushed (LIFO) abbreviatable segment at that priority.
            let abbrev_idx = self
                .segments
                .iter()
                .enumerate()
                .rev()
                .find(|(_, s)| {
                    s.priority == worst_priority
                        && s.short_spans.is_some()
                        && s.short_width < s.width
                })
                .map(|(i, _)| i);
            if let Some(idx) = abbrev_idx {
                let seg = &mut self.segments[idx];
                if let Some(short) = seg.short_spans.take() {
                    seg.width = seg.short_width;
                    seg.spans = short;
                }
            } else {
                break;
            }
        }

        // Phase B — drop under pressure.
        loop {
            let total: u16 = self
                .segments
                .iter()
                .fold(0u16, |a, s| a.saturating_add(s.width));
            if total <= max_width {
                break;
            }
            // Find the worst (highest) priority among non-Critical segments.
            let worst = self
                .segments
                .iter()
                .filter(|s| s.priority != Priority::Critical)
                .map(|s| s.priority)
                .max();
            let Some(worst_priority) = worst else {
                // Only Critical segments remain — truncate the last one's spans if needed.
                break;
            };
            // Drop the last-pushed segment at that priority level (LIFO).
            let drop_idx = self
                .segments
                .iter()
                .enumerate()
                .rev()
                .find(|(_, s)| s.priority == worst_priority)
                .map(|(i, _)| i);
            if let Some(idx) = drop_idx {
                self.segments.remove(idx);
            } else {
                break;
            }
        }

        // If Critical segments still overflow, truncate the last Critical span's content.
        let total: u16 = self
            .segments
            .iter()
            .fold(0u16, |a, s| a.saturating_add(s.width));
        if total > max_width && !self.segments.is_empty() {
            let overflow = total.saturating_sub(max_width) as usize;
            if let Some(last_span) = self
                .segments
                .last_mut()
                .and_then(|seg| seg.spans.last_mut())
            {
                let chars: Vec<char> = last_span.content.chars().collect();
                let keep = chars.len().saturating_sub(overflow);
                let truncated: String = chars[..keep].iter().collect();
                last_span.content = truncated.into();
            }
        }

        self.segments
            .into_iter()
            .flat_map(Segment::into_spans)
            .collect()
    }
}

impl Segment {
    fn into_spans(self) -> Vec<Span<'static>> {
        self.spans
    }
}

/// Compact token display: `4.2k` or `4.2k(R:1.0k)` when reasoning tokens > 0.
fn compact_tokens(total: u64, reasoning: u64) -> String {
    if reasoning > 0 {
        format!("{}(R:{})", format_tokens(total), format_tokens(reasoning))
    } else {
        format_tokens(total)
    }
}

pub fn render(app: &App, metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let (left_list, right_spans) = build_segment_lists(app, metrics, theme);

    let right_width: u16 = right_spans.iter().fold(0u16, |acc, s| {
        acc.saturating_add(u16::try_from(s.content.width()).unwrap_or(u16::MAX))
    });

    let left_budget = area.width.saturating_sub(right_width);
    let left_spans = left_list.layout(left_budget);
    let left_width: u16 = left_spans.iter().fold(0u16, |acc, s| {
        acc.saturating_add(u16::try_from(s.content.width()).unwrap_or(u16::MAX))
    });

    let padding = area
        .width
        .saturating_sub(left_width)
        .saturating_sub(right_width);

    let mut all_spans = left_spans;
    if !right_spans.is_empty() {
        if padding > 0 {
            all_spans.push(Span::styled(" ".repeat(padding as usize), theme.status_bar));
        }
        all_spans.extend(right_spans);
    }

    let line = Line::from(all_spans);
    let paragraph = Paragraph::new(line).style(theme.status_bar);
    frame.render_widget(paragraph, area);
}

/// Build the left segment list and right (uptime) spans separately so the caller
/// can right-align the uptime by computing the padding between them.
fn build_segment_lists(
    app: &App,
    metrics: &MetricsSnapshot,
    theme: &Theme,
) -> (SegmentList, Vec<Span<'static>>) {
    let mode = app.input_mode();
    let mut list = SegmentList::new();

    push_mode_chip(&mut list, mode, theme);

    if app.is_agent_busy() {
        push_busy_segment(&mut list, app, theme);
    }

    push_plan_subagent_segments(&mut list, app, metrics, theme);
    push_skills_segment(&mut list, metrics, theme);
    push_tokens_segment(&mut list, metrics, theme);

    if metrics.sanitizer_injection_flags > 0 {
        list.push(
            Priority::Low,
            vec![Span::styled(
                format!(" · SEC {} ⚑", metrics.sanitizer_injection_flags),
                theme.highlight,
            )],
        );
    }

    push_api_segment(&mut list, metrics, theme);
    push_extra_low_segments(&mut list, app, metrics, theme);

    let uptime_spans = build_uptime_spans(metrics, theme);
    (list, uptime_spans)
}

fn push_mode_chip(list: &mut SegmentList, mode: InputMode, theme: &Theme) {
    let surface_bg = theme.status_bar.bg.unwrap_or(Color::Black);
    let (chip_text, chip_bg) = match mode {
        InputMode::Insert => (" INSERT ", theme.user_message.fg.unwrap_or(Color::Cyan)),
        InputMode::Normal => (" NORMAL ", theme.panel_border.fg.unwrap_or(Color::Gray)),
    };
    list.push(
        Priority::Critical,
        vec![Span::styled(
            chip_text,
            Style::default().fg(surface_bg).bg(chip_bg),
        )],
    );
}

fn push_busy_segment(list: &mut SegmentList, app: &App, theme: &Theme) {
    let phrase = humanize(app.status_label().unwrap_or("thinking"));
    let verb = if phrase.detail.is_empty() {
        phrase.verb
    } else {
        format!("{} · {}", phrase.verb, phrase.detail)
    };
    if app.motion == zeph_config::Motion::Off {
        list.push(
            Priority::High,
            vec![
                Span::styled(" · ", theme.system_message),
                Span::styled("·", theme.system_message),
                Span::styled(format!(" {verb}"), theme.system_message),
            ],
        );
    } else {
        let idx = usize::try_from(app.throbber_state().index().rem_euclid(6)).unwrap_or(0);
        let spinner_char = breeze_frame(idx as u64, app.is_ascii_only());
        list.push(
            Priority::High,
            vec![
                Span::styled(" · ", theme.system_message),
                Span::styled(spinner_char, theme.highlight),
                Span::styled(format!(" {verb}"), theme.system_message),
            ],
        );
    }
}

fn push_plan_subagent_segments(
    list: &mut SegmentList,
    app: &App,
    metrics: &MetricsSnapshot,
    theme: &Theme,
) {
    let plan_seg = plan_mode_segment(app, metrics);
    if !plan_seg.is_empty() {
        list.push(
            Priority::Medium,
            vec![Span::styled(plan_seg.to_owned(), theme.status_bar)],
        );
    }
    let subagent_seg = subagent_view_segment(app);
    if !subagent_seg.is_empty() {
        list.push(
            Priority::Medium,
            vec![Span::styled(subagent_seg, theme.status_bar)],
        );
    }
}

fn push_skills_segment(list: &mut SegmentList, metrics: &MetricsSnapshot, theme: &Theme) {
    let active = metrics.active_skills.len();
    let total = metrics.total_skills;
    list.push_abbrev(
        Priority::Medium,
        vec![
            Span::styled(" · ", theme.system_message),
            Span::styled("skills ", theme.system_message),
            Span::styled(format!("{active}/{total}"), theme.status_bar),
        ],
        vec![
            Span::styled(" Sk ", theme.system_message),
            Span::styled(format!("{active}/{total}"), theme.status_bar),
        ],
    );
}

fn push_tokens_segment(list: &mut SegmentList, metrics: &MetricsSnapshot, theme: &Theme) {
    let compact = compact_tokens(metrics.total_tokens, metrics.reasoning_tokens);
    list.push_abbrev(
        Priority::Low,
        vec![
            Span::styled(" · ", theme.system_message),
            Span::styled("tokens ", theme.system_message),
            Span::styled(compact.clone(), theme.status_bar),
        ],
        vec![Span::styled(format!(" t:{compact}"), theme.status_bar)],
    );
}

fn push_api_segment(list: &mut SegmentList, metrics: &MetricsSnapshot, theme: &Theme) {
    list.push_abbrev(
        Priority::Low,
        vec![
            Span::styled(" · ", theme.system_message),
            Span::styled("api ", theme.system_message),
            Span::styled(metrics.api_calls.to_string(), theme.status_bar),
        ],
        vec![Span::styled(
            format!(" A{}", metrics.api_calls),
            theme.status_bar,
        )],
    );
}

fn build_uptime_spans(metrics: &MetricsSnapshot, theme: &Theme) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!(" ↑ {}", format_uptime(metrics.uptime_seconds)),
        theme.status_bar,
    )]
}

#[allow(clippy::too_many_lines)]
fn push_extra_low_segments(
    list: &mut SegmentList,
    app: &App,
    metrics: &MetricsSnapshot,
    theme: &Theme,
) {
    if metrics.cost_spent_cents > 0.0 {
        list.push_abbrev_styled(
            Priority::Low,
            format!(" · ${:.4}", metrics.cost_spent_cents / 100.0),
            format!(" ${:.2}", metrics.cost_spent_cents / 100.0),
            theme.status_bar,
        );
    }
    if !metrics.shell_background_runs.is_empty() {
        list.push(
            Priority::Low,
            vec![Span::styled(
                format!(" · sh:{}", metrics.shell_background_runs.len()),
                theme.status_bar,
            )],
        );
    }
    if let Some(cocoon_seg) = build_cocoon_spans(metrics, app.show_balance(), theme) {
        list.push(Priority::Low, cocoon_seg);
    }
    if metrics.bg_enrichment_inflight > 0 || metrics.bg_telemetry_inflight > 0 {
        list.push(
            Priority::Low,
            vec![Span::styled(
                format!(
                    " · bg: {} enrich, {} telem",
                    metrics.bg_enrichment_inflight, metrics.bg_telemetry_inflight,
                ),
                theme.status_bar,
            )],
        );
    }
    if metrics.server_compaction_events > 0 {
        list.push(
            Priority::Low,
            vec![
                Span::styled(" · ", theme.system_message),
                Span::styled(
                    format!("[SC: {}]", metrics.server_compaction_events),
                    Style::default().fg(Color::Cyan),
                ),
            ],
        );
    }
    if let Some(ref snap) = metrics.active_goal {
        list.push(Priority::Low, build_goal_spans(snap, theme));
    }
    if metrics.filter_applications > 0 {
        list.push(
            Priority::Low,
            vec![Span::styled(build_filter_text(metrics), theme.status_bar)],
        );
    }
    let security_spans = build_exfil_guardrail_spans(metrics, theme);
    if !security_spans.is_empty() {
        list.push(Priority::Low, security_spans);
    }
    // Stream metrics: tok/s while streaming, TTFT after turn completes (#5104).
    // Suppressed entirely when motion=Off (master kill-switch, same as all other delights).
    if app.motion != zeph_config::Motion::Off && app.delights.stream_metrics {
        if app.is_agent_busy()
            && let Some(rate) = app.stream_rate.tokens_per_sec()
        {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let rate_int = rate as u32;
            list.push_abbrev_styled(
                Priority::Low,
                format!(" · {}", crate::delights::format_toks(rate)),
                format!(" {rate_int}t/s"),
                theme.status_bar,
            );
        }
        if let Some(ttft_ms) = app.stream_rate.ttft_ms() {
            list.push_abbrev_styled(
                Priority::Low,
                format!(" · {}", crate::delights::format_ttft(ttft_ms)),
                format!(" {ttft_ms}ms"),
                theme.status_bar,
            );
        }
    }

    if app.mouse_enabled() {
        list.push(
            Priority::Low,
            vec![Span::styled(
                " · mouse on (Shift+drag selects)",
                theme.status_bar,
            )],
        );
    }
}

fn build_cocoon_spans(
    metrics: &MetricsSnapshot,
    show_balance: bool,
    theme: &Theme,
) -> Option<Vec<Span<'static>>> {
    match metrics.cocoon_connected {
        None => None,
        Some(true) => {
            let mut text = format!(
                " · Cocoon: healthy ({} models, {} workers)",
                metrics.cocoon_model_count, metrics.cocoon_worker_count,
            );
            if let Some(balance) = metrics.cocoon_ton_balance {
                if show_balance {
                    let _ = write!(text, ", {balance:.2} TON");
                } else {
                    text.push_str(", *** TON");
                }
            }
            Some(vec![Span::styled(text, theme.status_bar)])
        }
        Some(false) => Some(vec![Span::styled(
            " · Cocoon: sidecar unreachable".to_owned(),
            theme.status_bar,
        )]),
    }
}

fn build_goal_spans(snap: &crate::metrics::GoalSnapshot, theme: &Theme) -> Vec<Span<'static>> {
    use crate::metrics::GoalStatus;
    let (icon, color) = match snap.status {
        GoalStatus::Active => ("▶", Color::Green),
        GoalStatus::Paused => ("⏸", Color::Yellow),
        GoalStatus::Completed => ("✓", Color::Cyan),
        GoalStatus::Cleared => ("✗", Color::Red),
        _ => ("?", Color::DarkGray),
    };
    let label = if snap.text.is_empty() {
        format!(" {icon} goal")
    } else {
        let truncated = truncate_to_width(&snap.text, 30);
        format!(" {icon} {truncated}")
    };
    vec![
        Span::styled(" · ", theme.system_message),
        Span::styled(label, Style::default().fg(color)),
    ]
}

/// Spans for exfiltration blocked count and guardrail status.
/// Injection flags are handled separately as `SEC N ⚑` in the main field list.
fn build_exfil_guardrail_spans(metrics: &MetricsSnapshot, theme: &Theme) -> Vec<Span<'static>> {
    let exfil_total = metrics.exfiltration_images_blocked
        + metrics.exfiltration_tool_urls_flagged
        + metrics.exfiltration_memory_guards;

    let mut spans: Vec<Span<'static>> = Vec::new();

    if exfil_total > 0 {
        spans.push(Span::styled(" · ", theme.system_message));
        spans.push(Span::styled(
            format!("{exfil_total} blocked"),
            Style::default().fg(Color::Red),
        ));
    }
    if metrics.guardrail_enabled {
        spans.push(Span::styled(" · ", theme.system_message));
        let (label, color) = if metrics.guardrail_warn_mode {
            ("GRD:warn", Color::Yellow)
        } else {
            ("GRD:on", Color::Green)
        };
        spans.push(Span::styled(label, Style::default().fg(color)));
    }

    spans
}

fn subagent_view_segment(app: &App) -> String {
    if let Some(name) = app.view_target().subagent_name() {
        format!(" · Viewing: {name}")
    } else {
        String::new()
    }
}

fn plan_mode_segment<'a>(app: &App, metrics: &MetricsSnapshot) -> &'a str {
    if metrics
        .orchestration_graph
        .as_ref()
        .is_some_and(|s| !s.is_stale())
    {
        if app.plan_view_active() {
            " · [Agents]"
        } else {
            " · [Plan]"
        }
    } else {
        ""
    }
}

#[allow(clippy::cast_precision_loss)]
fn build_filter_text(metrics: &MetricsSnapshot) -> String {
    let savings = if metrics.filter_raw_tokens > 0 {
        metrics.filter_saved_tokens as f64 / metrics.filter_raw_tokens as f64 * 100.0
    } else {
        0.0
    };
    format!(
        " · Filters: {}/{} ({savings:.0}% saved)",
        metrics.filter_filtered_commands, metrics.filter_total_commands,
    )
}

fn format_uptime(secs: u64) -> String {
    let m = secs / 60;
    let s = secs % 60;
    if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short_uptime(secs: u64) -> String {
        let m = secs / 60;
        if m > 0 {
            format!(" {m}m")
        } else {
            format!(" {secs}s")
        }
    }

    #[test]
    fn format_tokens_small() {
        assert_eq!(format_tokens(500), "500");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(format_tokens(4200), "4.2k");
    }

    #[test]
    fn format_tokens_millions() {
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn format_uptime_seconds_only() {
        assert_eq!(format_uptime(45), "45s");
    }

    #[test]
    fn format_uptime_minutes_and_seconds() {
        assert_eq!(format_uptime(135), "2m 15s");
    }

    #[test]
    fn segment_list_drops_low_before_high() {
        let theme = Theme::default();
        let mut list = SegmentList::new();
        // Critical: 10 chars
        list.push(
            Priority::Critical,
            vec![Span::styled("0123456789", theme.status_bar)],
        );
        // High: 10 chars
        list.push(
            Priority::High,
            vec![Span::styled("ABCDEFGHIJ", theme.status_bar)],
        );
        // Low: 10 chars — should be dropped first
        list.push(
            Priority::Low,
            vec![Span::styled("xxxxxxxxxx", theme.status_bar)],
        );
        // max_width = 20: Critical + High fit (20 chars), Low is dropped
        let spans = list.layout(20);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("0123456789"), "Critical must survive");
        assert!(text.contains("ABCDEFGHIJ"), "High must survive");
        assert!(!text.contains("xxxxxxxxxx"), "Low must be dropped");
    }

    #[test]
    fn segment_list_lifo_among_equal_priority() {
        let theme = Theme::default();
        let mut list = SegmentList::new();
        // Critical: 10 chars
        list.push(
            Priority::Critical,
            vec![Span::styled("0123456789", theme.status_bar)],
        );
        // Two Low segments: the last-pushed (B) should be dropped first
        list.push(
            Priority::Low,
            vec![Span::styled("AAAAAAAAAA", theme.status_bar)],
        );
        list.push(
            Priority::Low,
            vec![Span::styled("BBBBBBBBBB", theme.status_bar)],
        );
        // max_width = 20: Critical + A fit, B is dropped (LIFO)
        let spans = list.layout(20);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("0123456789"), "Critical must survive");
        assert!(text.contains("AAAAAAAAAA"), "First Low must survive");
        assert!(
            !text.contains("BBBBBBBBBB"),
            "Second Low (LIFO) must be dropped"
        );
    }

    #[test]
    fn segment_list_critical_never_dropped() {
        let theme = Theme::default();
        let mut list = SegmentList::new();
        list.push(
            Priority::Critical,
            vec![Span::styled("CRITICAL_SEGMENT_DATA", theme.status_bar)],
        );
        list.push(
            Priority::Low,
            vec![Span::styled("lowpri", theme.status_bar)],
        );
        // Extremely narrow — Low must be dropped, Critical survives (possibly truncated).
        let spans = list.layout(5);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("lowpri"),
            "Low must be dropped under pressure"
        );
    }

    #[test]
    fn status_bar_snapshot() {
        use insta::assert_snapshot;
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let app = App::new(user_tx, agent_rx);
        let metrics = MetricsSnapshot {
            total_tokens: 4200,
            api_calls: 12,
            active_skills: vec!["web".into(), "code".into()],
            total_skills: 5,
            qdrant_available: true,
            vector_backend: "qdrant".into(),
            uptime_seconds: 135,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(100, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn status_bar_shows_sec_flags_when_injection_flags_nonzero() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let app = App::new(user_tx, agent_rx);
        let metrics = MetricsSnapshot {
            sanitizer_injection_flags: 2,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(180, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert!(
            output.contains("SEC 2 ⚑"),
            "expected SEC indicator with flag count; got: {output:?}"
        );
    }

    #[test]
    fn status_bar_shows_blocked_when_exfiltration_nonzero() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let app = App::new(user_tx, agent_rx);
        let metrics = MetricsSnapshot {
            exfiltration_images_blocked: 1,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(180, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert!(
            output.contains("1 blocked"),
            "expected blocked count in status bar"
        );
    }

    #[test]
    fn status_bar_omits_channel() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let app = App::new(user_tx, agent_rx);
        let metrics = MetricsSnapshot {
            active_channel: "tui".into(),
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(180, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert!(
            !output.contains("ch:tui"),
            "channel must not appear in redesigned status bar; got: {output:?}"
        );
    }

    #[test]
    fn cocoon_segment_none_is_empty() {
        let metrics = MetricsSnapshot::default();
        let theme = Theme::default();
        assert!(build_cocoon_spans(&metrics, true, &theme).is_none());
    }

    #[test]
    fn cocoon_segment_healthy() {
        let theme = Theme::default();
        let metrics = MetricsSnapshot {
            cocoon_connected: Some(true),
            cocoon_worker_count: 12,
            cocoon_model_count: 3,
            cocoon_ton_balance: Some(42.5),
            ..MetricsSnapshot::default()
        };
        let spans = build_cocoon_spans(&metrics, true, &theme).expect("should be Some");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("healthy"), "got: {text}");
        assert!(text.contains("3 models"), "got: {text}");
        assert!(text.contains("12 workers"), "got: {text}");
        assert!(text.contains("42.50 TON"), "got: {text}");
    }

    #[test]
    fn cocoon_segment_unreachable() {
        let theme = Theme::default();
        let metrics = MetricsSnapshot {
            cocoon_connected: Some(false),
            ..MetricsSnapshot::default()
        };
        let spans = build_cocoon_spans(&metrics, true, &theme).expect("should be Some");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("unreachable"),
            "expected 'unreachable' in segment"
        );
    }

    #[test]
    fn cocoon_segment_balance_redacted_when_show_balance_false() {
        let theme = Theme::default();
        let metrics = MetricsSnapshot {
            cocoon_connected: Some(true),
            cocoon_worker_count: 4,
            cocoon_model_count: 2,
            cocoon_ton_balance: Some(99.9),
            ..MetricsSnapshot::default()
        };
        let spans = build_cocoon_spans(&metrics, false, &theme).expect("should be Some");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("*** TON"), "balance must be redacted: {text}");
        assert!(
            !text.contains("99.9"),
            "real balance must not appear: {text}"
        );
    }

    #[test]
    fn status_bar_omits_sec_when_all_zero() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let app = App::new(user_tx, agent_rx);
        let metrics = MetricsSnapshot::default();

        let output = render_to_string(180, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert!(
            !output.contains("SEC"),
            "SEC indicator must be hidden when all counters are zero"
        );
    }

    #[test]
    fn status_bar_full_width_120() {
        use insta::assert_snapshot;
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let app = App::new(user_tx, agent_rx);
        let metrics = MetricsSnapshot {
            model_name: "claude-sonnet-4-6".into(),
            context_tokens: 8_000,
            context_max_tokens: 100_000,
            total_tokens: 12_500,
            uptime_seconds: 300,
            api_calls: 7,
            active_skills: vec!["code".into()],
            total_skills: 3,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(120, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn segment_list_cjk_width_counts_columns() {
        let theme = Theme::default();
        let mut list = SegmentList::new();
        // "日本語" = 3 chars but 6 display columns
        list.push(
            Priority::Critical,
            vec![Span::styled("日本語", theme.status_bar)],
        );
        // Low filler: 10 ASCII chars — should be dropped when max_width = 6
        list.push(
            Priority::Low,
            vec![Span::styled("AAAAAAAAAA", theme.status_bar)],
        );
        let spans = list.layout(6);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        // CJK segment (width=6) must survive; Low (width=10) must be dropped
        assert!(text.contains("日本語"), "CJK must survive: {text:?}");
        assert!(
            !text.contains("AAAAAAAAAA"),
            "Low must be dropped: {text:?}"
        );
    }

    #[test]
    fn segment_list_emoji_width_counts_columns() {
        let theme = Theme::default();
        let mut list = SegmentList::new();
        // "🎉🎊" = 2 emoji, each 2 cols = 4 display columns total
        list.push(
            Priority::Critical,
            vec![Span::styled("🎉🎊", theme.status_bar)],
        );
        list.push(
            Priority::Low,
            vec![Span::styled("BBBBBBBBBB", theme.status_bar)],
        );
        let spans = list.layout(4);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("🎉🎊"), "emoji must survive: {text:?}");
        assert!(
            !text.contains("BBBBBBBBBB"),
            "Low must be dropped: {text:?}"
        );
    }

    #[test]
    fn phase_a_abbreviates_before_phase_b_drops() {
        let theme = Theme::default();
        let mut list = SegmentList::new();
        // Critical: 10 chars — never dropped or abbreviated here (no short form).
        list.push(
            Priority::Critical,
            vec![Span::styled("0123456789", theme.status_bar)],
        );
        // Low with short form: full=10, short=3 — abbreviatable.
        list.push_abbrev(
            Priority::Low,
            vec![Span::styled("AAAAAAAAAA", theme.status_bar)],
            vec![Span::styled("AAA", theme.status_bar)],
        );
        // Low without short form: 10 chars — drops in Phase B.
        list.push(
            Priority::Low,
            vec![Span::styled("BBBBBBBBBB", theme.status_bar)],
        );
        // max_width = 23: Critical(10) + short_A(3) + B(10) = 23 — Phase A abbreviates A,
        // then total = 23 which fits. B must NOT be dropped.
        let spans = list.layout(23);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("0123456789"), "Critical must survive");
        assert!(text.contains("AAA"), "abbreviated A must appear");
        assert!(
            !text.contains("AAAAAAAAAA"),
            "full A must be replaced by short form"
        );
        assert!(
            text.contains("BBBBBBBBBB"),
            "B must survive — Phase A was enough"
        );
    }

    #[test]
    fn phase_a_skips_segment_where_short_equals_full_width() {
        let theme = Theme::default();
        let mut list = SegmentList::new();
        list.push(
            Priority::Critical,
            vec![Span::styled("0123456789", theme.status_bar)],
        );
        // short_width == width — no savings, must be skipped by Phase A and dropped in Phase B.
        list.push_abbrev(
            Priority::Low,
            vec![Span::styled("AAAA", theme.status_bar)],
            vec![Span::styled("BBBB", theme.status_bar)], // same width = 4
        );
        // max_width = 10: only Critical fits. The abbrev segment has no savings so Phase A
        // skips it; Phase B drops it.
        let spans = list.layout(10);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("0123456789"), "Critical must survive");
        assert!(
            !text.contains("AAAA") && !text.contains("BBBB"),
            "no-savings abbrev segment must be dropped in Phase B"
        );
    }

    #[test]
    fn phase_a_lifo_order_among_abbreviatable_at_same_priority() {
        let theme = Theme::default();
        let mut list = SegmentList::new();
        list.push(
            Priority::Critical,
            vec![Span::styled("CRIT______", theme.status_bar)],
        ); // 10
        // Two Low abbreviatable: first pushed = A, last pushed = B.
        // Phase A should abbreviate B first (LIFO).
        list.push_abbrev(
            Priority::Low,
            vec![Span::styled("FULL_A____", theme.status_bar)], // 10
            vec![Span::styled("sA", theme.status_bar)],         // 2
        );
        list.push_abbrev(
            Priority::Low,
            vec![Span::styled("FULL_B____", theme.status_bar)], // 10
            vec![Span::styled("sB", theme.status_bar)],         // 2
        );
        // Total = 30. max_width = 22: need to save 8. Abbreviating B saves 8 (10→2). Fits.
        let spans = list.layout(22);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("CRIT______"), "Critical must survive");
        assert!(
            text.contains("FULL_A____"),
            "A must stay full — last pushed (B) abbreviated first"
        );
        assert!(text.contains("sB"), "B must be abbreviated (LIFO)");
        assert!(!text.contains("FULL_B____"), "full B must not appear");
    }

    #[test]
    fn short_uptime_seconds() {
        assert_eq!(short_uptime(45), " 45s");
    }

    #[test]
    fn short_uptime_minutes() {
        assert_eq!(short_uptime(135), " 2m");
    }

    #[test]
    fn motion_off_shows_static_busy_not_braille() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let mut app = App::new(user_tx, agent_rx);
        app.motion = zeph_config::Motion::Off;
        app.sessions.current_mut().status_label = Some("thinking".to_owned());

        let metrics = MetricsSnapshot::default();
        let output = render_to_string(200, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });

        let contains_braille = output
            .chars()
            .any(|c| ('\u{2800}'..='\u{28FF}').contains(&c));
        assert!(
            !contains_braille,
            "motion=Off must not show braille spinner; got: {output:?}"
        );
        assert!(
            output.contains("thinking"),
            "motion=Off must still show verb; got: {output:?}"
        );
    }

    /// Off-state byte-identity: motion=Off must suppress tok/s and TTFT segments
    /// even when `stream_rate` holds live data.  This is the test that would have
    /// caught the missing guard in M1.
    #[test]
    fn motion_off_suppresses_stream_metrics_segments() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let mut app = App::new(user_tx, agent_rx);
        app.motion = zeph_config::Motion::Off;
        // Inject synthetic TTFT so the render path would include it if the guard is absent.
        app.stream_rate.last_ttft_ms = Some(123);

        let metrics = MetricsSnapshot::default();
        let output = render_to_string(200, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });

        assert!(
            !output.contains("TTFT"),
            "motion=Off must suppress TTFT segment; got: {output:?}"
        );
        assert!(
            !output.contains("tok/s") && !output.contains("t/s"),
            "motion=Off must suppress tok/s segment; got: {output:?}"
        );
    }

    #[test]
    fn busy_segment_humanizes_label_with_detail() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let mut app = App::new(user_tx, agent_rx);
        app.sessions.current_mut().status_label = Some("Loading skills...".to_owned());

        let metrics = MetricsSnapshot::default();
        let output = render_to_string(200, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });

        assert!(
            output.contains("loading · skills"),
            "raw label must be humanized to 'loading · skills'; got: {output:?}"
        );
        assert!(
            !output.contains("Loading skills"),
            "raw internal label must not leak into the rendered status bar; got: {output:?}"
        );
    }

    #[test]
    fn busy_segment_unrecognized_label_passes_through() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let mut app = App::new(user_tx, agent_rx);
        app.sessions.current_mut().status_label = Some("Some unknown operation...".to_owned());

        let metrics = MetricsSnapshot::default();
        let output = render_to_string(200, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });

        assert!(
            output.contains("Some unknown operation"),
            "humanize() fallback must pass through unrecognized labels verbatim; got: {output:?}"
        );
    }

    #[test]
    fn busy_segment_uses_breeze_spinner_not_braille() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;
        use crate::widgets::spinner::BREEZE_FRAMES;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let mut app = App::new(user_tx, agent_rx);
        app.sessions.current_mut().status_label = Some("thinking...".to_owned());

        let metrics = MetricsSnapshot::default();
        let output = render_to_string(200, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });

        let contains_braille = output
            .chars()
            .any(|c| ('\u{2800}'..='\u{28FF}').contains(&c));
        assert!(
            !contains_braille,
            "the old hardcoded braille spinner must be gone; got: {output:?}"
        );
        assert!(
            BREEZE_FRAMES.iter().any(|f| output.contains(f)),
            "expected a breeze_frame() glyph in the busy segment; got: {output:?}"
        );
    }

    #[test]
    fn busy_segment_ascii_fallback_uses_ascii_breeze_frames() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;
        use crate::widgets::spinner::{BREEZE_ASCII, BREEZE_FRAMES};

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let mut app = App::new(user_tx, agent_rx);
        app.unicode_capable = false;
        app.sessions.current_mut().status_label = Some("thinking...".to_owned());

        assert!(app.is_ascii_only());

        let metrics = MetricsSnapshot::default();
        let output = render_to_string(200, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });

        assert!(
            BREEZE_ASCII.iter().any(|f| output.contains(f)),
            "expected an ASCII breeze_frame() glyph when unicode is unavailable; got: {output:?}"
        );
        assert!(
            !BREEZE_FRAMES.iter().any(|f| output.contains(f)),
            "Unicode breeze glyphs must not appear in ASCII fallback mode; got: {output:?}"
        );
    }
}
