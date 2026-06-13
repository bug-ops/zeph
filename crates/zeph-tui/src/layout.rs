// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncates `s` to fit within `max_width` display columns, appending `…` if truncated.
///
/// Accumulates display width character-by-character using [`UnicodeWidthChar`], reserving
/// one column for the ellipsis. Returns an owned copy of `s` unchanged when it already fits.
pub(crate) fn truncate_to_width(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_owned();
    }
    let budget = max_width.saturating_sub(1); // reserve 1 col for "…"
    let mut out = String::new();
    let mut cols = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if cols + cw > budget {
            break;
        }
        out.push(ch);
        cols += cw;
    }
    out.push('…');
    out
}

/// Returns a centered `Rect` with the given percentage width and fixed height.
#[must_use]
pub fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

/// Pre-computed layout rectangles for all regions of the TUI dashboard.
///
/// Call [`compute`](Self::compute) once per render frame; pass the result to
/// individual widget renderers so each widget knows its exact screen region
/// without re-running the layout algorithm.
///
/// When the terminal is narrower than 80 columns or `show_side_panels` is
/// `false`, all side-panel fields are set to [`Rect::default()`] (zero-sized)
/// and the chat area expands to fill the full width.
///
/// # Examples
///
/// ```rust
/// use ratatui::layout::Rect;
/// use zeph_tui::layout::AppLayout;
///
/// let area = Rect::new(0, 0, 120, 40);
/// let layout = AppLayout::compute(area, true, 3, [false; 4]);
/// assert_eq!(layout.header.height, 1);
/// assert_eq!(layout.status.height, 1);
/// assert!(layout.chat.width > layout.side_panel.width);
/// ```
#[derive(Clone, Copy)]
pub struct AppLayout {
    /// Single-row header bar (model name, session info).
    pub header: Rect,
    /// Main chat / transcript area.
    pub chat: Rect,
    /// One-column vertical separator between chat and side panels (zero when panels hidden).
    pub separator: Rect,
    /// Combined side-panel column (zero when hidden).
    pub side_panel: Rect,
    /// Skills mini-panel within the side column.
    pub skills: Rect,
    /// Memory mini-panel within the side column.
    pub memory: Rect,
    /// MCP resources mini-panel within the side column.
    pub resources: Rect,
    /// Sub-agents mini-panel within the side column.
    pub subagents: Rect,
    /// Multi-row text input box.
    pub input: Rect,
    /// Single-row bottom status bar (metrics, keybinding hints).
    pub status: Rect,
}

impl AppLayout {
    /// Compute the layout for the given terminal area.
    ///
    /// # Arguments
    ///
    /// * `area` — the full terminal rect (from `Frame::area()`).
    /// * `show_side_panels` — `false` hides the side panels regardless of width.
    /// * `input_height` — requested composer height including borders.
    /// * `collapsed` — per-section collapse mask `[skills, memory, resources, subagents]`.
    ///   A collapsed section renders as a single summary row (`Length(1)`); an expanded
    ///   section uses `Fill(1)` to share the remaining space equally.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use ratatui::layout::Rect;
    /// use zeph_tui::layout::AppLayout;
    ///
    /// // Wide terminal: side panels visible.
    /// let layout = AppLayout::compute(Rect::new(0, 0, 120, 40), true, 3, [false; 4]);
    /// assert!(layout.side_panel.width > 0);
    ///
    /// // Narrow terminal: side panels hidden.
    /// let layout = AppLayout::compute(Rect::new(0, 0, 60, 24), true, 3, [false; 4]);
    /// assert_eq!(layout.side_panel.width, 0);
    ///
    /// // All panels collapsed: each gets a single summary row.
    /// let layout = AppLayout::compute(Rect::new(0, 0, 120, 40), true, 3, [true; 4]);
    /// assert!(layout.side_panel.width > 0);
    /// assert_eq!(layout.skills.height, 1);
    /// ```
    #[must_use]
    pub fn compute(
        area: Rect,
        show_side_panels: bool,
        input_height: u16,
        collapsed: [bool; 4],
    ) -> Self {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(10),
                Constraint::Length(input_height),
                Constraint::Length(1),
            ])
            .split(area);

        if !show_side_panels || area.width < 80 {
            return Self {
                header: outer[0],
                chat: outer[1],
                separator: Rect::default(),
                side_panel: Rect::default(),
                skills: Rect::default(),
                memory: Rect::default(),
                resources: Rect::default(),
                subagents: Rect::default(),
                input: outer[2],
                status: outer[3],
            };
        }

        // chat | 1-col separator | side panels
        let main_split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(69),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .split(outer[1]);

        // Each side section is either a single summary row (collapsed) or fills available space.
        // When all four are collapsed the four Length(1) rows sit at the top and the remainder
        // is blank; add a trailing Fill(1) spacer only in that case so the layout is clean.
        let [c0, c1, c2, c3] = collapsed;
        let mut side_constraints: Vec<Constraint> = [c0, c1, c2, c3]
            .iter()
            .map(|&col| {
                if col {
                    Constraint::Length(1)
                } else {
                    Constraint::Fill(1)
                }
            })
            .collect();
        if c0 && c1 && c2 && c3 {
            side_constraints.push(Constraint::Fill(1));
        }
        let side_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints(side_constraints)
            .split(main_split[2]);

        Self {
            header: outer[0],
            chat: main_split[0],
            separator: main_split[1],
            side_panel: main_split[2],
            skills: side_split[0],
            memory: side_split[1],
            resources: side_split[2],
            subagents: side_split[3],
            input: outer[2],
            status: outer[3],
        }
    }
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    use super::*;

    #[test]
    fn truncate_to_width_ascii_fits() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_width_ascii_truncated() {
        let r = truncate_to_width("hello world", 7);
        assert!(r.width() <= 7, "width={}", r.width());
        assert!(r.ends_with('…'));
    }

    #[test]
    fn truncate_to_width_cjk_fits() {
        // "日本語" = 3 chars, width = 6
        assert_eq!(truncate_to_width("日本語", 6), "日本語");
    }

    #[test]
    fn truncate_to_width_cjk_truncated() {
        // "日本語テスト" = 6 chars, width = 12; max=5 → must fit in 5 cols
        let r = truncate_to_width("日本語テスト", 5);
        assert!(r.width() <= 5, "width={} result={r:?}", r.width());
        assert!(r.ends_with('…'));
    }

    #[test]
    fn truncate_to_width_emoji_truncated() {
        // "🎉🎊🎈" = 3 emoji, width = 6; max=5 → must fit in 5 cols
        let r = truncate_to_width("🎉🎊🎈", 5);
        assert!(r.width() <= 5, "width={} result={r:?}", r.width());
        assert!(r.ends_with('…'));
    }

    #[test]
    fn truncate_to_width_exact_boundary() {
        // width == max → no truncation
        let r = truncate_to_width("日本", 4);
        assert_eq!(r, "日本");
    }

    #[test]
    fn truncate_to_width_max_zero() {
        let r = truncate_to_width("hello", 0);
        assert!(r.width() == 1, "only ellipsis: {r:?}"); // "…" = 1 col
    }

    #[test]
    fn layout_for_standard_terminal() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, true, 3, [false; 4]);
        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.input.height, 3);
        assert_eq!(layout.status.height, 1);
        assert!(layout.chat.width > layout.side_panel.width);
    }

    #[test]
    fn layout_for_small_terminal() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = AppLayout::compute(area, true, 3, [false; 4]);
        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.status.height, 1);
        assert!(layout.chat.height >= 10);
    }

    #[test]
    fn layout_side_panels_stack_vertically() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, true, 3, [false; 4]);
        assert!(layout.skills.y < layout.memory.y);
        assert!(layout.memory.y < layout.resources.y);
        assert!(layout.resources.y < layout.subagents.y);
    }

    #[test]
    fn layout_input_below_chat() {
        let area = Rect::new(0, 0, 100, 30);
        let layout = AppLayout::compute(area, true, 3, [false; 4]);
        assert!(layout.input.y > layout.chat.y);
        assert!(layout.status.y > layout.input.y);
    }

    #[test]
    fn layout_narrow_hides_side_panels() {
        let area = Rect::new(0, 0, 60, 24);
        let layout = AppLayout::compute(area, true, 3, [false; 4]);
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
        assert_eq!(layout.memory, Rect::default());
        assert_eq!(layout.resources, Rect::default());
        assert_eq!(layout.subagents, Rect::default());
        assert_eq!(layout.chat.width, area.width);
    }

    #[test]
    fn layout_very_narrow_hides_side_panels() {
        let area = Rect::new(0, 0, 30, 24);
        let layout = AppLayout::compute(area, true, 3, [false; 4]);
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
    }

    #[test]
    fn layout_boundary_at_80_shows_side_panels() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = AppLayout::compute(area, true, 3, [false; 4]);
        assert!(layout.side_panel.width > 0);
        assert!(layout.skills.width > 0);
    }

    #[test]
    fn layout_boundary_at_79_hides_side_panels() {
        let area = Rect::new(0, 0, 79, 24);
        let layout = AppLayout::compute(area, true, 3, [false; 4]);
        assert_eq!(layout.side_panel, Rect::default());
    }

    #[test]
    fn layout_toggle_off_hides_side_panels() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, false, 3, [false; 4]);
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
        assert_eq!(layout.memory, Rect::default());
        assert_eq!(layout.resources, Rect::default());
        assert_eq!(layout.subagents, Rect::default());
        assert_eq!(layout.chat.width, area.width);
    }

    #[test]
    fn layout_toggle_on_shows_side_panels() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, true, 3, [false; 4]);
        assert!(layout.side_panel.width > 0);
        assert!(layout.skills.width > 0);
    }

    #[test]
    fn centered_rect_is_within_area() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = centered_rect(70, 22, area);
        assert!(popup.x >= area.x);
        assert!(popup.y >= area.y);
        assert!(popup.x + popup.width <= area.x + area.width);
        assert!(popup.y + popup.height <= area.y + area.height);
    }

    #[test]
    fn centered_rect_height_matches_requested() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = centered_rect(70, 22, area);
        assert_eq!(popup.height, 22);
    }

    #[test]
    fn centered_rect_width_is_approximately_percent() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = centered_rect(70, 10, area);
        let expected = (100 * 70) / 100;
        let delta = (i32::from(popup.width) - expected).unsigned_abs();
        assert!(delta <= 2, "width={} expected~={}", popup.width, expected);
    }

    #[test]
    fn centered_rect_is_horizontally_centered() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = centered_rect(70, 10, area);
        let left_margin = popup.x;
        let right_margin = area.width - popup.width - popup.x;
        let diff = (i32::from(left_margin) - i32::from(right_margin)).unsigned_abs();
        assert!(diff <= 2, "left={left_margin} right={right_margin}");
    }

    #[test]
    fn collapsed_panel_gets_single_row() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, true, 3, [true, false, false, false]);
        assert_eq!(layout.skills.height, 1, "collapsed skills must be height 1");
        assert!(
            layout.memory.height > 1,
            "expanded memory must be taller than 1"
        );
    }

    #[test]
    fn all_panels_collapsed_no_panic_and_each_height_one() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, true, 3, [true; 4]);
        assert_eq!(layout.skills.height, 1);
        assert_eq!(layout.memory.height, 1);
        assert_eq!(layout.resources.height, 1);
        assert_eq!(layout.subagents.height, 1);
        // All four sections must still be within bounds.
        assert!(layout.skills.y + layout.skills.height <= area.height);
        assert!(layout.subagents.y + layout.subagents.height <= area.height);
    }

    #[test]
    fn narrow_terminal_ignores_collapsed_mask() {
        // width < 80 → side panels hidden regardless of collapse state.
        let area = Rect::new(0, 0, 60, 24);
        let layout = AppLayout::compute(area, true, 3, [true; 4]);
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
    }

    #[test]
    fn single_expanded_panel_fills_remaining_height() {
        let area = Rect::new(0, 0, 120, 40);
        // Collapse first three, only subagents expanded.
        let layout = AppLayout::compute(area, true, 3, [true, true, true, false]);
        assert_eq!(layout.skills.height, 1);
        assert_eq!(layout.memory.height, 1);
        assert_eq!(layout.resources.height, 1);
        assert!(
            layout.subagents.height > 1,
            "sole expanded section must be tall"
        );
    }

    #[test]
    fn collapse_mask_proptest_never_panics() {
        // Manually cover all 16 combinations for a fixed area.
        let area = Rect::new(0, 0, 120, 40);
        for bits in 0u8..16 {
            let c = [
                bits & 0b0001 != 0,
                bits & 0b0010 != 0,
                bits & 0b0100 != 0,
                bits & 0b1000 != 0,
            ];
            let layout = AppLayout::compute(area, true, 3, c);
            // All rects must be within terminal bounds.
            assert!(layout.skills.y + layout.skills.height <= area.height);
            assert!(layout.subagents.y + layout.subagents.height <= area.height);
        }
    }

    mod proptest_layout {
        use super::*;
        use proptest::prelude::*;

        fn assert_within_bounds(rect: Rect, area: Rect) {
            assert!(
                rect.x + rect.width <= area.x + area.width,
                "rect {rect:?} exceeds area width {area:?}"
            );
            assert!(
                rect.y + rect.height <= area.y + area.height,
                "rect {rect:?} exceeds area height {area:?}"
            );
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(1000))]

            #[test]
            fn layout_never_panics(
                width in 1u16..500,
                height in 1u16..500,
                show_side in proptest::bool::ANY,
                c0 in proptest::bool::ANY,
                c1 in proptest::bool::ANY,
                c2 in proptest::bool::ANY,
                c3 in proptest::bool::ANY,
            ) {
                let area = Rect::new(0, 0, width, height);
                let layout = AppLayout::compute(area, show_side, 3, [c0, c1, c2, c3]);

                assert_within_bounds(layout.header, area);
                assert_within_bounds(layout.chat, area);
                assert_within_bounds(layout.input, area);
                assert_within_bounds(layout.status, area);

                if layout.side_panel != Rect::default() {
                    assert_within_bounds(layout.side_panel, area);
                    assert_within_bounds(layout.skills, area);
                    assert_within_bounds(layout.memory, area);
                    assert_within_bounds(layout.resources, area);
                    assert_within_bounds(layout.subagents, area);
                }
            }

            #[test]
            fn centered_rect_within_bounds(
                percent_x in 10u16..100,
                popup_h in 1u16..50,
                area_w in 20u16..300,
                area_h in 10u16..100,
            ) {
                let area = Rect::new(0, 0, area_w, area_h);
                let popup = centered_rect(percent_x, popup_h.min(area_h), area);
                assert_within_bounds(popup, area);
            }
        }
    }
}
