// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, InputMode};
use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

#[allow(clippy::too_many_lines)]
pub fn render(app: &App, metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let mode = match app.input_mode() {
        InputMode::Normal => "Normal",
        InputMode::Insert => "Insert",
    };

    let uptime = format_uptime(metrics.uptime_seconds);

    let plan_mode_segment = plan_mode_segment(app, metrics);
    let subagent_view_segment = subagent_view_segment(app);
    let cancel_hint = if app.is_agent_busy() && app.input_mode() == InputMode::Normal {
        " | [Esc to cancel]"
    } else {
        ""
    };

    let qdrant_segment = if metrics.qdrant_available {
        format!(" | {}: OK", metrics.vector_backend)
    } else {
        String::new()
    };

    let filter_segment = build_filter_segment(metrics);

    let channel_segment = if metrics.active_channel.is_empty() {
        String::new()
    } else {
        format!(" | ch:{}", metrics.active_channel)
    };

    let bg_segment = build_bg_segment(metrics);

    let main_text = format!(
        " [{mode}]{model}{channel_segment}{plan_mode_segment}{subagent_view_segment} | Skills: {active} active / {total} loaded | Tokens: {tok}{qdrant_segment}{filter_segment}{bg_segment}",
        model = if metrics.model_name.is_empty() {
            String::new()
        } else {
            format!(" | {}", metrics.model_name)
        },
        active = metrics.active_skills.len(),
        total = metrics.total_skills,
        tok = format_tokens(metrics.total_tokens),
    );

    let mut spans: Vec<Span<'_>> = vec![Span::styled(main_text, theme.status_bar)];
    append_security_spans(&mut spans, metrics, &theme);

    if metrics.server_compaction_events > 0 {
        spans.push(Span::styled(" | ", theme.status_bar));
        spans.push(Span::styled(
            format!("[SC: {}]", metrics.server_compaction_events),
            Style::default().fg(Color::Cyan),
        ));
    }

    let suffix = format!(
        " | API: {api} | {uptime}{cancel_hint}",
        api = metrics.api_calls
    );
    spans.push(Span::styled(suffix, theme.status_bar));

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line).style(theme.status_bar);
    frame.render_widget(paragraph, area);
}

fn subagent_view_segment(app: &App) -> String {
    if let Some(name) = app.view_target().subagent_name() {
        format!(" | Viewing: {name}")
    } else {
        String::new()
    }
}

fn plan_mode_segment<'a>(app: &App, metrics: &MetricsSnapshot) -> &'a str {
    // MF3: show current side-panel mode when a plan graph is active.
    if metrics
        .orchestration_graph
        .as_ref()
        .is_some_and(|s| !s.is_stale())
    {
        if app.plan_view_active() {
            " | [Agents]"
        } else {
            " | [Plan]"
        }
    } else {
        ""
    }
}

#[allow(clippy::cast_precision_loss)]
fn build_filter_segment(metrics: &MetricsSnapshot) -> String {
    if metrics.filter_applications > 0 {
        let savings = if metrics.filter_raw_tokens > 0 {
            metrics.filter_saved_tokens as f64 / metrics.filter_raw_tokens as f64 * 100.0
        } else {
            0.0
        };
        format!(
            " | Filters: {}/{} ({savings:.0}% saved)",
            metrics.filter_filtered_commands, metrics.filter_total_commands,
        )
    } else {
        String::new()
    }
}

fn build_bg_segment(metrics: &MetricsSnapshot) -> String {
    let e = metrics.bg_enrichment_inflight;
    let t = metrics.bg_telemetry_inflight;
    if e > 0 || t > 0 {
        format!(" | bg: {e} enrich, {t} telem")
    } else {
        String::new()
    }
}

fn append_security_spans(spans: &mut Vec<Span<'_>>, metrics: &MetricsSnapshot, theme: &Theme) {
    let injection_flags = metrics.sanitizer_injection_flags;
    let exfil_total = metrics.exfiltration_images_blocked
        + metrics.exfiltration_tool_urls_flagged
        + metrics.exfiltration_memory_guards;

    if injection_flags > 0 || exfil_total > 0 {
        spans.push(Span::styled(" | ", theme.status_bar));
        if injection_flags > 0 {
            spans.push(Span::styled(
                format!("SEC: {injection_flags} flags"),
                Style::default().fg(Color::Yellow),
            ));
        }
        if exfil_total > 0 {
            if injection_flags > 0 {
                spans.push(Span::styled(" ", theme.status_bar));
            }
            spans.push(Span::styled(
                format!("{exfil_total} blocked"),
                Style::default().fg(Color::Red),
            ));
        }
    }
    if metrics.guardrail_enabled {
        spans.push(Span::styled(" | ", theme.status_bar));
        let (label, color) = if metrics.guardrail_warn_mode {
            ("GRD:warn", Color::Yellow)
        } else {
            ("GRD:on", Color::Green)
        };
        spans.push(Span::styled(label, Style::default().fg(color)));
    }
}

#[allow(clippy::cast_precision_loss)]
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
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

        let output = render_to_string(120, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert!(
            output.contains("SEC: 2 flags"),
            "expected SEC indicator with flag count"
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

        let output = render_to_string(120, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert!(
            output.contains("1 blocked"),
            "expected blocked count in status bar"
        );
    }

    #[test]
    fn status_bar_shows_channel_when_active_channel_set() {
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

        let output = render_to_string(120, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert!(
            output.contains("ch:tui"),
            "expected ch:tui in status bar; got: {output:?}"
        );
    }

    #[test]
    fn status_bar_shows_model_name_when_set() {
        use tokio::sync::mpsc;

        use crate::app::App;
        use crate::metrics::MetricsSnapshot;
        use crate::test_utils::render_to_string;

        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        let app = App::new(user_tx, agent_rx);
        let metrics = MetricsSnapshot {
            model_name: "claude-sonnet-4-6".into(),
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(140, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert!(
            output.contains("claude-sonnet-4-6"),
            "expected model name in status bar; got: {output:?}"
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

        let output = render_to_string(120, 1, |frame, area| {
            super::render(&app, &metrics, frame, area);
        });
        assert!(
            !output.contains("SEC:"),
            "SEC indicator must be hidden when all counters are zero"
        );
    }
}
