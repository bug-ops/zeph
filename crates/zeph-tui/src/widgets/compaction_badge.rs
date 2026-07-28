// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Persistent compaction badge for the TUI side panel.
//!
//! Unlike the ephemeral `send_status` line that gets overwritten, this badge reads from the
//! `compaction_last_*` metric fields and remains visible across turns — giving the user a
//! persistent record of when the last compaction happened and how many tokens were freed.
//!
//! Hidden when `compaction_last_at_ms == 0` (no compaction has occurred this session).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

/// Number of rows the compaction badge needs: `0` when no compaction has occurred this
/// session (`compaction_last_at_ms == 0`), `1` otherwise.
///
/// Pure function of `metrics` — never of the allocated `Rect` — matching [`render`]'s own
/// hidden-when-no-compaction rule so the two can never disagree.
#[must_use]
pub fn desired_height(metrics: &MetricsSnapshot) -> u16 {
    u16::from(metrics.compaction_last_at_ms != 0)
}

/// Render the compaction badge into `area`.
///
/// Shows `compaction  {before}k→{after}k (-{saved}k)  {elapsed}` on a single line.
/// Renders nothing when no compaction has occurred or `area` has zero height.
pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect, theme: &Theme) {
    if area.height == 0 || metrics.compaction_last_at_ms == 0 {
        return;
    }

    let before = metrics.compaction_last_before;
    let after = metrics.compaction_last_after;
    let saved = before.saturating_sub(after);
    let elapsed = format_elapsed(metrics.compaction_last_at_ms);

    let detail = format!(
        "{}k→{}k (-{}k)  {elapsed}",
        before / 1000,
        after / 1000,
        saved / 1000,
    );

    let line = Line::from(vec![
        Span::styled("compaction  ", theme.system_message),
        Span::styled(detail, theme.status_bar),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

/// Format elapsed time since `at_ms` (Unix epoch ms) as a human-readable string.
///
/// Returns strings like `"3s ago"`, `"2m ago"`, `"1h ago"`.
/// Returns `"?"` when system time is unavailable or `at_ms` is in the future.
fn format_elapsed(at_ms: u64) -> String {
    // u128 → u64: safe until year 584 million; truncation is acceptable for display.
    #[allow(clippy::cast_possible_truncation)]
    let now_ms = std::time::SystemTime::UNIX_EPOCH
        .elapsed()
        .map_or(0, |d| d.as_millis() as u64);

    let elapsed_secs = now_ms.saturating_sub(at_ms) / 1000;

    if elapsed_secs < 60 {
        format!("{elapsed_secs}s ago")
    } else if elapsed_secs < 3600 {
        format!("{}m ago", elapsed_secs / 60)
    } else {
        format!("{}h ago", elapsed_secs / 3600)
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    #[test]
    fn format_elapsed_seconds() {
        // Use a timestamp 30 seconds ago.
        let now_ms = std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map_or(0, |d| d.as_millis() as u64);
        let at_ms = now_ms.saturating_sub(30_000);
        let s = format_elapsed(at_ms);
        assert!(s.ends_with("s ago"), "expected seconds ago, got: {s}");
    }

    #[test]
    fn format_elapsed_minutes() {
        let now_ms = std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map_or(0, |d| d.as_millis() as u64);
        let at_ms = now_ms.saturating_sub(2 * 60 * 1000);
        let s = format_elapsed(at_ms);
        assert!(s.ends_with("m ago"), "expected minutes ago, got: {s}");
    }

    #[test]
    fn format_elapsed_hours() {
        let now_ms = std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map_or(0, |d| d.as_millis() as u64);
        let at_ms = now_ms.saturating_sub(2 * 3600 * 1000);
        let s = format_elapsed(at_ms);
        assert!(s.ends_with("h ago"), "expected hours ago, got: {s}");
    }

    #[test]
    fn badge_hidden_when_no_compaction() {
        // compaction_last_at_ms == 0 → at_ms == 0 → early-return in render.
        let m = MetricsSnapshot::default();
        assert_eq!(m.compaction_last_at_ms, 0);
    }

    #[test]
    fn desired_height_zero_when_no_compaction() {
        let m = MetricsSnapshot::default();
        assert_eq!(desired_height(&m), 0);
    }

    #[test]
    fn desired_height_one_when_compaction_occurred() {
        let m = MetricsSnapshot {
            compaction_last_at_ms: 1,
            ..MetricsSnapshot::default()
        };
        assert_eq!(desired_height(&m), 1);
    }
}
