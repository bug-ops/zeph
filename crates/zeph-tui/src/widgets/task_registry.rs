// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Task registry panel widget.
//!
//! Renders a live view of all tasks tracked by [`TaskSupervisor`]. Each row
//! shows a spinner (for active tasks), the task name, current status, uptime
//! since the last (re)start, and the total restart count.
//!
//! # Uptime semantics
//!
//! The uptime column shows time elapsed since the task was **last started**
//! (or restarted). It resets on each restart. This is intentional: it lets
//! the operator see whether a task has been stable or has restarted recently.
//! Total lifetime cannot be derived exactly from the snapshot alone.
//!
//! [`TaskSupervisor`]: zeph_core::task_supervisor::TaskSupervisor

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use zeph_core::task_supervisor::{TaskSnapshot, TaskStatus};

use crate::theme::Theme;

/// Braille spinner frames for animated running tasks.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn status_color(status: &TaskStatus) -> Color {
    match status {
        TaskStatus::Running => Color::Yellow,
        TaskStatus::Restarting { .. } => Color::Cyan,
        TaskStatus::Completed => Color::Green,
        TaskStatus::Failed { .. } => Color::Red,
        TaskStatus::Aborted => Color::DarkGray,
    }
}

fn format_status(status: &TaskStatus) -> String {
    match status {
        TaskStatus::Running => "Running".to_owned(),
        TaskStatus::Restarting { attempt, max } => format!("Restart {attempt}/{max}"),
        TaskStatus::Completed => "Completed".to_owned(),
        TaskStatus::Failed { .. } => "Failed".to_owned(),
        TaskStatus::Aborted => "Aborted".to_owned(),
    }
}

fn format_uptime(started_at: Instant) -> String {
    let secs = started_at.elapsed().as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn build_list_item(snapshot: &TaskSnapshot, tick: u8) -> ListItem<'static> {
    let color = status_color(&snapshot.status);
    let is_active = matches!(
        snapshot.status,
        TaskStatus::Running | TaskStatus::Restarting { .. }
    );
    let spinner = if is_active {
        let idx = (tick as usize) % SPINNER_FRAMES.len();
        SPINNER_FRAMES[idx].to_string()
    } else {
        "  ".to_owned()
    };

    let uptime = format_uptime(snapshot.started_at);
    let status_str = format_status(&snapshot.status);
    let restarts = snapshot.restart_count;

    let line = Line::from(vec![
        Span::styled(format!(" {spinner} "), Style::default().fg(color)),
        Span::styled(
            format!("{:<20}", snapshot.name),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{status_str:<14}"), Style::default().fg(color)),
        Span::styled(format!("{uptime}  "), Style::default()),
        Span::styled(format!("↺{restarts}"), Style::default().fg(Color::DarkGray)),
    ]);
    ListItem::new(line)
}

/// Render the task registry panel into `area`.
///
/// Shows a spinner for running/restarting tasks, the task name, current
/// status (color-coded), uptime since last start, and restart count.
///
/// When `snapshots` is empty, displays a placeholder message instead.
///
/// # Arguments
///
/// * `snapshots` — point-in-time task list from `TaskSupervisor::snapshot()`.
/// * `tick` — current animation tick (wraps via `% SPINNER_FRAMES.len()`).
/// * `area` — terminal rect to render into.
/// * `frame` — ratatui frame for widget rendering.
pub fn render(snapshots: &[TaskSnapshot], tick: u8, area: Rect, frame: &mut Frame<'_>) {
    let theme = Theme::default();
    let title = format!(" Tasks ({}) ", snapshots.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(title);

    if snapshots.is_empty() {
        let paragraph = Paragraph::new(" No supervised tasks registered yet.")
            .block(block)
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, area);
        return;
    }

    let items: Vec<ListItem<'_>> = snapshots.iter().map(|s| build_list_item(s, tick)).collect();
    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use insta::assert_snapshot;
    use zeph_core::task_supervisor::{TaskSnapshot, TaskStatus};

    use crate::test_utils::render_to_string;

    fn running_snapshot(name: &'static str) -> TaskSnapshot {
        TaskSnapshot {
            name,
            status: TaskStatus::Running,
            started_at: Instant::now(),
            restart_count: 0,
        }
    }

    fn completed_snapshot(name: &'static str) -> TaskSnapshot {
        TaskSnapshot {
            name,
            status: TaskStatus::Completed,
            started_at: Instant::now(),
            restart_count: 1,
        }
    }

    fn failed_snapshot(name: &'static str) -> TaskSnapshot {
        TaskSnapshot {
            name,
            status: TaskStatus::Failed {
                reason: "oops".into(),
            },
            started_at: Instant::now(),
            restart_count: 3,
        }
    }

    #[test]
    fn render_empty_does_not_panic() {
        // Must not panic; displays placeholder text.
        let output = render_to_string(50, 6, |frame, area| {
            super::render(&[], 0, area, frame);
        });
        assert!(
            output.contains("No supervised tasks"),
            "expected placeholder: {output}"
        );
    }

    #[test]
    fn render_running_task_shows_name_and_status() {
        let snapshots = [running_snapshot("config-watcher")];
        let output = render_to_string(60, 5, |frame, area| {
            super::render(&snapshots, 0, area, frame);
        });
        assert!(output.contains("config-watcher"), "name missing: {output}");
        assert!(output.contains("Running"), "status missing: {output}");
    }

    #[test]
    fn render_completed_task_shows_status() {
        let snapshots = [completed_snapshot("memory-loop")];
        let output = render_to_string(60, 5, |frame, area| {
            super::render(&snapshots, 0, area, frame);
        });
        assert!(output.contains("memory-loop"), "name missing: {output}");
        assert!(output.contains("Completed"), "status missing: {output}");
    }

    #[test]
    fn render_failed_task_shows_status() {
        let snapshots = [failed_snapshot("scheduler")];
        let output = render_to_string(60, 5, |frame, area| {
            super::render(&snapshots, 0, area, frame);
        });
        assert!(output.contains("scheduler"), "name missing: {output}");
        assert!(output.contains("Failed"), "status missing: {output}");
    }

    #[test]
    fn render_multiple_tasks_snapshot() {
        let snapshots = [
            running_snapshot("config-watcher"),
            completed_snapshot("memory-loop"),
            failed_snapshot("scheduler"),
        ];
        let output = render_to_string(70, 8, |frame, area| {
            super::render(&snapshots, 2, area, frame);
        });
        assert_snapshot!(output);
    }
}
