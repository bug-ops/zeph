// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Resources side-panel widget.
//!
//! Shows a compact summary of LLM routing, session token usage, and API calls:
//! `tokens`, `api`, `route` — matching design spec §4. Extra lines (cache,
//! MCP, background shell, turn latency, classifiers) appear only when they
//! carry non-zero data, so the panel stays quiet during idle sessions.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use std::fmt::Write as _;

use ratatui::widgets::{Block, Borders, Paragraph};

use crate::layout::truncate_to_width;
use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

/// Render the resources panel into `area`.
///
/// Layout (spec §4): section title · tokens · api · route, with optional
/// cache, MCP, background-shell, turn-latency, and classifier-latency lines
/// when non-zero.
pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect, theme: &Theme) {
    let mut lines: Vec<Line<'_>> = vec![Line::from(Span::styled(
        "resources",
        theme.system_message.add_modifier(Modifier::BOLD),
    ))];

    append_tokens_line(&mut lines, metrics, theme);
    append_api_line(&mut lines, metrics, theme);
    append_route_line(&mut lines, metrics, theme);
    append_cache_line(&mut lines, metrics, theme);
    append_mcp_line(&mut lines, metrics, theme);
    append_shell_background_section(&mut lines, metrics);
    append_turn_latency_section(&mut lines, metrics);
    append_classifier_latency_line(&mut lines, metrics);

    let resources = Paragraph::new(lines).block(Block::default().borders(Borders::NONE));
    frame.render_widget(resources, area);
}

/// `tokens  Nk` (with reasoning suffix when non-zero).
fn append_tokens_line(lines: &mut Vec<Line<'_>>, metrics: &MetricsSnapshot, theme: &Theme) {
    use zeph_common::format_tokens;
    let mut detail = format_tokens(metrics.total_tokens);
    if metrics.reasoning_tokens > 0 {
        let _ = write!(detail, "  R:{}", format_tokens(metrics.reasoning_tokens));
    }
    lines.push(Line::from(vec![
        Span::styled("  tokens  ", theme.system_message),
        Span::styled(detail, theme.status_bar),
    ]));
}

/// `api  N calls [· Nms last]`.
fn append_api_line(lines: &mut Vec<Line<'_>>, metrics: &MetricsSnapshot, theme: &Theme) {
    let mut detail = format!("{} calls", metrics.api_calls);
    if metrics.last_llm_latency_ms > 0 {
        let _ = write!(detail, "  · {}ms last", metrics.last_llm_latency_ms);
    }
    lines.push(Line::from(vec![
        Span::styled("  api     ", theme.system_message),
        Span::styled(detail, theme.status_bar),
    ]));
}

/// `route  provider/model` (or just `model` when provider is empty).
fn append_route_line(lines: &mut Vec<Line<'_>>, metrics: &MetricsSnapshot, theme: &Theme) {
    if metrics.model_name.is_empty() {
        return;
    }
    let route = if metrics.provider_name.is_empty() {
        metrics.model_name.clone()
    } else {
        format!("{}/{}", metrics.provider_name, metrics.model_name)
    };
    lines.push(Line::from(vec![
        Span::styled("  route   ", theme.system_message),
        Span::styled(route, theme.status_bar),
    ]));
    if !metrics.embedding_model.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  embed   ", theme.system_message),
            Span::styled(metrics.embedding_model.clone(), theme.status_bar),
        ]));
    }
}

/// `cache  W:N R:N` — only when there is cache activity.
fn append_cache_line(lines: &mut Vec<Line<'_>>, metrics: &MetricsSnapshot, theme: &Theme) {
    if metrics.cache_creation_tokens == 0 && metrics.cache_read_tokens == 0 {
        return;
    }
    lines.push(Line::from(vec![
        Span::styled("  cache   ", theme.system_message),
        Span::styled(
            format!(
                "W:{} R:{}",
                metrics.cache_creation_tokens, metrics.cache_read_tokens
            ),
            theme.status_bar,
        ),
    ]));
}

/// `mcp  N/N connected, N tools` — only when MCP servers are configured.
fn append_mcp_line(lines: &mut Vec<Line<'_>>, metrics: &MetricsSnapshot, theme: &Theme) {
    if metrics.mcp_server_count == 0 {
        return;
    }
    lines.push(Line::from(vec![
        Span::styled("  mcp     ", theme.system_message),
        Span::styled(
            format!(
                "{}/{} connected, {} tools",
                metrics.mcp_connected_count, metrics.mcp_server_count, metrics.mcp_tool_count
            ),
            theme.status_bar,
        ),
    ]));
}

/// Background shell runs — only when present.
fn append_shell_background_section(lines: &mut Vec<Line<'_>>, metrics: &MetricsSnapshot) {
    if metrics.shell_background_runs.is_empty() {
        return;
    }
    lines.push(Line::from(format!(
        "  shell ({} bg)",
        metrics.shell_background_runs.len()
    )));
    for run in &metrics.shell_background_runs {
        let elapsed_secs = run.elapsed_secs;
        let mm = elapsed_secs / 60;
        let ss = elapsed_secs % 60;
        let cmd = truncate_to_width(&run.command, 60);
        lines.push(Line::from(format!(
            "  [{}] {:02}:{:02}  {}",
            run.run_id, mm, ss, cmd
        )));
    }
}

/// Turn latency breakdown — only when timing samples exist.
fn append_turn_latency_section(lines: &mut Vec<Line<'_>>, metrics: &MetricsSnapshot) {
    if metrics.timing_sample_count == 0 {
        return;
    }
    let last = &metrics.last_turn_timings;
    lines.push(Line::from(format!(
        "  latency  ctx:{}ms llm:{}ms tool:{}ms save:{}ms",
        last.prepare_context_ms, last.llm_chat_ms, last.tool_exec_ms, last.persist_message_ms,
    )));
}

/// Classifier p50 latency (injection/PII/feedback) — only when at least one
/// classifier has recorded a call. Full p50/p95/call-count breakdown is
/// available via the `view:latency` command (see `App::format_latency_stats`).
fn append_classifier_latency_line(lines: &mut Vec<Line<'_>>, metrics: &MetricsSnapshot) {
    let c = &metrics.classifier;
    if c.injection.call_count == 0 && c.pii.call_count == 0 && c.feedback.call_count == 0 {
        return;
    }
    let mut detail = String::new();
    for (label, task) in [("inj", &c.injection), ("pii", &c.pii), ("fb", &c.feedback)] {
        if task.call_count == 0 {
            continue;
        }
        if !detail.is_empty() {
            detail.push(' ');
        }
        // "-" for a missing sample matches the placeholder used by the detailed
        // `view:latency` breakdown (`App::format_latency_stats`).
        let p50 = task
            .p50_ms
            .map_or_else(|| "-".to_owned(), |v| format!("{v}ms"));
        let _ = write!(detail, "{label}:{p50}");
    }
    lines.push(Line::from(format!("  classify {detail}")));
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::metrics::MetricsSnapshot;
    use crate::test_utils::render_to_string;

    fn theme() -> crate::theme::Theme {
        crate::theme::Theme::default()
    }

    #[test]
    fn resources_shows_tokens_api_route() {
        let metrics = MetricsSnapshot {
            provider_name: "claude".into(),
            model_name: "opus-4".into(),
            total_tokens: 12_500,
            api_calls: 5,
            last_llm_latency_ms: 250,
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(35, 8, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(
            output.contains("tokens"),
            "must show tokens; got: {output:?}"
        );
        assert!(output.contains("api"), "must show api; got: {output:?}");
        assert!(output.contains("route"), "must show route; got: {output:?}");
        assert!(
            output.contains("claude/opus-4"),
            "route must be provider/model; got: {output:?}"
        );
        assert_snapshot!(output);
    }

    #[test]
    fn resources_shows_embedding_model_when_set() {
        let metrics = MetricsSnapshot {
            provider_name: "ollama".into(),
            model_name: "qwen3:8b".into(),
            embedding_model: "nomic-embed-text".into(),
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(35, 10, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(
            output.contains("nomic-embed-text"),
            "must show embed model; got: {output:?}"
        );
    }

    #[test]
    fn resources_omits_embedding_model_when_empty() {
        let metrics = MetricsSnapshot::default();
        let output = render_to_string(35, 8, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(
            !output.contains("embed"),
            "must not show embed when empty; got: {output:?}"
        );
    }

    #[test]
    fn resources_shows_cache_when_nonzero() {
        let metrics = MetricsSnapshot {
            cache_creation_tokens: 1000,
            cache_read_tokens: 500,
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(40, 8, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(output.contains("cache"), "must show cache; got: {output:?}");
        assert!(
            output.contains("W:1000"),
            "must show write tokens; got: {output:?}"
        );
        assert!(
            output.contains("R:500"),
            "must show read tokens; got: {output:?}"
        );
    }

    #[test]
    fn resources_omits_cache_when_zero() {
        let metrics = MetricsSnapshot::default();
        let output = render_to_string(35, 8, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(
            !output.contains("cache"),
            "must not show cache when zero; got: {output:?}"
        );
    }

    #[test]
    fn resources_shows_mcp_when_configured() {
        let metrics = MetricsSnapshot {
            mcp_server_count: 2,
            mcp_connected_count: 2,
            mcp_tool_count: 14,
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(40, 8, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(output.contains("mcp"), "must show mcp; got: {output:?}");
        assert!(
            output.contains("14 tools"),
            "must show tool count; got: {output:?}"
        );
    }

    #[test]
    fn resources_shows_background_run_when_present() {
        use zeph_core::metrics::ShellBackgroundRunRow;

        let metrics = MetricsSnapshot {
            shell_background_runs: vec![ShellBackgroundRunRow {
                run_id: "a1b2c3d4".into(),
                command: "cargo build --workspace".into(),
                elapsed_secs: 75,
            }],
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(
            output.contains("shell"),
            "must show shell header; got: {output:?}"
        );
        assert!(
            output.contains("a1b2c3d"),
            "must show run_id; got: {output:?}"
        );
        assert!(
            output.contains("01:15"),
            "must show elapsed mm:ss; got: {output:?}"
        );
    }

    #[test]
    fn resources_shows_turn_latency_breakdown_when_samples_exist() {
        use zeph_core::metrics::TurnTimings;

        let metrics = MetricsSnapshot {
            timing_sample_count: 3,
            last_turn_timings: TurnTimings {
                prepare_context_ms: 12,
                llm_chat_ms: 340,
                tool_exec_ms: 58,
                persist_message_ms: 4,
            },
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(
            output.contains("latency"),
            "must show latency header; got: {output:?}"
        );
        assert!(
            output.contains("ctx:12ms"),
            "must show context latency; got: {output:?}"
        );
        assert!(
            output.contains("tool:58ms"),
            "must show tool exec latency; got: {output:?}"
        );
        assert!(
            output.contains("save:4ms"),
            "must show persist latency; got: {output:?}"
        );
    }

    #[test]
    fn resources_omits_turn_latency_when_no_samples() {
        let metrics = MetricsSnapshot::default();
        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(
            !output.contains("latency"),
            "must not show latency when no samples; got: {output:?}"
        );
    }

    #[test]
    fn resources_shows_classifier_latency_when_calls_recorded() {
        use zeph_core::metrics::{ClassifierMetricsSnapshot, TaskMetricsSnapshot};

        let metrics = MetricsSnapshot {
            classifier: ClassifierMetricsSnapshot {
                injection: TaskMetricsSnapshot {
                    call_count: 4,
                    p50_ms: Some(7),
                    p95_ms: Some(15),
                },
                pii: TaskMetricsSnapshot::default(),
                feedback: TaskMetricsSnapshot::default(),
            },
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(
            output.contains("classify"),
            "must show classify header; got: {output:?}"
        );
        assert!(
            output.contains("inj:7ms"),
            "must show injection p50; got: {output:?}"
        );
    }

    #[test]
    fn resources_omits_classifier_latency_when_no_calls() {
        let metrics = MetricsSnapshot::default();
        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(
            !output.contains("classify"),
            "must not show classify when no calls; got: {output:?}"
        );
    }

    #[test]
    fn resources_shows_reasoning_tokens_when_nonzero() {
        let metrics = MetricsSnapshot {
            total_tokens: 10_000,
            reasoning_tokens: 2_000,
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(40, 8, |frame, area| {
            super::render(&metrics, frame, area, &theme());
        });
        assert!(
            output.contains("R:"),
            "must show reasoning label; got: {output:?}"
        );
    }
}
