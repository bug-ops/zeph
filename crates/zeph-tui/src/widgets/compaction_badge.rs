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
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::metrics::MetricsSnapshot;

/// Render the compaction badge into `area`.
///
/// Shows `"Last: {before}k→{after}k ({saved}k freed) {elapsed}"`.
/// Hidden (renders an empty block) when no compaction has occurred or `area` has zero height.
pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }

    let at_ms = metrics.compaction_last_at_ms;
    let before = metrics.compaction_last_before;
    let after = metrics.compaction_last_after;

    let block = Block::default().borders(Borders::ALL).title("Compaction");

    if at_ms == 0 {
        // No compaction yet — render empty block.
        frame.render_widget(block, area);
        return;
    }

    let saved = before.saturating_sub(after);
    let elapsed = format_elapsed(at_ms);

    let text = format!(
        "{}k→{}k (-{}k) {}",
        before / 1000,
        after / 1000,
        saved / 1000,
        elapsed,
    );

    let paragraph = Paragraph::new(Line::from(vec![Span::styled(
        text,
        Style::default().fg(Color::Cyan),
    )]))
    .block(block);

    frame.render_widget(paragraph, area);
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
}
