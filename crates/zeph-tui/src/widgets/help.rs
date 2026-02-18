use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Row, Table};

use crate::layout::centered_rect;
use crate::theme::Theme;

// 23 data rows + 1 header row + 2 border lines
const POPUP_HEIGHT: u16 = 26;

pub fn render(frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let popup = centered_rect(70, POPUP_HEIGHT, area);
    frame.render_widget(Clear, popup);

    let rows = vec![
        Row::new([
            Cell::from(Span::styled("Normal mode", theme.panel_title)),
            Cell::from(""),
        ]),
        keybind_row("q", "quit"),
        keybind_row("i", "enter insert mode"),
        keybind_row("j / k", "scroll down / up"),
        keybind_row("PgDn / PgUp", "page scroll down / up"),
        keybind_row("End / Home", "jump to bottom / top"),
        keybind_row("d", "toggle side panels"),
        keybind_row("e", "expand tools"),
        keybind_row("c", "compact tools"),
        keybind_row("Tab", "cycle panels"),
        keybind_row("?", "toggle this help"),
        Row::new([Cell::from(""), Cell::from("")]),
        Row::new([
            Cell::from(Span::styled("Insert mode", theme.panel_title)),
            Cell::from(""),
        ]),
        keybind_row("Enter", "send message"),
        keybind_row("Shift+Enter", "insert newline"),
        keybind_row("Esc", "return to normal mode"),
        keybind_row("Ctrl+U", "clear input"),
        keybind_row("Ctrl+K", "clear queue"),
        keybind_row("Up / Down", "navigate history"),
        Row::new([Cell::from(""), Cell::from("")]),
        Row::new([
            Cell::from(Span::styled("Confirm mode", theme.panel_title)),
            Cell::from(""),
        ]),
        keybind_row("y", "confirm"),
        keybind_row("n / Esc", "cancel"),
    ];

    let header = Row::new([
        Cell::from(Span::styled("Key", theme.highlight)),
        Cell::from(Span::styled("Action", theme.highlight)),
    ]);

    let table = Table::new(
        rows,
        [Constraint::Percentage(35), Constraint::Percentage(65)],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border)
            .title(" Help — press ? or Esc to close ")
            .title_alignment(Alignment::Center),
    );

    frame.render_widget(table, popup);
}

fn keybind_row(key: &'static str, action: &'static str) -> Row<'static> {
    Row::new([Cell::from(Line::from(key)), Cell::from(Line::from(action))])
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::test_utils::render_to_string;

    #[test]
    fn help_default() {
        let output = render_to_string(80, 30, |frame, area| {
            super::render(frame, area);
        });
        assert_snapshot!(output);
    }
}
