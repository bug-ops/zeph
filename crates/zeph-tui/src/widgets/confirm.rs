use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::layout::centered_rect;
use crate::theme::Theme;

pub fn render(prompt: &str, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let popup = centered_rect(50, 7, area);

    frame.render_widget(Clear, popup);

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(prompt, theme.panel_title)),
        Line::from(""),
        Line::from(Span::styled("[Y]es / [N]o", theme.highlight)),
    ];

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(" Confirm ")
                .title_alignment(Alignment::Center),
        )
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, popup);
}
