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
