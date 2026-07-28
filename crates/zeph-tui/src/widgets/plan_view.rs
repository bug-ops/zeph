// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};

use crate::metrics::MetricsSnapshot;
use crate::widgets::spinner::breeze_frame;

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

fn render_placeholder(frame: &mut Frame, area: Rect, theme: &crate::theme::Theme) {
    let header = Line::from(Span::styled(
        "plan · idle",
        theme.system_message.add_modifier(Modifier::BOLD),
    ));
    let splits = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    frame.render_widget(Paragraph::new(header), splits[0]);
    let para = Paragraph::new("No active plan. Use /plan <goal> to create one.")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(para, splits[1]);
}

fn build_task_row(task: &crate::metrics::TaskSnapshotRow, tick: u8, ascii: bool) -> Row<'static> {
    let color = status_color(&task.status);
    let icon = if task.status == "running" {
        Span::styled(
            breeze_frame(u64::from(tick), ascii).to_owned(),
            Style::default().fg(Color::Yellow),
        )
    } else {
        // Pad idle to 3 spaces to match the 3-cell wide active frame, preventing column jitter.
        Span::raw("   ")
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
    } else if let Some(ref rejected) = task.handoff_rejected {
        // spec-080/#6390: a Command-handoff `goto` rejection itself does not change the
        // node's own status (it stays completed with its real output) — surface it as a
        // title suffix regardless of status, mirroring the failed-error suffix above. A
        // *later*, independent post-hoc correction (#6394's CheckToolOutcome) can still
        // flip that same node to `failed` afterward, which is exactly why this branch is
        // reached only when the `status == "failed"` arm above didn't already match —
        // see the precedence test below for that now-reachable combination.
        format!("{title_display} [handoff rejected: {rejected}]")
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

/// Build the per-task table rows for an active, non-stale plan snapshot.
///
/// Shared by [`render`] and [`desired_height`] so the row *count* measurement can never
/// drift from what `render` actually builds — `tick`/`ascii` only affect a spinner glyph,
/// never the number of rows, so [`desired_height`] calls this with fixed placeholder values.
fn build_rows(
    snapshot: &crate::metrics::TaskGraphSnapshot,
    tick: u8,
    ascii: bool,
) -> Vec<Row<'static>> {
    snapshot
        .tasks
        .iter()
        .map(|task| build_task_row(task, tick, ascii))
        .collect()
}

/// Number of rows the plan view needs: `2` for the idle placeholder (header + hint line),
/// or `2 + tasks.len()` when an active, non-stale plan snapshot is present (header + table
/// column header + one row per task).
///
/// Pure function of `metrics` — never of the allocated `Rect` — so [`desired_height`] and
/// [`render`] can never disagree about how many rows this panel needs.
#[must_use]
pub fn desired_height(metrics: &MetricsSnapshot) -> u16 {
    let Some(ref snapshot) = metrics.orchestration_graph else {
        return 2;
    };
    if snapshot.is_stale() {
        return 2;
    }
    // tick/ascii are irrelevant to row count; build_rows is the same function render() uses
    // so a future change to which tasks get a row (e.g. filtering) can't silently desync
    // measurement from what's actually drawn.
    let rows = u16::try_from(build_rows(snapshot, 0, false).len()).unwrap_or(u16::MAX);
    2u16.saturating_add(rows)
}

/// Render the plan view widget in the given area.
///
/// When `metrics.orchestration_graph` is `None`, renders a placeholder paragraph.
/// When it contains a snapshot, renders a table with per-task rows.
/// Render the plan view widget in the given area.
///
/// When `metrics.orchestration_graph` is `None`, renders a placeholder paragraph.
/// When it contains a snapshot, renders a table with per-task rows showing status,
/// name, and a spinner for running tasks.
///
/// # Arguments
///
/// * `metrics` — current metrics snapshot.
/// * `frame` — ratatui frame for widget rendering.
/// * `area` — terminal rect to render into.
/// * `tick` — current animation tick.
/// * `ascii` — when `true`, uses ASCII-only spinner frames for terminals without Unicode support.
pub fn render(
    metrics: &MetricsSnapshot,
    frame: &mut Frame,
    area: Rect,
    tick: u8,
    ascii: bool,
    theme: &crate::theme::Theme,
) {
    let Some(ref snapshot) = metrics.orchestration_graph else {
        render_placeholder(frame, area, theme);
        return;
    };

    // Stale snapshots (completed/failed/canceled >30s ago) show as empty.
    if snapshot.is_stale() {
        render_placeholder(frame, area, theme);
        return;
    }

    let any_running = snapshot.tasks.iter().any(|t| t.status == "running");

    let status_tag = match snapshot.status.as_str() {
        "created" => "pending",
        "running" => {
            if any_running {
                breeze_frame(u64::from(tick), ascii)
            } else {
                "running"
            }
        }
        "completed" => "completed",
        "failed" => "failed",
        "paused" => "paused",
        "canceled" => "canceled",
        _ => "active",
    };
    let goal_short = truncate_goal(&snapshot.goal, 30);
    let header_text = format!("plan · {status_tag} · {goal_short}");
    let header_style = if any_running {
        theme
            .system_message
            .add_modifier(Modifier::BOLD)
            .fg(Color::Yellow)
    } else {
        theme.system_message.add_modifier(Modifier::BOLD)
    };
    let header = Line::from(Span::styled(header_text, header_style));
    let splits = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    frame.render_widget(Paragraph::new(header), splits[0]);

    let widths = [
        Constraint::Length(4),  // spinner or status icon (3 cells + 1 padding)
        Constraint::Length(3),  // id
        Constraint::Fill(1),    // title
        Constraint::Length(10), // status
        Constraint::Length(12), // agent
        Constraint::Length(8),  // duration
    ];

    let col_header = Row::new([
        Cell::from(""),
        Cell::from("#").style(Style::default().fg(Color::DarkGray)),
        Cell::from("Title").style(Style::default().fg(Color::DarkGray)),
        Cell::from("Status").style(Style::default().fg(Color::DarkGray)),
        Cell::from("Agent").style(Style::default().fg(Color::DarkGray)),
        Cell::from("ms").style(Style::default().fg(Color::DarkGray)),
    ]);

    let mut rows = build_rows(snapshot, tick, ascii);

    // The `Table` widget's own column header consumes 1 row of `splits[1]`; the rest is
    // where `rows` render. `Table` silently drops rows beyond that budget with no visual
    // cue, so — matching `widgets::panel::render_lines`'s overflow convention (#6675 M2) —
    // truncate ourselves and replace the last visible row with a `+N more` indicator.
    let capacity = usize::from(splits[1].height.saturating_sub(1));
    if rows.len() > capacity {
        let hidden = rows.len() - capacity + 1;
        rows.truncate(capacity);
        if let Some(last) = rows.last_mut() {
            *last = Row::new([
                Cell::from(""),
                Cell::from(""),
                Cell::from(format!("+{hidden} more")).style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ]);
        }
    }

    let table = Table::new(rows, widths)
        .header(col_header)
        .column_spacing(1);

    frame.render_widget(table, splits[1]);
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
                    handoff_rejected: None,
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
                render(
                    metrics,
                    frame,
                    area,
                    0,
                    false,
                    &crate::theme::Theme::default(),
                );
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
    fn desired_height_placeholder_is_two() {
        let metrics = MetricsSnapshot::default();
        assert_eq!(desired_height(&metrics), 2);
    }

    #[test]
    fn desired_height_matches_header_plus_column_header_plus_task_count() {
        let metrics = MetricsSnapshot {
            orchestration_graph: Some(make_snapshot(
                "running",
                vec![("Task Alpha", "pending"), ("Task Beta", "running")],
            )),
            ..MetricsSnapshot::default()
        };
        assert_eq!(desired_height(&metrics), 4);
    }

    #[test]
    fn desired_height_stale_snapshot_is_placeholder_height() {
        let mut metrics = MetricsSnapshot::default();
        let mut snap = make_snapshot("completed", vec![("Task", "completed")]);
        snap.completed_at = Some(
            std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(31))
                .unwrap(),
        );
        metrics.orchestration_graph = Some(snap);
        assert_eq!(desired_height(&metrics), 2);
    }

    #[test]
    fn desired_height_matches_actual_row_count_render_builds() {
        // #6675 M1: desired_height must derive from the same build_rows() render() uses,
        // not an independently-maintained count.
        let metrics = MetricsSnapshot {
            orchestration_graph: Some(make_snapshot(
                "running",
                vec![("A", "pending"), ("B", "running"), ("C", "completed")],
            )),
            ..MetricsSnapshot::default()
        };
        let Some(ref snapshot) = metrics.orchestration_graph else {
            unreachable!()
        };
        let actual_rows = super::build_rows(snapshot, 0, false).len();
        assert_eq!(
            desired_height(&metrics),
            2 + u16::try_from(actual_rows).unwrap()
        );
    }

    #[test]
    fn render_shows_overflow_indicator_when_tasks_exceed_area() {
        // #6675 M2: plan_view's Table must not silently clip rows under pressure.
        let metrics = MetricsSnapshot {
            orchestration_graph: Some(make_snapshot(
                "running",
                vec![
                    ("Task One", "pending"),
                    ("Task Two", "running"),
                    ("Task Three", "completed"),
                    ("Task Four", "pending"),
                    ("Task Five", "pending"),
                ],
            )),
            ..MetricsSnapshot::default()
        };
        // Outer header (1) + table column header (1) + only 2 data rows worth of space.
        let output = crate::test_utils::render_to_string(80, 4, |frame, area| {
            render(
                &metrics,
                frame,
                area,
                0,
                false,
                &crate::theme::Theme::default(),
            );
        });
        assert!(
            output.contains("more"),
            "overflowing task list must show an indicator, got:\n{output}"
        );
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
    fn handoff_rejected_task_shows_suffix_in_title() {
        // #6390: a rejected Command handoff has no dedicated display surface today —
        // it must render as a title suffix even though the task itself stays "completed".
        let mut snap = make_snapshot("running", vec![("Router Task", "completed")]);
        snap.tasks[0].handoff_rejected = Some("goto target already completed".to_owned());
        let metrics = MetricsSnapshot {
            orchestration_graph: Some(snap),
            ..MetricsSnapshot::default()
        };
        let rendered = render_to_buffer(&metrics);
        assert!(
            rendered.contains("handoff rejected"),
            "expected rejection suffix in rendered output, got: {rendered:?}"
        );
    }

    #[test]
    fn failed_task_error_suffix_takes_precedence_over_handoff_rejected() {
        // Both fields CAN be set on the same node in practice: `try_handoff` sets
        // `handoff_rejected` on a still-`Completed` node (spec-080), and issue #6394's
        // post-hoc `CheckToolOutcome` correction can independently flip that same node's
        // status to `failed` afterward (code review Finding 3, 2026-07-17) — so this is
        // not a hypothetical precedence rule, it is a reachable state. The failed-error
        // branch stays the first match so a genuinely failed task's error is never masked.
        // Note this creates a deliberate CLI/TUI display divergence: `format_plan_status`
        // (zeph-core/agent/plan.rs) lists `handoff_rejected` for every task regardless of
        // status, so the CLI still surfaces the rejection reason in this state — only the
        // TUI title suffix drops it in favor of the (more actionable) failure reason.
        let mut snap = make_snapshot("running", vec![("Router Task", "failed")]);
        snap.tasks[0].error = Some("boom".to_owned());
        snap.tasks[0].handoff_rejected = Some("goto target already completed".to_owned());
        let metrics = MetricsSnapshot {
            orchestration_graph: Some(snap),
            ..MetricsSnapshot::default()
        };
        let rendered = render_to_buffer(&metrics);
        assert!(
            rendered.contains("[boom]"),
            "expected error suffix, got: {rendered:?}"
        );
        assert!(!rendered.contains("handoff rejected"));
    }

    #[test]
    fn breeze_frame_cycles_in_plan_view() {
        use crate::widgets::spinner::breeze_frame;
        // All 6 breeze frames are reachable from tick 0..5.
        let frames: Vec<&str> = (0u8..6)
            .map(|t| breeze_frame(u64::from(t), false))
            .collect();
        assert_eq!(frames.len(), 6);
        assert_eq!(frames[0], "▹▹▹");
        assert_eq!(frames[3], "▸▸▸");
    }
}
