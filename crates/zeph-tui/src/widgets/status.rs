use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, InputMode};
use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

pub fn render(app: &App, metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let mode = match app.input_mode() {
        InputMode::Normal => "Normal",
        InputMode::Insert => "Insert",
    };

    let qdrant = if metrics.qdrant_available { "OK" } else { "--" };

    let uptime = format_uptime(metrics.uptime_seconds);

    let panel = if app.show_side_panels() { "ON" } else { "OFF" };

    let cancel_hint = if app.is_agent_busy() && app.input_mode() == InputMode::Normal {
        " | [Esc to cancel]"
    } else {
        ""
    };

    let text = format!(
        " [{mode}] | Panel: {panel} | Skills: {active}/{total} | Tokens: {tok} | Qdrant: {qdrant} | API: {api} | {uptime}{cancel_hint}",
        active = metrics.active_skills.len(),
        total = metrics.total_skills,
        tok = format_tokens(metrics.total_tokens),
        api = metrics.api_calls,
    );

    let line = Line::from(Span::styled(text, theme.status_bar));
    let paragraph = Paragraph::new(line).style(theme.status_bar);
    frame.render_widget(paragraph, area);
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
}
