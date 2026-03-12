// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::metrics::MetricsSnapshot;
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

    use crate::metrics::MetricsSnapshot;
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
}
