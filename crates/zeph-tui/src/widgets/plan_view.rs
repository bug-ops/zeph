// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use crate::metrics::MetricsSnapshot;

/// Spinner characters cycled using the frame counter from `ThrobberState` tick parity.
const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn spinner_char(tick: u8) -> char {
    SPINNER_CHARS[tick as usize % SPINNER_CHARS.len()]
}

fn status_color(status: &str) -> Color {
    match status {
        "ready" => Color::White,
        "running" => Color::Yellow,
        "completed" => Color::Green,
        "failed" => Color::Red,
        "canceled" => Color::Magenta,
        // "pending", "skipped", and any unknown status
        _ => Color::DarkGray,
    }
}

fn render_placeholder(frame: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Plan");
    let para = Paragraph::new("No active plan. Use /plan <goal> to create one.")
        .block(block)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(para, area);
}

fn build_task_row(task: &crate::metrics::TaskSnapshotRow, tick: u8) -> Row<'static> {
    let color = status_color(&task.status);
    let icon = if task.status == "running" {
        Span::styled(
            spinner_char(tick).to_string(),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Span::raw(" ")
    };

    let title_display = if task.title.len() > 28 {
        let end = task.title.floor_char_boundary(27);
        format!("{}…", &task.title[..end])
    } else {
        task.title.clone()
    };

    let title_with_err = if task.status == "failed" {
        if let Some(ref err) = task.error {
            format!("{title_display} [{err}]")
        } else {
            title_display
        }
    } else {
        title_display
    };

    let agent_display = task
        .agent
        .as_deref()
        .map(|a| {
            if a.len() > 10 {
                let end = a.floor_char_boundary(9);
                format!("{}…", &a[..end])
            } else {
                a.to_owned()
            }
        })
        .unwrap_or_default();

    let duration = if task.duration_ms > 0 {
        task.duration_ms.to_string()
    } else {
        String::new()
    };

    Row::new([
        Cell::from(Line::from(icon)),
        Cell::from(task.id.to_string()).style(Style::default().fg(Color::DarkGray)),
        Cell::from(title_with_err).style(Style::default().fg(color)),
        Cell::from(task.status.clone()).style(Style::default().fg(color)),
        Cell::from(agent_display).style(Style::default().fg(Color::Cyan)),
        Cell::from(duration).style(Style::default().fg(Color::DarkGray)),
    ])
}

/// Render the plan view widget in the given area.
///
/// When `metrics.orchestration_graph` is `None`, renders a placeholder paragraph.
/// When it contains a snapshot, renders a table with per-task rows.
#[allow(clippy::too_many_lines)]
pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect, tick: u8) {
    let Some(ref snapshot) = metrics.orchestration_graph else {
        render_placeholder(frame, area);
        return;
    };

    // Stale snapshots (completed/failed/canceled >30s ago) show as empty.
    if snapshot.is_stale() {
        render_placeholder(frame, area);
        return;
    }

    let any_running = snapshot.tasks.iter().any(|t| t.status == "running");

    let title = match snapshot.status.as_str() {
        "created" => format!(
            " Plan [pending confirmation]: {} ",
            truncate_goal(&snapshot.goal, 30)
        ),
        "running" => format!(
            " Plan {} [running…]: {} ",
            spinner_char(tick),
            truncate_goal(&snapshot.goal, 30)
        ),
        "completed" => format!(" Plan [completed]: {} ", truncate_goal(&snapshot.goal, 30)),
        "failed" => format!(" Plan [failed]: {} ", truncate_goal(&snapshot.goal, 30)),
        "paused" => format!(" Plan [paused]: {} ", truncate_goal(&snapshot.goal, 30)),
        "canceled" => format!(" Plan [canceled]: {} ", truncate_goal(&snapshot.goal, 30)),
        _ => format!(" Plan: {} ", truncate_goal(&snapshot.goal, 30)),
    };

    // Show a spinner in the title when tasks are actively running.
    let title_span = if any_running {
        Span::styled(title, Style::default().fg(Color::Yellow))
    } else {
        Span::raw(title)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(title_span));

    let widths = [
        Constraint::Length(2),  // spinner or status icon
        Constraint::Length(3),  // id
        Constraint::Fill(1),    // title
        Constraint::Length(10), // status
        Constraint::Length(12), // agent
        Constraint::Length(8),  // duration
    ];

    let header = Row::new([
        Cell::from(""),
        Cell::from("#").style(Style::default().fg(Color::DarkGray)),
        Cell::from("Title").style(Style::default().fg(Color::DarkGray)),
        Cell::from("Status").style(Style::default().fg(Color::DarkGray)),
        Cell::from("Agent").style(Style::default().fg(Color::DarkGray)),
        Cell::from("ms").style(Style::default().fg(Color::DarkGray)),
    ]);

    let rows: Vec<Row<'_>> = snapshot
        .tasks
        .iter()
        .map(|task| build_task_row(task, tick))
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1);

    frame.render_widget(table, area);
}

fn truncate_goal(goal: &str, max: usize) -> String {
    if goal.len() <= max {
        goal.to_owned()
    } else {
        let end = goal.floor_char_boundary(max.saturating_sub(1));
        format!("{}…", &goal[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricsSnapshot, TaskGraphSnapshot, TaskSnapshotRow};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn make_snapshot(status: &str, tasks: Vec<(&str, &str)>) -> TaskGraphSnapshot {
        TaskGraphSnapshot {
            graph_id: "test-id".into(),
            goal: "Test goal".into(),
            status: status.to_owned(),
            tasks: tasks
                .into_iter()
                .enumerate()
                .map(|(i, (title, stat))| TaskSnapshotRow {
                    id: u32::try_from(i).expect("test task index fits in u32"),
                    title: title.to_owned(),
                    status: stat.to_owned(),
                    agent: None,
                    duration_ms: 0,
                    error: None,
                })
                .collect(),
            completed_at: None,
        }
    }

    fn render_to_buffer(metrics: &MetricsSnapshot) -> String {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render(metrics, frame, area, 0);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect::<String>()
    }

    #[test]
    fn empty_graph_renders_placeholder() {
        let metrics = MetricsSnapshot::default();
        let rendered = render_to_buffer(&metrics);
        assert!(
            rendered.contains("No active plan"),
            "expected placeholder text, got: {rendered:?}"
        );
    }

    #[test]
    fn render_row_count_three_tasks() {
        let metrics = MetricsSnapshot {
            orchestration_graph: Some(make_snapshot(
                "created",
                vec![
                    ("Task Alpha", "pending"),
                    ("Task Beta", "running"),
                    ("Task Gamma", "completed"),
                ],
            )),
            ..MetricsSnapshot::default()
        };
        let rendered = render_to_buffer(&metrics);
        assert!(rendered.contains("Task Alpha"), "missing Task Alpha");
        assert!(rendered.contains("Task Beta"), "missing Task Beta");
        assert!(rendered.contains("Task Gamma"), "missing Task Gamma");
    }

    #[test]
    fn status_colors_map_correctly() {
        assert_eq!(status_color("pending"), Color::DarkGray);
        assert_eq!(status_color("ready"), Color::White);
        assert_eq!(status_color("running"), Color::Yellow);
        assert_eq!(status_color("completed"), Color::Green);
        assert_eq!(status_color("failed"), Color::Red);
        assert_eq!(status_color("skipped"), Color::DarkGray);
        assert_eq!(status_color("canceled"), Color::Magenta);
        assert_eq!(status_color("unknown"), Color::DarkGray);
    }

    #[test]
    fn stale_completed_snapshot_shows_placeholder() {
        let mut metrics = MetricsSnapshot::default();
        let mut snap = make_snapshot("completed", vec![("Task", "completed")]);
        // Simulate a completed_at 31 seconds ago.
        snap.completed_at = Some(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(31))
                .unwrap(),
        );
        metrics.orchestration_graph = Some(snap);
        let rendered = render_to_buffer(&metrics);
        assert!(
            rendered.contains("No active plan"),
            "stale completed snapshot should show placeholder"
        );
    }

    #[test]
    fn active_completed_snapshot_shows_tasks() {
        let mut metrics = MetricsSnapshot::default();
        let mut snap = make_snapshot("completed", vec![("My Task", "completed")]);
        // Just finished — within 30-second window.
        snap.completed_at = Some(std::time::Instant::now());
        metrics.orchestration_graph = Some(snap);
        let rendered = render_to_buffer(&metrics);
        assert!(
            rendered.contains("My Task"),
            "fresh completed snapshot should still show tasks"
        );
    }

    #[test]
    fn mixed_status_tasks_render() {
        let metrics = MetricsSnapshot {
            orchestration_graph: Some(make_snapshot(
                "running",
                vec![
                    ("Step 1", "completed"),
                    ("Step 2", "running"),
                    ("Step 3", "failed"),
                    ("Step 4", "pending"),
                ],
            )),
            ..MetricsSnapshot::default()
        };
        let rendered = render_to_buffer(&metrics);
        assert!(rendered.contains("Step 1"));
        assert!(rendered.contains("Step 2"));
        assert!(rendered.contains("Step 3"));
        assert!(rendered.contains("Step 4"));
    }

    #[test]
    fn spinner_chars_cycle() {
        // Ensure all 10 spinner chars are reachable.
        let chars: Vec<char> = (0..10u8).map(spinner_char).collect();
        assert_eq!(chars.len(), 10);
        assert!(chars.iter().all(|c| SPINNER_CHARS.contains(c)));
    }
}
