// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! TUI durable execution panel (spec-064, #4949): a live table of durable executions.
//!
//! Renders only redaction-safe metadata (INV-5) — execution id, kind, status, step count, age —
//! never payload bytes. Data arrives as a [`DurableSnapshot`] via
//! [`AgentEvent::DurableSnapshot`](crate::AgentEvent::DurableSnapshot), refreshed by the binary's
//! durable poll task. When the journal is unreachable (durable execution disabled or the file is
//! missing) the panel shows the [`STATUS_UNAVAILABLE`] message; when `encryption_gate` (INV-8)
//! rejects the deployment's configuration it shows [`STATUS_GATE_REJECTED`] instead, so the two
//! distinct causes aren't conflated (see [`DurableStatus`]).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

use crate::theme::Theme;

/// Status message: the journal is unreachable, so the agent runs without durability (spec-011).
pub const STATUS_UNAVAILABLE: &str = "Journal unavailable — non-durable mode";

/// Status message: `encryption_gate` (INV-8) rejected this deployment's configuration.
pub const STATUS_GATE_REJECTED: &str =
    "Durable journal disabled by encryption policy (INV-8) — see logs";

/// One durable execution, display-ready. Carries no payload bytes (INV-5 redaction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableRow {
    /// First 8 characters of the execution UUID.
    pub id_short: String,
    /// The execution kind tag (`agent_turn`, `dag_run`, …).
    pub kind: String,
    /// The execution status (`running`, `completed`, `failed`, `aborted`).
    pub status: String,
    /// Number of journal entries recorded for the execution.
    pub step_count: u64,
    /// Seconds since the execution was created.
    pub age_secs: u64,
}

/// Why the durable panel does or doesn't have live execution rows to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DurableStatus {
    /// Durable execution disabled, or the journal file/backend could not be opened.
    #[default]
    Unavailable,
    /// `encryption_gate` (INV-8) rejected this configuration (e.g. shared DB + `encrypt_payload=false`).
    GateRejected,
    /// Journal reachable; `executions` holds the live rows (possibly empty).
    Available,
}

/// Snapshot of the durable journal for the panel, refreshed by the binary's poll task.
#[derive(Debug, Clone, Default)]
pub struct DurableSnapshot {
    /// Why the panel is or isn't showing live rows. Anything other than [`DurableStatus::Available`]
    /// renders a status message instead of `executions`.
    pub status: DurableStatus,
    /// Executions, newest first.
    pub executions: Vec<DurableRow>,
}

fn status_color(status: &str) -> Color {
    match status {
        "running" => Color::Green,
        "completed" => Color::DarkGray,
        "failed" => Color::Red,
        "aborted" => Color::Yellow,
        _ => Color::White,
    }
}

/// Render a coarse age such as `12s`, `5m`, `3h`, `2d`.
fn fmt_age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// Render the durable executions panel. `list_state` tracks scroll position.
pub fn render(
    snapshot: &DurableSnapshot,
    frame: &mut Frame,
    area: Rect,
    list_state: &mut ListState,
    theme: &Theme,
) {
    frame.render_widget(Clear, area);

    let exec_count = snapshot.executions.len();
    let header_text = format!("durable · {exec_count}  [D]");
    let header = Line::from(Span::styled(
        header_text,
        theme.system_message.add_modifier(Modifier::BOLD),
    ));
    let splits = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    frame.render_widget(Paragraph::new(header), splits[0]);

    match snapshot.status {
        DurableStatus::Unavailable => {
            let msg = Paragraph::new(STATUS_UNAVAILABLE).style(Style::default().fg(Color::Yellow));
            frame.render_widget(msg, splits[1]);
            return;
        }
        DurableStatus::GateRejected => {
            let msg = Paragraph::new(STATUS_GATE_REJECTED).style(Style::default().fg(Color::Red));
            frame.render_widget(msg, splits[1]);
            return;
        }
        DurableStatus::Available => {}
    }

    if snapshot.executions.is_empty() {
        let msg = Paragraph::new("No durable executions recorded.")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(msg, splits[1]);
        return;
    }

    let col_header = ListItem::new(Line::from(vec![Span::styled(
        format!(
            " {:<10}  {:<16}{:<10}{:>6}  {:>6}",
            "ID", "KIND", "STATUS", "STEPS", "AGE"
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )]));

    let mut items: Vec<ListItem> = vec![col_header];
    for (i, row) in snapshot.executions.iter().enumerate() {
        let selected = list_state.selected() == Some(i + 1);
        let base = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let color = status_color(&row.status);
        let line = Line::from(vec![
            Span::styled(format!(" {:<10}  ", row.id_short), base),
            Span::styled(format!("{:<16}", row.kind), base),
            Span::styled(format!("{:<10}", row.status), base.fg(color)),
            Span::styled(format!("{:>6}", row.step_count), base),
            Span::styled(format!("  {:>6}", fmt_age(row.age_secs)), base),
        ]);
        items.push(ListItem::new(line));
    }

    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, splits[1], list_state);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn fmt_age_scales_units() {
        assert_eq!(fmt_age(12), "12s");
        assert_eq!(fmt_age(300), "5m");
        assert_eq!(fmt_age(7_200), "2h");
        assert_eq!(fmt_age(172_800), "2d");
    }

    #[test]
    fn status_color_maps_known_states() {
        assert_eq!(status_color("running"), Color::Green);
        assert_eq!(status_color("failed"), Color::Red);
        assert_eq!(status_color("mystery"), Color::White);
    }

    /// Fills the whole area with a sentinel glyph before calling `render`, in the same frame —
    /// mirroring the real bug shape (#6048): multiple widgets drawing into the identical `Rect`
    /// within one `draw()` pass, where `Paragraph`/`List` only touch cells for their own content
    /// and leave any earlier glyph in place. Without the `Clear` call in `render`, the sentinel
    /// survives in every cell the status message doesn't cover; with it, none do.
    fn render_over_sentinel(snapshot: &DurableSnapshot) -> ratatui::buffer::Buffer {
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
    fn render_clears_stale_glyphs_before_drawing_unavailable_status() {
        let snapshot = DurableSnapshot {
            status: DurableStatus::Unavailable,
            executions: Vec::new(),
        };
        let buf = render_over_sentinel(&snapshot);
        for cell in &buf.content {
            assert_ne!(
                cell.symbol(),
                "#",
                "stray sentinel glyph survived render — Clear is missing or not applied to the whole area"
            );
        }
        let rendered: String = buf.content.iter().map(|c| c.symbol().to_owned()).collect();
        assert!(
            rendered.contains(STATUS_UNAVAILABLE),
            "expected unavailable status text, got: {rendered:?}"
        );
    }

    #[test]
    fn render_clears_stale_glyphs_before_drawing_gate_rejected_status() {
        let snapshot = DurableSnapshot {
            status: DurableStatus::GateRejected,
            executions: Vec::new(),
        };
        let buf = render_over_sentinel(&snapshot);
        for cell in &buf.content {
            assert_ne!(
                cell.symbol(),
                "#",
                "stray sentinel glyph survived render — Clear is missing or not applied to the whole area"
            );
        }
        let rendered: String = buf.content.iter().map(|c| c.symbol().to_owned()).collect();
        assert!(
            rendered.contains(STATUS_GATE_REJECTED),
            "expected gate-rejected status text, got: {rendered:?}"
        );
    }

    #[test]
    fn render_clears_stale_glyphs_before_drawing_available_list() {
        let snapshot = DurableSnapshot {
            status: DurableStatus::Available,
            executions: vec![DurableRow {
                id_short: "abcd1234".into(),
                kind: "agent_turn".into(),
                status: "running".into(),
                step_count: 3,
                age_secs: 12,
            }],
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
    fn unavailable_and_gate_rejected_render_distinct_messages() {
        assert_ne!(STATUS_UNAVAILABLE, STATUS_GATE_REJECTED);

        let unavailable = render_over_sentinel(&DurableSnapshot {
            status: DurableStatus::Unavailable,
            executions: Vec::new(),
        });
        let rendered_unavailable: String = unavailable
            .content
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect();
        assert!(rendered_unavailable.contains(STATUS_UNAVAILABLE));
        assert!(!rendered_unavailable.contains(STATUS_GATE_REJECTED));

        let gate_rejected = render_over_sentinel(&DurableSnapshot {
            status: DurableStatus::GateRejected,
            executions: Vec::new(),
        });
        let rendered_gate_rejected: String = gate_rejected
            .content
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect();
        assert!(rendered_gate_rejected.contains(STATUS_GATE_REJECTED));
        assert!(!rendered_gate_rejected.contains(STATUS_UNAVAILABLE));
    }

    #[test]
    fn available_status_falls_through_to_executions_list() {
        let snapshot = DurableSnapshot {
            status: DurableStatus::Available,
            executions: vec![DurableRow {
                id_short: "deadbeef".into(),
                kind: "dag_run".into(),
                status: "completed".into(),
                step_count: 7,
                age_secs: 300,
            }],
        };
        let buf = render_over_sentinel(&snapshot);
        let rendered: String = buf.content.iter().map(|c| c.symbol().to_owned()).collect();
        assert!(
            rendered.contains("deadbeef"),
            "expected execution row to render, got: {rendered:?}"
        );
        assert!(!rendered.contains(STATUS_UNAVAILABLE));
        assert!(!rendered.contains(STATUS_GATE_REJECTED));
    }

    #[test]
    fn available_status_with_empty_executions_renders_placeholder() {
        let snapshot = DurableSnapshot {
            status: DurableStatus::Available,
            executions: Vec::new(),
        };
        let buf = render_over_sentinel(&snapshot);
        let rendered: String = buf.content.iter().map(|c| c.symbol().to_owned()).collect();
        assert!(rendered.contains("No durable executions recorded."));
        assert!(!rendered.contains(STATUS_UNAVAILABLE));
        assert!(!rendered.contains(STATUS_GATE_REJECTED));
    }
}
