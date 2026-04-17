// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::metrics::{McpServerConnectionStatus, MetricsSnapshot, SkillConfidence};
use crate::theme::Theme;

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let has_mcp = !metrics.active_mcp_tools.is_empty() || metrics.mcp_tool_count > 0;
    let chunks = if has_mcp {
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area)
    } else {
        Layout::vertical([Constraint::Percentage(100), Constraint::Min(0)]).split(area)
    };

    let confidence_map: std::collections::HashMap<&str, &SkillConfidence> = metrics
        .skill_confidence
        .iter()
        .map(|c| (c.name.as_str(), c))
        .collect();

    let skill_lines: Vec<Line<'_>> = metrics
        .active_skills
        .iter()
        .map(|s| {
            if let Some(conf) = confidence_map.get(s.as_str()) {
                let bar = confidence_bar(conf.posterior, 8);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let pct = (conf.posterior * 100.0) as u32;
                let color = confidence_color(conf.posterior);
                Line::from(vec![
                    Span::raw(format!("  {s}  ")),
                    Span::styled(bar, Style::default().fg(color)),
                    Span::raw(format!(" {pct}% ({})", conf.total_uses)),
                ])
            } else {
                Line::from(format!("  - {s}"))
            }
        })
        .collect();

    let skills = Paragraph::new(skill_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border)
            .title(format!(
                " Skills ({} active / {} loaded) ",
                metrics.active_skills.len(),
                metrics.total_skills
            )),
    );
    frame.render_widget(skills, chunks[0]);

    if has_mcp {
        let mut mcp_lines: Vec<Line<'_>> = Vec::new();
        for srv in &metrics.mcp_servers {
            let (indicator, color) = match srv.status {
                McpServerConnectionStatus::Connected => ("OK", Color::Green),
                McpServerConnectionStatus::Failed => ("FAIL", Color::Red),
            };
            mcp_lines.push(Line::from(vec![
                Span::raw(format!("  {} ", srv.id)),
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(format!(" ({})", srv.tool_count)),
            ]));
        }
        for t in &metrics.active_mcp_tools {
            mcp_lines.push(Line::from(format!("  - {t}")));
        }
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

fn confidence_bar(posterior: f64, width: usize) -> String {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let filled = ((posterior * width as f64).round() as usize).min(width);
    let empty = width - filled;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

fn confidence_color(posterior: f64) -> Color {
    if posterior > 0.75 {
        Color::Green
    } else if posterior >= 0.40 {
        Color::Yellow
    } else {
        Color::Red
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::metrics::{MetricsSnapshot, SkillConfidence};
    use crate::test_utils::render_to_string;

    #[test]
    fn skills_with_data() {
        let metrics = MetricsSnapshot {
            active_skills: vec!["web-search".into(), "code-gen".into()],
            total_skills: 5,
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(30, 10, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn skills_with_confidence() {
        let metrics = MetricsSnapshot {
            active_skills: vec!["git".into(), "docker".into()],
            total_skills: 3,
            skill_confidence: vec![
                SkillConfidence {
                    name: "git".into(),
                    posterior: 0.92,
                    total_uses: 42,
                },
                SkillConfidence {
                    name: "docker".into(),
                    posterior: 0.35,
                    total_uses: 5,
                },
            ],
            ..MetricsSnapshot::default()
        };

        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn confidence_bar_full() {
        assert_eq!(super::confidence_bar(1.0, 8), "[████████]");
    }

    #[test]
    fn confidence_bar_empty() {
        assert_eq!(super::confidence_bar(0.0, 8), "[░░░░░░░░]");
    }

    #[test]
    fn confidence_bar_half() {
        assert_eq!(super::confidence_bar(0.5, 8), "[████░░░░]");
    }

    #[test]
    fn confidence_color_green() {
        assert_eq!(super::confidence_color(0.9), ratatui::style::Color::Green);
    }

    #[test]
    fn confidence_color_yellow() {
        assert_eq!(super::confidence_color(0.6), ratatui::style::Color::Yellow);
    }

    #[test]
    fn confidence_color_red() {
        assert_eq!(super::confidence_color(0.2), ratatui::style::Color::Red);
    }

    #[test]
    fn confidence_color_boundary_exactly_0_75_is_yellow() {
        // >0.75 = Green, >=0.40 = Yellow → 0.75 itself should be Yellow
        assert_eq!(super::confidence_color(0.75), ratatui::style::Color::Yellow);
    }

    #[test]
    fn confidence_color_boundary_exactly_0_40_is_yellow() {
        // >=0.40 = Yellow → exactly 0.40 should be Yellow
        assert_eq!(super::confidence_color(0.40), ratatui::style::Color::Yellow);
    }

    #[test]
    fn confidence_color_just_below_0_40_is_red() {
        assert_eq!(super::confidence_color(0.39), ratatui::style::Color::Red);
    }

    #[test]
    fn confidence_color_just_above_0_75_is_green() {
        assert_eq!(super::confidence_color(0.76), ratatui::style::Color::Green);
    }

    #[test]
    fn confidence_bar_width_zero_no_panic() {
        let result = super::confidence_bar(0.5, 0);
        assert_eq!(result, "[]");
    }

    #[test]
    fn skills_no_confidence_uses_dash_prefix() {
        let metrics = MetricsSnapshot {
            active_skills: vec!["unknown-skill".into()],
            total_skills: 1,
            ..MetricsSnapshot::default()
        };
        // No skill_confidence entries → should render with "  - " prefix
        let output = render_to_string(40, 8, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(
            output.contains("- unknown-skill"),
            "expected dash prefix, got:\n{output}"
        );
    }
}
