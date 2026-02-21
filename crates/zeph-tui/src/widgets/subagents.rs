use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    if metrics.sub_agents.is_empty() {
        return;
    }

    let theme = Theme::default();

    let items: Vec<ListItem<'_>> = metrics
        .sub_agents
        .iter()
        .map(|sa| {
            let state_color = match sa.state.as_str() {
                "working" | "submitted" => Color::Yellow,
                "completed" => Color::Green,
                "failed" => Color::Red,
                "input_required" => Color::Cyan,
                _ => Color::DarkGray,
            };
            let bg_marker = if sa.background { " [bg]" } else { "" };
            let line = Line::from(vec![
                Span::styled(format!("  {}{}", sa.name, bg_marker), Style::default()),
                Span::styled(
                    format!("  {}", sa.state.to_uppercase()),
                    Style::default().fg(state_color),
                ),
                Span::raw(format!(
                    "  {}/{}  {}s",
                    sa.turns_used, sa.max_turns, sa.elapsed_secs
                )),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border)
            .title(format!(" Sub-Agents ({}) ", metrics.sub_agents.len())),
    );
    frame.render_widget(list, area);
}

#[cfg(test)]
mod tests {
    use crate::metrics::MetricsSnapshot;
    use crate::test_utils::render_to_string;
    use zeph_core::metrics::SubAgentMetrics;

    #[test]
    fn subagents_widget_renders_nothing_when_empty() {
        let metrics = MetricsSnapshot::default();
        // empty sub_agents — render should be a no-op (no panic)
        let output = render_to_string(30, 5, |frame, area| {
            super::render(&metrics, frame, area);
        });
        // widget renders nothing — output is all spaces
        assert!(output.chars().all(|c| c == ' ' || c == '\n'));
    }

    #[test]
    fn subagents_widget_renders_entries() {
        let mut metrics = MetricsSnapshot::default();
        metrics.sub_agents = vec![
            SubAgentMetrics {
                id: "abc123".into(),
                name: "code-reviewer".into(),
                state: "working".into(),
                turns_used: 3,
                max_turns: 20,
                background: false,
                elapsed_secs: 42,
            },
            SubAgentMetrics {
                id: "def456".into(),
                name: "test-writer".into(),
                state: "completed".into(),
                turns_used: 10,
                max_turns: 20,
                background: true,
                elapsed_secs: 100,
            },
        ];
        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(output.contains("Sub-Agents"));
        assert!(output.contains("code-reviewer"));
        assert!(output.contains("test-writer"));
    }
}
