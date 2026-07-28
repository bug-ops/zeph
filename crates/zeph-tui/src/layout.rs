// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Number of side-panel slots: Skills, Memory, Resources, `SubAgents`.
const PANEL_SLOTS: usize = 4;

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

/// How much vertical space a side-panel slot wants this frame.
///
/// Computed once per frame from [`crate::metrics::MetricsSnapshot`] content plus the chrome
/// each slot's renderer adds (focused-panel header row, resources' gauge + compaction badge,
/// the subagents equalizer). Must be a pure function of content — **never** of the allocated
/// [`Rect`]; sizing a slot from its own rendered area would create a layout feedback loop that
/// oscillates frame to frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelDemand {
    /// User-pinned to its single summary row (see `App::toggle_panel_collapse`), regardless
    /// of content.
    Collapsed,
    /// Wants exactly `rows` rows; anything beyond is wasted space.
    Rows(u16),
    /// Wants every row it can get (overlays, wrapped or scrollable views).
    Greedy,
}

impl Default for PanelDemand {
    /// Defaults to [`PanelDemand::Greedy`]. Four `Greedy` demands give every slot the same
    /// *total* the pre-#6675 equal-`Fill(1)` split did, and [`PanelDemand::Collapsed`]
    /// reproduces the old `Length(1)` collapse behavior exactly — but `fit_panel_heights`'
    /// top-down remainder placement is not pixel-identical to ratatui's cassowary solver
    /// (which spreads remainder rows toward the middle slots rather than the topmost ones);
    /// see [`fit_panel_heights`]'s own docs for the exact remainder rule.
    fn default() -> Self {
        Self::Greedy
    }
}

/// Per-frame sizing request for all four side-panel slots.
///
/// Built once per frame (see `App::panel_demands`) and passed to [`AppLayout::compute`].
#[derive(Debug, Clone, Copy)]
pub struct PanelSizing {
    /// Demand for each slot, in `[skills, memory, resources, subagents]` order.
    pub demands: [PanelDemand; PANEL_SLOTS],
    /// Slot index that receives rounding remainders first when space is under pressure.
    pub focus: Option<usize>,
}

impl Default for PanelSizing {
    fn default() -> Self {
        Self {
            demands: [PanelDemand::Greedy; PANEL_SLOTS],
            focus: None,
        }
    }
}

/// Upper bound on rows a single slot's demand can absorb.
fn demand_cap(demand: PanelDemand) -> u32 {
    match demand {
        PanelDemand::Collapsed => 1,
        PanelDemand::Rows(rows) => u32::from(rows),
        PanelDemand::Greedy => u32::MAX,
    }
}

/// Integer max-min fair water-filling allocator for the four side-panel slots.
///
/// Distributes `available` rows across `sizing.demands` so that no slot is granted more
/// than it asked for, every visible slot gets a floor of one row (identity row / mouse hit
/// target / collapse affordance) whenever `available >= 4`, and any surplus beyond total
/// demand is left unallocated as trailing blank space at the bottom of the column — donating
/// it to chat is not geometrically possible, since chat is a horizontal sibling that already
/// spans the full band height.
///
/// A slot demanding [`PanelDemand::Rows`]`(0)` is the one exception to the floor guarantee:
/// granting it a row would violate `granted <= demand`, so it is skipped entirely rather than
/// padded — this only matters for a slot with genuinely zero content to show.
///
/// When `available < 4` there isn't room for one identity row per slot; rows are handed out
/// top-down (skipping any slot whose demand is zero) until either `available` or the slot
/// list is exhausted.
///
/// Uses `u32` intermediates so `PanelDemand::Rows(u16::MAX)` cannot overflow the arithmetic.
#[must_use]
pub fn fit_panel_heights(sizing: &PanelSizing, available: u16) -> [u16; PANEL_SLOTS] {
    let caps: [u32; PANEL_SLOTS] = core::array::from_fn(|i| demand_cap(sizing.demands[i]));

    if available < 4 {
        let mut out = [0u16; PANEL_SLOTS];
        let mut left = available;
        for (cap, slot) in caps.iter().zip(out.iter_mut()) {
            if left == 0 {
                break;
            }
            if *cap >= 1 {
                *slot = 1;
                left -= 1;
            }
        }
        return out;
    }

    let available = u32::from(available);

    // Floor stage: every slot whose demand allows it gets its one guaranteed row.
    let mut grant = [0u32; PANEL_SLOTS];
    let mut cap_left = [0u32; PANEL_SLOTS];
    for i in 0..PANEL_SLOTS {
        if caps[i] >= 1 {
            grant[i] = 1;
        }
        cap_left[i] = caps[i].saturating_sub(grant[i]);
    }
    let mut remaining = available.saturating_sub(grant.iter().sum());
    let mut active: Vec<usize> = (0..PANEL_SLOTS).filter(|&i| cap_left[i] > 0).collect();

    while remaining > 0 && !active.is_empty() {
        let active_len = u32::try_from(active.len()).unwrap_or(u32::MAX);
        let share = remaining / active_len;
        if share == 0 {
            distribute_remainder(remaining, &active, sizing.focus, &mut grant, &mut cap_left);
            break;
        }
        let mut used = 0u32;
        let mut next_active = Vec::with_capacity(active.len());
        for &i in &active {
            let take = share.min(cap_left[i]);
            grant[i] += take;
            cap_left[i] -= take;
            used += take;
            if cap_left[i] > 0 {
                next_active.push(i);
            }
        }
        remaining -= used;
        active = next_active;
    }

    core::array::from_fn(|i| u16::try_from(grant[i]).unwrap_or(u16::MAX))
}

/// Distribute a residual `remaining < active.len()` rows one at a time: `focus` first (when
/// still active), then the rest of `active` in ascending (top-down) index order.
fn distribute_remainder(
    mut remaining: u32,
    active: &[usize],
    focus: Option<usize>,
    grant: &mut [u32; PANEL_SLOTS],
    cap_left: &mut [u32; PANEL_SLOTS],
) {
    let mut order: Vec<usize> = Vec::with_capacity(active.len());
    if let Some(f) = focus
        && active.contains(&f)
    {
        order.push(f);
    }
    for &i in active {
        if Some(i) != focus {
            order.push(i);
        }
    }
    for i in order {
        if remaining == 0 {
            break;
        }
        if cap_left[i] > 0 {
            grant[i] += 1;
            cap_left[i] -= 1;
            remaining -= 1;
        }
    }
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
/// use zeph_tui::layout::{AppLayout, PanelSizing};
///
/// let area = Rect::new(0, 0, 120, 40);
/// let layout = AppLayout::compute(area, true, 3, PanelSizing::default());
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
    /// * `panels` — per-slot sizing demand, resolved into concrete row counts by
    ///   [`fit_panel_heights`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use ratatui::layout::Rect;
    /// use zeph_tui::layout::{AppLayout, PanelDemand, PanelSizing};
    ///
    /// // Wide terminal: side panels visible.
    /// let layout = AppLayout::compute(Rect::new(0, 0, 120, 40), true, 3, PanelSizing::default());
    /// assert!(layout.side_panel.width > 0);
    ///
    /// // Narrow terminal: side panels hidden.
    /// let layout = AppLayout::compute(Rect::new(0, 0, 60, 24), true, 3, PanelSizing::default());
    /// assert_eq!(layout.side_panel.width, 0);
    ///
    /// // All panels collapsed: each gets a single summary row.
    /// let collapsed = PanelSizing {
    ///     demands: [PanelDemand::Collapsed; 4],
    ///     focus: None,
    /// };
    /// let layout = AppLayout::compute(Rect::new(0, 0, 120, 40), true, 3, collapsed);
    /// assert!(layout.side_panel.width > 0);
    /// assert_eq!(layout.skills.height, 1);
    /// ```
    #[must_use]
    pub fn compute(
        area: Rect,
        show_side_panels: bool,
        input_height: u16,
        panels: PanelSizing,
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

        let side_area = main_split[2];
        let heights = fit_panel_heights(&panels, side_area.height);

        // Direct y-offset arithmetic rather than a second Layout::split: per-frame-varying
        // Length constraints are the worst case for ratatui's internal layout cache.
        let mut y = side_area.y;
        let mut side_rects = [Rect::default(); PANEL_SLOTS];
        for (rect, height) in side_rects.iter_mut().zip(heights) {
            *rect = Rect {
                x: side_area.x,
                y,
                width: side_area.width,
                height,
            };
            y = y.saturating_add(height);
        }

        Self {
            header: outer[0],
            chat: main_split[0],
            separator: main_split[1],
            side_panel: side_area,
            skills: side_rects[0],
            memory: side_rects[1],
            resources: side_rects[2],
            subagents: side_rects[3],
            input: outer[2],
            status: outer[3],
        }
    }
}

#[cfg(test)]
mod tests {
    use unicode_width::UnicodeWidthStr;

    use super::*;

    fn sizing(demands: [PanelDemand; PANEL_SLOTS]) -> PanelSizing {
        PanelSizing {
            demands,
            focus: None,
        }
    }

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
        let layout = AppLayout::compute(area, true, 3, PanelSizing::default());
        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.input.height, 3);
        assert_eq!(layout.status.height, 1);
        assert!(layout.chat.width > layout.side_panel.width);
    }

    #[test]
    fn layout_for_small_terminal() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = AppLayout::compute(area, true, 3, PanelSizing::default());
        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.status.height, 1);
        assert!(layout.chat.height >= 10);
    }

    #[test]
    fn layout_side_panels_stack_vertically() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, true, 3, PanelSizing::default());
        assert!(layout.skills.y < layout.memory.y);
        assert!(layout.memory.y < layout.resources.y);
        assert!(layout.resources.y < layout.subagents.y);
    }

    #[test]
    fn layout_input_below_chat() {
        let area = Rect::new(0, 0, 100, 30);
        let layout = AppLayout::compute(area, true, 3, PanelSizing::default());
        assert!(layout.input.y > layout.chat.y);
        assert!(layout.status.y > layout.input.y);
    }

    #[test]
    fn layout_narrow_hides_side_panels() {
        let area = Rect::new(0, 0, 60, 24);
        let layout = AppLayout::compute(area, true, 3, PanelSizing::default());
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
        let layout = AppLayout::compute(area, true, 3, PanelSizing::default());
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
    }

    #[test]
    fn layout_boundary_at_80_shows_side_panels() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = AppLayout::compute(area, true, 3, PanelSizing::default());
        assert!(layout.side_panel.width > 0);
        assert!(layout.skills.width > 0);
    }

    #[test]
    fn layout_boundary_at_79_hides_side_panels() {
        let area = Rect::new(0, 0, 79, 24);
        let layout = AppLayout::compute(area, true, 3, PanelSizing::default());
        assert_eq!(layout.side_panel, Rect::default());
    }

    #[test]
    fn layout_toggle_off_hides_side_panels() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, false, 3, PanelSizing::default());
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
        let layout = AppLayout::compute(area, true, 3, PanelSizing::default());
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
        let layout = AppLayout::compute(
            area,
            true,
            3,
            sizing([
                PanelDemand::Collapsed,
                PanelDemand::Greedy,
                PanelDemand::Greedy,
                PanelDemand::Greedy,
            ]),
        );
        assert_eq!(layout.skills.height, 1, "collapsed skills must be height 1");
        assert!(
            layout.memory.height > 1,
            "expanded memory must be taller than 1"
        );
    }

    #[test]
    fn all_panels_collapsed_no_panic_and_each_height_one() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, true, 3, sizing([PanelDemand::Collapsed; 4]));
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
        let layout = AppLayout::compute(area, true, 3, sizing([PanelDemand::Collapsed; 4]));
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
    }

    #[test]
    fn single_expanded_panel_fills_remaining_height() {
        let area = Rect::new(0, 0, 120, 40);
        // Collapse first three, only subagents expanded.
        let layout = AppLayout::compute(
            area,
            true,
            3,
            sizing([
                PanelDemand::Collapsed,
                PanelDemand::Collapsed,
                PanelDemand::Collapsed,
                PanelDemand::Greedy,
            ]),
        );
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
            let demands = c.map(|collapsed| {
                if collapsed {
                    PanelDemand::Collapsed
                } else {
                    PanelDemand::Greedy
                }
            });
            let layout = AppLayout::compute(area, true, 3, sizing(demands));
            // All rects must be within terminal bounds.
            assert!(layout.skills.y + layout.skills.height <= area.height);
            assert!(layout.subagents.y + layout.subagents.height <= area.height);
        }
    }

    // ── fit_panel_heights unit table ─────────────────────────────────────────

    #[test]
    fn fit_all_greedy_splits_into_equal_totals_when_evenly_divisible() {
        let s = sizing([PanelDemand::Greedy; 4]);
        assert_eq!(fit_panel_heights(&s, 40), [10, 10, 10, 10]);
    }

    #[test]
    fn fit_all_greedy_remainder_placement_is_top_down_not_cassowary_middle_out() {
        // #6675 S3: four Greedy demands reproduce the pre-#6675 Fill(1) split's *total*
        // (5 rows split across 4 slots), but NOT its exact per-slot remainder placement.
        // Ratatui's cassowary solver spreads the remainder toward the middle slots
        // (observed: [1, 2, 1, 1]); this allocator's remainder rule is top-down instead
        // (no `focus`, so the first slot(s) in iteration order absorb it first).
        let s = sizing([PanelDemand::Greedy; 4]);
        let granted = fit_panel_heights(&s, 5);
        assert_eq!(granted.iter().sum::<u16>(), 5, "total must still match");
        assert_eq!(
            granted,
            [2, 1, 1, 1],
            "remainder goes to the first slot top-down, unlike cassowary's middle-out [1,2,1,1]"
        );
    }

    #[test]
    fn fit_all_collapsed_caps_at_one_each() {
        let s = sizing([PanelDemand::Collapsed; 4]);
        assert_eq!(fit_panel_heights(&s, 40), [1, 1, 1, 1]);
    }

    #[test]
    fn fit_surplus_beyond_total_demand_left_unallocated() {
        let s = sizing([
            PanelDemand::Rows(2),
            PanelDemand::Rows(2),
            PanelDemand::Rows(2),
            PanelDemand::Rows(2),
        ]);
        let granted = fit_panel_heights(&s, 40);
        assert_eq!(granted, [2, 2, 2, 2]);
        assert!(granted.iter().sum::<u16>() < 40);
    }

    #[test]
    fn fit_exact_fit_grants_exactly_demand() {
        let s = sizing([
            PanelDemand::Rows(3),
            PanelDemand::Rows(5),
            PanelDemand::Rows(2),
            PanelDemand::Rows(6),
        ]);
        assert_eq!(fit_panel_heights(&s, 16), [3, 5, 2, 6]);
    }

    #[test]
    fn fit_pressure_below_floor_hands_out_one_row_top_down() {
        let s = sizing([PanelDemand::Greedy; 4]);
        assert_eq!(fit_panel_heights(&s, 0), [0, 0, 0, 0]);
        assert_eq!(fit_panel_heights(&s, 1), [1, 0, 0, 0]);
        assert_eq!(fit_panel_heights(&s, 2), [1, 1, 0, 0]);
        assert_eq!(fit_panel_heights(&s, 3), [1, 1, 1, 0]);
    }

    #[test]
    fn fit_available_exactly_four_gives_floor_of_one_to_every_slot() {
        // #6675 tester gap 5: available == 4 is the exact boundary between the "hand out
        // one row top-down" branch (available < 4) and the normal floor-then-water-fill
        // branch (available >= 4) — pin it as its own explicit case rather than relying on
        // proptest ranges to happen to cover it.
        let s = sizing([PanelDemand::Greedy; 4]);
        assert_eq!(fit_panel_heights(&s, 4), [1, 1, 1, 1]);
    }

    #[test]
    fn fit_pressure_below_floor_skips_zero_demand_slots() {
        let s = sizing([
            PanelDemand::Rows(0),
            PanelDemand::Greedy,
            PanelDemand::Greedy,
            PanelDemand::Greedy,
        ]);
        // Slot 0 has nothing to show; its row goes to slot 1 instead.
        assert_eq!(fit_panel_heights(&s, 2), [0, 1, 1, 0]);
    }

    #[test]
    fn fit_mixed_measured_and_greedy_gives_surplus_to_greedy() {
        let s = sizing([
            PanelDemand::Rows(3),
            PanelDemand::Greedy,
            PanelDemand::Rows(2),
            PanelDemand::Collapsed,
        ]);
        let granted = fit_panel_heights(&s, 20);
        assert_eq!(granted[0], 3, "measured slot capped at its demand");
        assert_eq!(granted[2], 2, "measured slot capped at its demand");
        assert_eq!(granted[3], 1, "collapsed slot capped at one row");
        assert_eq!(granted[1], 20 - 3 - 2 - 1, "greedy slot absorbs the rest");
    }

    #[test]
    fn fit_rows_zero_demand_not_padded_to_floor() {
        let s = sizing([PanelDemand::Rows(0); 4]);
        assert_eq!(fit_panel_heights(&s, 40), [0, 0, 0, 0]);
    }

    #[test]
    fn fit_rows_u16_max_does_not_overflow() {
        let s = sizing([PanelDemand::Rows(u16::MAX); 4]);
        let granted = fit_panel_heights(&s, 40);
        assert_eq!(granted.iter().sum::<u16>(), 40);
    }

    #[test]
    fn fit_focus_gets_remainder_first() {
        // 4 greedy slots sharing 10 rows: 10/4 = 2 rem 2 -> two slots get an extra row.
        let s = PanelSizing {
            demands: [PanelDemand::Greedy; 4],
            focus: Some(2),
        };
        let granted = fit_panel_heights(&s, 10);
        assert_eq!(granted.iter().sum::<u16>(), 10);
        assert_eq!(
            granted[2], 3,
            "focused slot must receive the first extra row"
        );
    }

    #[test]
    fn fit_out_of_range_focus_does_not_panic() {
        let s = PanelSizing {
            demands: [PanelDemand::Greedy; 4],
            focus: Some(99),
        };
        let granted = fit_panel_heights(&s, 10);
        assert_eq!(granted.iter().sum::<u16>(), 10);
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

        fn arb_demand() -> impl Strategy<Value = PanelDemand> {
            prop_oneof![
                Just(PanelDemand::Collapsed),
                Just(PanelDemand::Greedy),
                any::<u16>().prop_map(PanelDemand::Rows),
            ]
        }

        fn arb_sizing() -> impl Strategy<Value = PanelSizing> {
            (
                arb_demand(),
                arb_demand(),
                arb_demand(),
                arb_demand(),
                proptest::option::of(0usize..8),
            )
                .prop_map(|(d0, d1, d2, d3, focus)| PanelSizing {
                    demands: [d0, d1, d2, d3],
                    focus,
                })
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
                let demands = [c0, c1, c2, c3].map(|collapsed| {
                    if collapsed { PanelDemand::Collapsed } else { PanelDemand::Greedy }
                });
                let layout = AppLayout::compute(area, show_side, 3, sizing(demands));

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

            #[test]
            fn fit_panel_heights_never_panics(
                sizing in arb_sizing(),
                available in 0u16..2000,
            ) {
                let _ = fit_panel_heights(&sizing, available);
            }

            #[test]
            fn fit_panel_heights_sum_never_exceeds_available(
                sizing in arb_sizing(),
                available in 0u16..2000,
            ) {
                let granted = fit_panel_heights(&sizing, available);
                let total: u32 = granted.iter().map(|&h| u32::from(h)).sum();
                prop_assert!(total <= u32::from(available));
            }

            #[test]
            fn fit_panel_heights_respects_rows_demand_cap(
                r0 in any::<u16>(), r1 in any::<u16>(), r2 in any::<u16>(), r3 in any::<u16>(),
                available in 0u16..2000,
            ) {
                let s = sizing([
                    PanelDemand::Rows(r0),
                    PanelDemand::Rows(r1),
                    PanelDemand::Rows(r2),
                    PanelDemand::Rows(r3),
                ]);
                let granted = fit_panel_heights(&s, available);
                prop_assert!(granted[0] <= r0);
                prop_assert!(granted[1] <= r1);
                prop_assert!(granted[2] <= r2);
                prop_assert!(granted[3] <= r3);
            }

            #[test]
            fn fit_panel_heights_collapsed_never_exceeds_one(
                available in 4u16..2000,
                d1 in arb_demand(), d2 in arb_demand(), d3 in arb_demand(),
            ) {
                let s = sizing([PanelDemand::Collapsed, d1, d2, d3]);
                let granted = fit_panel_heights(&s, available);
                prop_assert!(granted[0] <= 1);
            }

            #[test]
            fn fit_panel_heights_floor_of_one_when_demand_allows(
                available in 4u16..2000,
                sizing in arb_sizing(),
            ) {
                let granted = fit_panel_heights(&sizing, available);
                for (i, &demand) in sizing.demands.iter().enumerate() {
                    if demand_cap(demand) >= 1 {
                        prop_assert!(
                            granted[i] >= 1,
                            "slot {i} with non-zero demand must get its floor row"
                        );
                    }
                }
            }

            #[test]
            fn fit_panel_heights_monotone_in_available(
                sizing in arb_sizing(),
                base in 0u16..1000,
                delta in 0u16..1000,
            ) {
                let low = fit_panel_heights(&sizing, base);
                let high = fit_panel_heights(&sizing, base.saturating_add(delta));
                for (h, l) in high.iter().zip(low.iter()) {
                    prop_assert!(h >= l);
                }
            }

            #[test]
            fn fit_panel_heights_monotone_in_own_demand(
                d1 in arb_demand(), d2 in arb_demand(), d3 in arb_demand(),
                r_low in any::<u16>(),
                grow in any::<u16>(),
                available in 0u16..2000,
            ) {
                let r_high = r_low.saturating_add(grow);
                let low = sizing([PanelDemand::Rows(r_low), d1, d2, d3]);
                let high = sizing([PanelDemand::Rows(r_high), d1, d2, d3]);
                let granted_low = fit_panel_heights(&low, available);
                let granted_high = fit_panel_heights(&high, available);
                prop_assert!(granted_high[0] >= granted_low[0]);
            }
        }
    }
}
