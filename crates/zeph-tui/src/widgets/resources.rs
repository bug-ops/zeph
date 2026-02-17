use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let mut res_lines = vec![
        Line::from(format!("  Provider: {}", metrics.provider_name)),
        Line::from(format!("  Model: {}", metrics.model_name)),
        Line::from(format!("  Context: {}", metrics.context_tokens)),
        Line::from(format!("  Session: {}", metrics.total_tokens)),
        Line::from(format!("  API calls: {}", metrics.api_calls)),
        Line::from(format!("  Latency: {}ms", metrics.last_llm_latency_ms)),
    ];
    if metrics.cache_creation_tokens > 0 || metrics.cache_read_tokens > 0 {
        res_lines.push(Line::from(format!(
            "  Cache write: {}",
            metrics.cache_creation_tokens
        )));
        res_lines.push(Line::from(format!(
            "  Cache read: {}",
            metrics.cache_read_tokens
        )));
    }
    if metrics.filter_applications > 0 {
        #[allow(clippy::cast_precision_loss)]
        let pct = if metrics.filter_raw_tokens > 0 {
            metrics.filter_saved_tokens as f64 / metrics.filter_raw_tokens as f64 * 100.0
        } else {
            0.0
        };
        res_lines.push(Line::from(format!(
            "  Filter saved: {} tok ({pct:.0}%)",
            metrics.filter_saved_tokens,
        )));
    }
    let resources = Paragraph::new(res_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border)
            .title(" Resources "),
    );
    frame.render_widget(resources, area);
}
