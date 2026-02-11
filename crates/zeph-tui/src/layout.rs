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
}
