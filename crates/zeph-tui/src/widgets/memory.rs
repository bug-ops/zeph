// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::metrics::{MetricsSnapshot, ProbeVerdict};
use crate::theme::Theme;

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let mut mem_lines = vec![Line::from(format!(
        "  SQLite: {} msgs",
        metrics.sqlite_message_count
    ))];
    if metrics.qdrant_available {
        mem_lines.push(Line::from(format!(
            "  Vector: {} (connected)",
            metrics.vector_backend
        )));
    } else if !metrics.vector_backend.is_empty() {
        mem_lines.push(Line::from(format!(
            "  Vector: {} (offline)",
            metrics.vector_backend
        )));
    }
    mem_lines.push(Line::from(format!(
        "  Conv ID: {}",
        metrics
            .sqlite_conversation_id
            .map_or_else(|| "---".to_string(), |id| id.to_string())
    )));
    mem_lines.push(Line::from(format!(
        "  Embeddings: {}",
        metrics.embeddings_generated
    )));
    {
        mem_lines.push(Line::from(format!(
            "  Graph: {} entities, {} edges, {} communities",
            metrics.graph_entities_total,
            metrics.graph_edges_total,
            metrics.graph_communities_total,
        )));
        mem_lines.push(Line::from(format!(
            "  Graph extractions: {} ok, {} failed",
            metrics.graph_extraction_count, metrics.graph_extraction_failures,
        )));
    }
    let total_probes = metrics.compaction_probe_passes
        + metrics.compaction_probe_soft_failures
        + metrics.compaction_probe_failures
        + metrics.compaction_probe_errors;

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    if total_probes > 0 {
        let pct = |n: u64| -> u64 { (n as f64 / total_probes as f64 * 100.0).round() as u64 };
        let p = pct(metrics.compaction_probe_passes);
        let s = pct(metrics.compaction_probe_soft_failures);
        let h = pct(metrics.compaction_probe_failures);
        let e = pct(metrics.compaction_probe_errors);
        mem_lines.push(Line::from(format!("  Probe: P {p}% S {s}% H {h}% E {e}%")));

        if let Some(verdict) = &metrics.last_probe_verdict {
            let (label, color) = match verdict {
                ProbeVerdict::Pass => ("Pass", Color::Green),
                ProbeVerdict::SoftFail => ("SoftFail", Color::Yellow),
                ProbeVerdict::HardFail => ("HardFail", Color::Red),
                ProbeVerdict::Error => ("Error", Color::Gray),
            };
            let score_str = metrics
                .last_probe_score
                .map_or_else(String::new, |sc| format!(" ({sc:.2})"));
            mem_lines.push(Line::from(vec![
                Span::raw("  Last: "),
                Span::styled(format!("{label}{score_str}"), Style::default().fg(color)),
            ]));
        }
    }

    if metrics.semantic_fact_count > 0 {
        mem_lines.push(Line::from(format!(
            "  Semantic facts: {}",
            metrics.semantic_fact_count,
        )));
    }
    if metrics.guidelines_version > 0 {
        mem_lines.push(Line::from(format!(
            "  Guidelines: v{} ({})",
            metrics.guidelines_version, metrics.guidelines_updated_at,
        )));
    }
    let memory = Paragraph::new(mem_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border)
            .title(" Memory "),
    );
    frame.render_widget(memory, area);
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::metrics::{MetricsSnapshot, ProbeVerdict};
    use crate::test_utils::render_to_string;

    #[test]
    fn memory_with_stats() {
        let metrics = MetricsSnapshot {
            sqlite_message_count: 42,
            qdrant_available: true,
            vector_backend: "qdrant".into(),
            embeddings_generated: 10,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(30, 8, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn memory_with_guidelines() {
        let metrics = MetricsSnapshot {
            sqlite_message_count: 10,
            embeddings_generated: 0,
            guidelines_version: 3,
            guidelines_updated_at: "2026-03-15T17:00:00.000Z".into(),
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn probe_lines_visible_when_probes_ran() {
        let metrics = MetricsSnapshot {
            sqlite_message_count: 5,
            compaction_probe_passes: 87,
            compaction_probe_soft_failures: 10,
            compaction_probe_failures: 2,
            compaction_probe_errors: 1,
            last_probe_verdict: Some(ProbeVerdict::Pass),
            last_probe_score: Some(0.91),
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(50, 12, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn probe_lines_hidden_when_no_probes() {
        let metrics = MetricsSnapshot {
            sqlite_message_count: 5,
            compaction_probe_passes: 0,
            compaction_probe_soft_failures: 0,
            compaction_probe_failures: 0,
            compaction_probe_errors: 0,
            last_probe_verdict: None,
            last_probe_score: None,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn probe_error_verdict_shows_no_score() {
        let metrics = MetricsSnapshot {
            sqlite_message_count: 5,
            compaction_probe_passes: 1,
            compaction_probe_errors: 1,
            last_probe_verdict: Some(ProbeVerdict::Error),
            last_probe_score: None,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(50, 12, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert_snapshot!(output);
    }
}
