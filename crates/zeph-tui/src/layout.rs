use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct AppLayout {
    pub header: Rect,
    pub chat: Rect,
    pub side_panel: Rect,
    pub skills: Rect,
    pub memory: Rect,
    pub resources: Rect,
    pub input: Rect,
    pub status: Rect,
}

impl AppLayout {
    #[must_use]
    pub fn compute(area: Rect) -> Self {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(10),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        if area.width < 80 {
            return Self {
                header: outer[0],
                chat: outer[1],
                side_panel: Rect::default(),
                skills: Rect::default(),
                memory: Rect::default(),
                resources: Rect::default(),
                input: outer[2],
                status: outer[3],
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
            input: outer[2],
            status: outer[3],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_for_standard_terminal() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area);
        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.input.height, 3);
        assert_eq!(layout.status.height, 1);
        assert!(layout.chat.width > layout.side_panel.width);
    }

    #[test]
    fn layout_for_small_terminal() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = AppLayout::compute(area);
        assert_eq!(layout.header.height, 1);
        assert_eq!(layout.status.height, 1);
        assert!(layout.chat.height >= 10);
    }

    #[test]
    fn layout_side_panels_stack_vertically() {
        let area = Rect::new(0, 0, 120, 40);
        let layout = AppLayout::compute(area);
        assert!(layout.skills.y < layout.memory.y);
        assert!(layout.memory.y < layout.resources.y);
    }

    #[test]
    fn layout_input_below_chat() {
        let area = Rect::new(0, 0, 100, 30);
        let layout = AppLayout::compute(area);
        assert!(layout.input.y > layout.chat.y);
        assert!(layout.status.y > layout.input.y);
    }

    #[test]
    fn layout_narrow_hides_side_panels() {
        let area = Rect::new(0, 0, 60, 24);
        let layout = AppLayout::compute(area);
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
        assert_eq!(layout.memory, Rect::default());
        assert_eq!(layout.resources, Rect::default());
        assert_eq!(layout.chat.width, area.width);
    }

    #[test]
    fn layout_very_narrow_hides_side_panels() {
        let area = Rect::new(0, 0, 30, 24);
        let layout = AppLayout::compute(area);
        assert_eq!(layout.side_panel, Rect::default());
        assert_eq!(layout.skills, Rect::default());
    }

    #[test]
    fn layout_boundary_at_80_shows_side_panels() {
        let area = Rect::new(0, 0, 80, 24);
        let layout = AppLayout::compute(area);
        assert!(layout.side_panel.width > 0);
        assert!(layout.skills.width > 0);
    }

    #[test]
    fn layout_boundary_at_79_hides_side_panels() {
        let area = Rect::new(0, 0, 79, 24);
        let layout = AppLayout::compute(area);
        assert_eq!(layout.side_panel, Rect::default());
    }
}
