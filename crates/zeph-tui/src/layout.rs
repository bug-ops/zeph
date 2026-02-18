use ratatui::layout::{Constraint, Direction, Layout, Rect};

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

pub struct AppLayout {
    pub header: Rect,
    pub chat: Rect,
    pub side_panel: Rect,
    pub skills: Rect,
    pub memory: Rect,
    pub resources: Rect,
    pub activity: Rect,
    pub input: Rect,
    pub status: Rect,
}

impl AppLayout {
    #[must_use]
    pub fn compute(area: Rect, show_side_panels: bool) -> Self {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(10),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        if !show_side_panels || area.width < 80 {
            return Self {
                header: outer[0],
                chat: outer[1],
                side_panel: Rect::default(),
                skills: Rect::default(),
                memory: Rect::default(),
                resources: Rect::default(),
                activity: outer[2],
                input: outer[3],
                status: outer[4],
            };
        }

        let main_split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(outer[1]);

        let side_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(33),
                Constraint::Percentage(34),
                Constraint::Percentage(33),
            ])
            .split(main_split[1]);

        Self {
            header: outer[0],
            chat: main_split[0],
            side_panel: main_split[1],
            skills: side_split[0],
            memory: side_split[1],
            resources: side_split[2],
            activity: outer[2],
            input: outer[3],
            status: outer[4],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_for_standard_terminal() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, true);
        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.input.height, 3);
        assert_eq!(layout.status.height, 1);
        assert!(layout.chat.width > layout.side_panel.width);
    }

    #[test]
    fn layout_for_small_terminal() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = AppLayout::compute(area, true);
        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.status.height, 1);
        assert!(layout.chat.height >= 10);
    }

    #[test]
    fn layout_side_panels_stack_vertically() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, true);
        assert!(layout.skills.y < layout.memory.y);
        assert!(layout.memory.y < layout.resources.y);
    }

    #[test]
    fn layout_input_below_chat() {
        let area = Rect::new(0, 0, 100, 30);
        let layout = AppLayout::compute(area, true);
        assert!(layout.input.y > layout.chat.y);
        assert!(layout.status.y > layout.input.y);
    }

    #[test]
    fn layout_narrow_hides_side_panels() {
        let area = Rect::new(0, 0, 60, 24);
        let layout = AppLayout::compute(area, true);
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
        assert_eq!(layout.memory, Rect::default());
        assert_eq!(layout.resources, Rect::default());
        assert_eq!(layout.chat.width, area.width);
    }

    #[test]
    fn layout_very_narrow_hides_side_panels() {
        let area = Rect::new(0, 0, 30, 24);
        let layout = AppLayout::compute(area, true);
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
    }

    #[test]
    fn layout_boundary_at_80_shows_side_panels() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = AppLayout::compute(area, true);
        assert!(layout.side_panel.width > 0);
        assert!(layout.skills.width > 0);
    }

    #[test]
    fn layout_boundary_at_79_hides_side_panels() {
        let area = Rect::new(0, 0, 79, 24);
        let layout = AppLayout::compute(area, true);
        assert_eq!(layout.side_panel, Rect::default());
    }

    #[test]
    fn layout_toggle_off_hides_side_panels() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, false);
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
        assert_eq!(layout.memory, Rect::default());
        assert_eq!(layout.resources, Rect::default());
        assert_eq!(layout.chat.width, area.width);
    }

    #[test]
    fn layout_toggle_on_shows_side_panels() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area, true);
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
        let delta = (popup.width as i32 - expected as i32).unsigned_abs();
        assert!(delta <= 2, "width={} expected~={}", popup.width, expected);
    }

    #[test]
    fn centered_rect_is_horizontally_centered() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = centered_rect(70, 10, area);
        let left_margin = popup.x;
        let right_margin = area.width - popup.width - popup.x;
        let diff = (left_margin as i32 - right_margin as i32).unsigned_abs();
        assert!(diff <= 2, "left={left_margin} right={right_margin}");
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
            ) {
                let area = Rect::new(0, 0, width, height);
                let layout = AppLayout::compute(area, show_side);

                assert_within_bounds(layout.header, area);
                assert_within_bounds(layout.chat, area);
                assert_within_bounds(layout.activity, area);
                assert_within_bounds(layout.input, area);
                assert_within_bounds(layout.status, area);

                if layout.side_panel != Rect::default() {
                    assert_within_bounds(layout.side_panel, area);
                    assert_within_bounds(layout.skills, area);
                    assert_within_bounds(layout.memory, area);
                    assert_within_bounds(layout.resources, area);
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
