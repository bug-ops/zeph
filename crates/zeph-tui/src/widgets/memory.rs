use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let mem_lines = vec![
        Line::from(format!("  SQLite: {} msgs", metrics.sqlite_message_count)),
        Line::from(format!(
            "  Qdrant: {}",
            if metrics.qdrant_available {
                "connected"
            } else {
                "---"
            }
        )),
        Line::from(format!(
            "  Conv ID: {}",
            metrics
                .sqlite_conversation_id
                .map_or_else(|| "---".to_string(), |id| id.to_string())
        )),
        Line::from(format!("  Embeddings: {}", metrics.embeddings_generated)),
    ];
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
        let mut metrics = MetricsSnapshot::default();
        metrics.sqlite_message_count = 42;
        metrics.qdrant_available = true;
        metrics.embeddings_generated = 10;

        let output = render_to_string(30, 8, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert_snapshot!(output);
    }
}
