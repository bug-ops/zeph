use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let has_mcp = !metrics.active_mcp_tools.is_empty() || metrics.mcp_tool_count > 0;
    let chunks = if has_mcp {
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area)
    } else {
        Layout::vertical([Constraint::Percentage(100), Constraint::Min(0)]).split(area)
    };

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
    frame.render_widget(skills, chunks[0]);

    if has_mcp {
        let mcp_lines: Vec<Line<'_>> = metrics
            .active_mcp_tools
            .iter()
            .map(|t| Line::from(format!("  - {t}")))
            .collect();
        let mcp = Paragraph::new(mcp_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(format!(
                    " MCP Tools ({}/{}) ",
                    metrics.active_mcp_tools.len(),
                    metrics.mcp_tool_count
                )),
        );
        frame.render_widget(mcp, chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::metrics::MetricsSnapshot;
    use crate::test_utils::render_to_string;

    #[test]
    fn skills_with_data() {
        let mut metrics = MetricsSnapshot::default();
        metrics.active_skills = vec!["web-search".into(), "code-gen".into()];
        metrics.total_skills = 5;

        let output = render_to_string(30, 10, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert_snapshot!(output);
    }
}
