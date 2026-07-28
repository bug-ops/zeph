// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared rendering primitive for measured side-panel widgets (skills, memory, resources,
//! subagents).
//!
//! Content-driven sizing (#6675) means a slot's granted [`Rect`] can be smaller than its
//! `desired_height` under space pressure — every measured widget routes its final render
//! through [`render_lines`] so overflow is signalled consistently instead of silently
//! clipped.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::theme::Theme;

/// Render `lines` into `area`, truncating to the available height.
///
/// When `lines` has more entries than `area.height` can show, `area.height - 1` lines are
/// rendered as-is and the **last visible row** is replaced with a muted `+N more` indicator,
/// where `N` is the total number of lines that are not shown (the truncated lines plus the
/// one whose row was overwritten by the indicator itself). Renders nothing when
/// `area.height == 0`.
pub fn render_lines(frame: &mut Frame, area: Rect, mut lines: Vec<Line<'_>>, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let capacity = usize::from(area.height);
    if lines.len() > capacity {
        // The indicator itself occupies the last visible row, so only `capacity - 1` lines
        // of real content remain visible; everything else — including that bumped line — is
        // hidden.
        let hidden = lines.len() - capacity + 1;
        lines.truncate(capacity);
        if let Some(last) = lines.last_mut() {
            *last = Line::from(Span::styled(
                format!("  +{hidden} more"),
                theme.system_message.add_modifier(Modifier::ITALIC),
            ));
        }
    }
    let para = Paragraph::new(lines).block(Block::default().borders(Borders::NONE));
    frame.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::render_to_string;

    fn theme() -> Theme {
        Theme::default()
    }

    fn lines_of(n: usize) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::from(format!("line {i}"))).collect()
    }

    #[test]
    fn renders_all_lines_when_they_fit() {
        let output = render_to_string(20, 5, |frame, area| {
            render_lines(frame, area, lines_of(3), &theme());
        });
        assert!(output.contains("line 0"));
        assert!(output.contains("line 1"));
        assert!(output.contains("line 2"));
        assert!(!output.contains("more"));
    }

    #[test]
    fn replaces_last_visible_row_with_overflow_indicator() {
        let output = render_to_string(20, 3, |frame, area| {
            render_lines(frame, area, lines_of(5), &theme());
        });
        assert!(output.contains("line 0"));
        assert!(output.contains("line 1"));
        assert!(!output.contains("line 2"));
        assert!(!output.contains("line 4"));
        // 5 lines, 3 rows: 2 visible ("line 0", "line 1") + 1 indicator row → 3 hidden
        // (lines 2, 3, 4), including the row the indicator itself overwrote.
        assert!(output.contains("+3 more"), "got: {output:?}");
    }

    #[test]
    fn exact_fit_shows_no_overflow_indicator() {
        let output = render_to_string(20, 3, |frame, area| {
            render_lines(frame, area, lines_of(3), &theme());
        });
        assert!(!output.contains("more"));
    }

    #[test]
    fn zero_height_area_does_not_panic() {
        render_to_string(20, 0, |frame, area| {
            render_lines(frame, area, lines_of(3), &theme());
        });
    }

    #[test]
    fn single_row_overflow_replaces_only_row() {
        let output = render_to_string(20, 1, |frame, area| {
            render_lines(frame, area, lines_of(4), &theme());
        });
        // 4 lines, 1 row: the only visible row is the indicator itself, so all 4 lines
        // (including "line 0") are hidden.
        assert!(output.contains("+4 more"), "got: {output:?}");
    }

    #[test]
    fn overflow_count_includes_the_row_the_indicator_replaced() {
        // Regression for the off-by-one found in review: 4 lines into 3 rows means 2 lines
        // render as-is and 1 row becomes the indicator, so only 1 line's *text* survives
        // unreplaced beyond the visible ones — but the indicator's own row also counts as
        // hidden content, for a total of 2 hidden lines, not 1.
        let output = render_to_string(20, 3, |frame, area| {
            render_lines(frame, area, lines_of(4), &theme());
        });
        assert!(output.contains("line 0"));
        assert!(output.contains("line 1"));
        assert!(output.contains("+2 more"), "got: {output:?}");
    }
}
