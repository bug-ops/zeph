// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! TUI durable execution panel (spec-064, #4949): a live table of durable executions.
//!
//! Renders only redaction-safe metadata (INV-5) — execution id, kind, status, step count, age —
//! never payload bytes. Data arrives as a [`DurableSnapshot`] via
//! [`AgentEvent::DurableSnapshot`](crate::AgentEvent::DurableSnapshot), refreshed by the binary's
//! durable poll task. When the journal is unreachable (durable execution disabled or the file is
//! missing) the panel shows the [`STATUS_UNAVAILABLE`] message.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use crate::theme::Theme;

/// Status message: a crashed execution is replaying its journal (spec-011).
pub const STATUS_REPLAYING: &str = "Replaying execution…";
/// Status message: a background retention sweep is running (spec-011).
pub const STATUS_PRUNING: &str = "Pruning journal…";
/// Status message: a durable promise is parked awaiting an external completion (spec-011).
pub const STATUS_AWAITING: &str = "Awaiting external completion…";
/// Status message: the journal is unreachable, so the agent runs without durability (spec-011).
pub const STATUS_UNAVAILABLE: &str = "Journal unavailable — non-durable mode";

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

/// Snapshot of the durable journal for the panel, refreshed by the binary's poll task.
#[derive(Debug, Clone, Default)]
pub struct DurableSnapshot {
    /// Whether the journal could be reached. `false` renders [`STATUS_UNAVAILABLE`].
    pub available: bool,
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
) {
    let theme = Theme::default();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(Span::styled(
            " Durable [D] ",
            Style::default().add_modifier(Modifier::BOLD),
        ));

    let placeholder = if !snapshot.available {
        Some((STATUS_UNAVAILABLE, Color::Yellow))
    } else if snapshot.executions.is_empty() {
        Some(("No durable executions recorded.", Color::DarkGray))
    } else {
        None
    };

    if let Some((text, color)) = placeholder {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let msg = ratatui::widgets::Paragraph::new(text).style(Style::default().fg(color));
        frame.render_widget(msg, inner);
        return;
    }

    let header = ListItem::new(Line::from(vec![Span::styled(
        format!(
            " {:<10}  {:<16}{:<10}{:>6}  {:>6}",
            "ID", "KIND", "STATUS", "STEPS", "AGE"
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )]));

    let mut items: Vec<ListItem> = vec![header];
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

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, list_state);
}

#[cfg(test)]
mod tests {
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
}
