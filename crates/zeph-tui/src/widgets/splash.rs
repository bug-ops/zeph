use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

const BANNER: &[&str] = &[
    r" ███████╗███████╗██████╗ ██╗  ██╗",
    r" ╚══███╔╝██╔════╝██╔══██╗██║  ██║",
    r"   ███╔╝ █████╗  ██████╔╝███████║",
    r"  ███╔╝  ██╔══╝  ██╔═══╝ ██╔══██║",
    r" ███████╗███████╗██║     ██║  ██║",
    r" ╚══════╝╚══════╝╚═╝     ╚═╝  ╚═╝",
];

pub fn render(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let banner_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let version_style = Style::default().fg(Color::DarkGray);

    let hint_style = Style::default().fg(Color::Gray);

    let inner_height = area.height.saturating_sub(2) as usize;
    let content_height = BANNER.len() + 3; // banner + blank + version + hints
    let top_pad = inner_height.saturating_sub(content_height) / 2;

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(top_pad + content_height);

    for _ in 0..top_pad {
        lines.push(Line::default());
    }

    for row in BANNER {
        lines.push(Line::from(Span::styled(*row, banner_style)));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("  v{}  ", env!("CARGO_PKG_VERSION")),
        version_style,
    )));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Type a message to start.",
        hint_style,
    )));

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .alignment(Alignment::Center);

    frame.render_widget(paragraph, area);
}
