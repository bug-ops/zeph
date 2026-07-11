// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! TUI fleet panel: shows a live table of all agent sessions (#3884).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};
use zeph_common::format_tokens;
use zeph_memory::store::agent_sessions::{AgentSessionRow, SessionStatus};

use crate::layout::truncate_to_width;
use crate::theme::Theme;

/// Cached fleet data loaded from the database by the background refresh task.
#[derive(Debug, Clone, Default)]
pub struct FleetSnapshot {
    /// Sessions ordered by `last_active_at` descending.
    pub sessions: Vec<AgentSessionRow>,
}

fn status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Active => Color::Green,
        SessionStatus::Completed => Color::DarkGray,
        SessionStatus::Failed => Color::Red,
        SessionStatus::Cancelled => Color::Yellow,
        SessionStatus::Unknown => Color::Magenta,
        _ => Color::White,
    }
}

fn build_session_item(row: &AgentSessionRow, selected: bool) -> ListItem<'static> {
    let color = status_color(row.status);
    let base = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };

    let id_short: String = row.id.chars().take(8).collect();

    let model_short = truncate_to_width(&row.model, 20);

    let tokens = format!(
        "{}/{}",
        format_tokens(row.prompt_tokens),
        format_tokens(row.completion_tokens)
    );
    let cost = if row.cost_cents > 0.0 {
        format!("${:.4}", row.cost_cents / 100.0)
    } else {
        "—".to_owned()
    };

    let line = Line::from(vec![
        Span::styled(format!(" {id_short}  "), base),
        Span::styled(format!("{:<12}", row.kind), base.fg(color)),
        Span::styled(format!("{:<11}", row.status), base.fg(color)),
        Span::styled(format!("{:<5}", row.channel), base),
        Span::styled(format!("{model_short:<22}"), base),
        Span::styled(format!("{:>5}  ", row.turns), base),
        Span::styled(format!("{tokens:<15}"), base),
        Span::styled(cost, base),
    ]);
    ListItem::new(line)
}

/// Render the fleet panel.
///
/// `list_state` is used for scroll position tracking.
pub fn render(
    snapshot: &FleetSnapshot,
    frame: &mut Frame,
    area: Rect,
    list_state: &mut ListState,
    theme: &Theme,
) {
    frame.render_widget(Clear, area);

    let header_text = format!(
        "fleet · {} session{}  [f]",
        snapshot.sessions.len(),
        if snapshot.sessions.len() == 1 {
            ""
        } else {
            "s"
        },
    );
    let header = Line::from(Span::styled(
        header_text,
        theme.system_message.add_modifier(Modifier::BOLD),
    ));
    let splits = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    frame.render_widget(Paragraph::new(header), splits[0]);

    if snapshot.sessions.is_empty() {
        let msg = Paragraph::new("No agent sessions recorded.")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(msg, splits[1]);
        return;
    }

    let col_header = ListItem::new(Line::from(vec![Span::styled(
        format!(
            " {:<8}  {:<12}{:<11}{:<5}{:<22}{:>5}  {:<15}{}",
            "ID", "KIND", "STATUS", "CH", "MODEL", "TURNS", "P/C TOKENS", "COST"
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )]));

    let mut items: Vec<ListItem> = vec![col_header];
    for (i, row) in snapshot.sessions.iter().enumerate() {
        let selected = list_state.selected() == Some(i + 1);
        items.push(build_session_item(row, selected));
    }

    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, splits[1], list_state);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use zeph_memory::store::agent_sessions::{SessionChannel, SessionKind};

    use super::*;

    #[test]
    fn format_tokens_zero() {
        assert_eq!(format_tokens(0), "0");
    }

    #[test]
    fn format_tokens_below_1k() {
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn format_tokens_exactly_1k() {
        assert_eq!(format_tokens(1_000), "1.0k");
    }

    #[test]
    fn format_tokens_below_1m() {
        assert_eq!(format_tokens(999_999), "1000.0k");
    }

    #[test]
    fn format_tokens_exactly_1m() {
        assert_eq!(format_tokens(1_000_000), "1.0M");
    }

    fn sample_row(id: &str) -> AgentSessionRow {
        AgentSessionRow {
            id: id.to_owned(),
            kind: SessionKind::Interactive,
            status: SessionStatus::Active,
            channel: SessionChannel::Cli,
            model: "claude-sonnet-5".to_owned(),
            created_at: "2026-01-01T00:00:00".to_owned(),
            last_active_at: "2026-01-01T00:00:00".to_owned(),
            turns: 3,
            prompt_tokens: 100,
            completion_tokens: 50,
            reasoning_tokens: 0,
            cost_cents: 1.2345,
            goal_text: None,
        }
    }

    /// Fills the whole area with a sentinel glyph before calling `render`, in the same frame —
    /// mirroring the real bug shape (#6054): the fleet overlay shares its `Rect` with other
    /// sidebar widgets (see `render_subagents_slot`) but, before the fix, never called `Clear`,
    /// so stale glyphs from whatever rendered underneath survived in every cell the panel's own
    /// content didn't touch.
    fn render_over_sentinel(snapshot: &FleetSnapshot) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut list_state = ListState::default();
        terminal
            .draw(|frame| {
                let area = frame.area();
                for y in area.top()..area.bottom() {
                    for x in area.left()..area.right() {
                        frame.buffer_mut()[(x, y)].set_symbol("#");
                    }
                }
                render(snapshot, frame, area, &mut list_state, &Theme::default());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn render_clears_stale_glyphs_before_drawing_empty_state() {
        let snapshot = FleetSnapshot {
            sessions: Vec::new(),
        };
        let buf = render_over_sentinel(&snapshot);
        for cell in &buf.content {
            assert_ne!(
                cell.symbol(),
                "#",
                "stray sentinel glyph survived render — Clear is missing or not applied to the whole area"
            );
        }
    }

    #[test]
    fn render_clears_stale_glyphs_before_drawing_session_list() {
        let snapshot = FleetSnapshot {
            sessions: vec![sample_row("s1")],
        };
        let buf = render_over_sentinel(&snapshot);
        for cell in &buf.content {
            assert_ne!(
                cell.symbol(),
                "#",
                "stray sentinel glyph survived render — Clear is missing or not applied to the whole area"
            );
        }
    }
}
