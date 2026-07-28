// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::metrics::{McpServerConnectionStatus, MetricsSnapshot, SkillConfidence};
use crate::theme::Theme;
use crate::widgets::panel;

/// Build the skills panel's two logical sections: the active-skills list and, when any MCP
/// tools are configured, the MCP servers/tools list. Both are pure functions of `metrics` and
/// `theme` — never of the allocated `Rect` — so [`desired_height`] and [`render`] can never
/// disagree about how many rows this panel needs.
///
/// Returns `(skills_lines, mcp_lines)`; `mcp_lines` is empty when no MCP tools are configured.
pub(crate) fn sections(
    metrics: &MetricsSnapshot,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let confidence_map: std::collections::HashMap<&str, &SkillConfidence> = metrics
        .skill_confidence
        .iter()
        .map(|c| (c.name.as_str(), c))
        .collect();

    let skill_lines: Vec<Line<'static>> = metrics
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

    // Section header: lowercase muted label, no border box
    let skills_header = Line::from(Span::styled(
        format!(
            "skills  {} active / {} loaded",
            metrics.active_skills.len(),
            metrics.total_skills
        ),
        theme.system_message.add_modifier(Modifier::BOLD),
    ));
    let mut skills_content = vec![skills_header];
    skills_content.extend(skill_lines);

    let has_mcp = !metrics.active_mcp_tools.is_empty() || metrics.mcp_tool_count > 0;
    let mut mcp_lines: Vec<Line<'static>> = Vec::new();
    if has_mcp {
        mcp_lines.push(Line::from(Span::styled(
            format!(
                "mcp tools  {}/{}",
                metrics.active_mcp_tools.len(),
                metrics.mcp_tool_count
            ),
            theme.system_message.add_modifier(Modifier::BOLD),
        )));
        for srv in &metrics.mcp_servers {
            let (indicator, color) = match srv.status {
                McpServerConnectionStatus::Connected => ("ok", Color::Green),
                McpServerConnectionStatus::Failed => ("fail", Color::Red),
                _ => ("?", Color::DarkGray),
            };
            let mut spans = vec![
                Span::raw(format!("  {} ", srv.id)),
                Span::styled(indicator, Style::default().fg(color)),
                Span::raw(format!(" ({})", srv.tool_count)),
            ];
            let schemas_dropped = srv.input_schemas_dropped + srv.output_schemas_dropped;
            if schemas_dropped > 0 {
                spans.push(Span::styled(
                    format!(" schema-drop:{schemas_dropped}"),
                    Style::default().fg(Color::Yellow),
                ));
            }
            mcp_lines.push(Line::from(spans));
        }
        for t in &metrics.active_mcp_tools {
            mcp_lines.push(Line::from(format!("  - {t}")));
        }
    }

    (skills_content, mcp_lines)
}

/// Number of rows the skills panel needs to show both sections without truncation.
#[must_use]
pub fn desired_height(metrics: &MetricsSnapshot, theme: &Theme) -> u16 {
    let (skills, mcp) = sections(metrics, theme);
    u16::try_from(skills.len() + mcp.len()).unwrap_or(u16::MAX)
}

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect, theme: &Theme) {
    let (skills_content, mcp_lines) = sections(metrics, theme);
    if mcp_lines.is_empty() {
        panel::render_lines(frame, area, skills_content, theme);
        return;
    }

    // Content-length split: when there's room, the skills section gets exactly its own line
    // count and the MCP section gets the rest. Under space pressure `split_two_sections`
    // still gives the MCP section its own floor row so it can show a `+N more` indicator
    // instead of vanishing entirely when the skills section alone exceeds the granted area
    // (#6675 S2).
    let skills_demand = u16::try_from(skills_content.len()).unwrap_or(u16::MAX);
    let mcp_demand = u16::try_from(mcp_lines.len()).unwrap_or(u16::MAX);
    let skills_h = split_two_sections(skills_demand, mcp_demand, area.height);
    let chunks = Layout::vertical([Constraint::Length(skills_h), Constraint::Min(0)]).split(area);
    panel::render_lines(frame, chunks[0], skills_content, theme);
    panel::render_lines(frame, chunks[1], mcp_lines, theme);
}

/// Split `available` rows between two stacked sections so that, unless space is critically
/// tight, each section gets at least one row and can render its own overflow indicator
/// instead of being silently dropped when the other section's demand alone exceeds
/// `available`. Returns the height granted to the first section; the second is expected to
/// receive whatever remains (e.g. via `Constraint::Min(0)`), which reproduces "first section
/// gets exactly its own line count" whenever `available` covers both demands.
fn split_two_sections(first_demand: u16, second_demand: u16, available: u16) -> u16 {
    if available == 0 {
        return 0;
    }
    if available == 1 {
        // Only one row total: give it to whichever section actually has something to show;
        // ties (including neither having content) favor the first section.
        return u16::from(first_demand >= 1 || second_demand == 0);
    }
    let first_floor = u16::from(first_demand >= 1);
    let second_floor = u16::from(second_demand >= 1);
    let remaining = available - first_floor - second_floor;
    let first_room = first_demand.saturating_sub(first_floor);
    first_floor + remaining.min(first_room)
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

        let theme = crate::theme::Theme::default();
        let output = render_to_string(30, 10, |frame, area| {
            super::render(&metrics, frame, area, &theme);
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

        let theme = crate::theme::Theme::default();
        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area, &theme);
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
        let theme = crate::theme::Theme::default();
        let output = render_to_string(40, 8, |frame, area| {
            super::render(&metrics, frame, area, &theme);
        });
        assert!(
            output.contains("- unknown-skill"),
            "expected dash prefix, got:\n{output}"
        );
    }

    // ── split_two_sections / MCP-under-pressure (#6675 S2) ────────────────────

    #[test]
    fn split_two_sections_generous_space_gives_first_its_full_demand() {
        assert_eq!(super::split_two_sections(5, 3, 20), 5);
    }

    #[test]
    fn split_two_sections_zero_available_gives_nothing() {
        assert_eq!(super::split_two_sections(5, 3, 0), 0);
    }

    #[test]
    fn split_two_sections_single_row_favors_first_when_both_want_it() {
        assert_eq!(super::split_two_sections(5, 3, 1), 1);
    }

    #[test]
    fn split_two_sections_single_row_goes_to_second_when_first_is_empty() {
        assert_eq!(super::split_two_sections(0, 3, 1), 0);
    }

    #[test]
    fn split_two_sections_second_keeps_its_floor_under_pressure() {
        // First section alone would exceed the whole budget; second must still get >= 1 row.
        let first_h = super::split_two_sections(10, 3, 4);
        assert!(
            first_h <= 3,
            "second section must keep at least 1 row, got first={first_h}"
        );
    }

    #[test]
    fn mcp_section_shows_overflow_indicator_instead_of_vanishing_under_pressure() {
        let metrics = MetricsSnapshot {
            active_skills: vec![
                "one".into(),
                "two".into(),
                "three".into(),
                "four".into(),
                "five".into(),
            ],
            total_skills: 5,
            mcp_tool_count: 3,
            active_mcp_tools: vec!["tool-a".into(), "tool-b".into(), "tool-c".into()],
            ..MetricsSnapshot::default()
        };
        let theme = crate::theme::Theme::default();
        // Skills section alone (header + 5 lines = 6) exceeds this tiny area; the MCP
        // section must still get a chance to show its own truncation indicator rather than
        // disappearing with height 0.
        let output = render_to_string(40, 4, |frame, area| {
            super::render(&metrics, frame, area, &theme);
        });
        assert!(
            output.contains("more"),
            "MCP section must not vanish silently under pressure, got:\n{output}"
        );
    }

    // ── desired_height / render parity (#6675 tester gap 2) ─────────────────────

    #[test]
    fn desired_height_matches_actual_rendered_row_count() {
        use crate::metrics::McpServerConnectionStatus;
        use crate::test_utils::count_non_blank_rows;
        use zeph_core::metrics::McpServerStatus;

        let metrics = MetricsSnapshot {
            active_skills: vec!["web-search".into(), "code-gen".into()],
            total_skills: 5,
            skill_confidence: vec![SkillConfidence {
                name: "web-search".into(),
                posterior: 0.8,
                total_uses: 12,
            }],
            mcp_tool_count: 2,
            active_mcp_tools: vec!["tool-a".into()],
            mcp_servers: vec![McpServerStatus {
                id: "srv1".into(),
                status: McpServerConnectionStatus::Connected,
                tool_count: 2,
                error: String::new(),
                input_schemas_dropped: 0,
                output_schemas_dropped: 0,
            }],
            ..MetricsSnapshot::default()
        };
        let theme = crate::theme::Theme::default();
        let expected = super::desired_height(&metrics, &theme);

        // Oversized area, comfortably split between both sections: nothing truncates.
        let output = render_to_string(80, 30, |frame, area| {
            super::render(&metrics, frame, area, &theme);
        });
        assert_eq!(
            u16::try_from(count_non_blank_rows(&output)).unwrap(),
            expected,
            "desired_height must match the actual non-blank rendered row count, got:\n{output}"
        );
    }
}
