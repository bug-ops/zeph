use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let skill_lines: Vec<Line<'_>> = metrics
        .active_skills
        .iter()
        .map(|s| Line::from(format!("  - {s}")))
        .collect();
    let skills = Paragraph::new(skill_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border)
            .title(format!(
                " Skills ({}/{}) ",
                metrics.active_skills.len(),
                metrics.total_skills
            )),
    );
    frame.render_widget(skills, area);
}
