// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
            let perm_badge = match sa.permission_mode.as_str() {
                "plan" => " [plan]",
                "bypass_permissions" => " [bypass!]",
                "dont_ask" => " [dont_ask]",
                "accept_edits" => " [accept_edits]",
                _ => "",
            };
            let line = Line::from(vec![
                Span::styled(
                    format!("  {}{}{}", sa.name, bg_marker, perm_badge),
                    Style::default(),
                ),
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
                permission_mode: String::new(),
            },
            SubAgentMetrics {
                id: "def456".into(),
                name: "test-writer".into(),
                state: "completed".into(),
                turns_used: 10,
                max_turns: 20,
                background: true,
                elapsed_secs: 100,
                permission_mode: "dont_ask".into(),
            },
        ];
        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(output.contains("Sub-Agents"));
        assert!(output.contains("code-reviewer"));
        assert!(output.contains("test-writer"));
        assert!(output.contains("[dont_ask]"));
    }

    #[test]
    fn subagents_widget_renders_permission_badges() {
        let mut metrics = MetricsSnapshot::default();
        metrics.sub_agents = vec![
            SubAgentMetrics {
                id: "a".into(),
                name: "planner".into(),
                state: "working".into(),
                turns_used: 1,
                max_turns: 5,
                background: false,
                elapsed_secs: 1,
                permission_mode: "plan".into(),
            },
            SubAgentMetrics {
                id: "b".into(),
                name: "bypasser".into(),
                state: "working".into(),
                turns_used: 1,
                max_turns: 5,
                background: false,
                elapsed_secs: 1,
                permission_mode: "bypass_permissions".into(),
            },
        ];
        let output = render_to_string(60, 10, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(output.contains("[plan]"));
        assert!(output.contains("[bypass!]"));
    }
}
