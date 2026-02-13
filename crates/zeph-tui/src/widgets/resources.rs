use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let res_lines = vec![
        Line::from(format!("  Provider: {}", metrics.provider_name)),
        Line::from(format!("  Model: {}", metrics.model_name)),
        Line::from(format!("  Context: {}", metrics.context_tokens)),
        Line::from(format!("  Session: {}", metrics.total_tokens)),
        Line::from(format!("  API calls: {}", metrics.api_calls)),
        Line::from(format!("  Latency: {}ms", metrics.last_llm_latency_ms)),
    ];
    let resources = Paragraph::new(res_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border)
            .title(" Resources "),
    );
    frame.render_widget(resources, area);
}
