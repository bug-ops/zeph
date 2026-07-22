// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Row, Table};

use crate::layout::centered_rect;
use crate::theme::Theme;

// 48 data rows + 1 header row + 2 border lines
const POPUP_HEIGHT: u16 = 51;

#[allow(clippy::too_many_lines)]
pub fn render(frame: &mut Frame, area: Rect, theme: &Theme) {
    let popup = centered_rect(70, POPUP_HEIGHT, area);
    frame.render_widget(Clear, popup);

    let rows = vec![
        Row::new([
            Cell::from(Span::styled("Normal mode", theme.panel_title)),
            Cell::from(""),
        ]),
        keybind_row("q", "quit"),
        keybind_row(
            "Ctrl+C",
            "interrupt agent turn / press twice to exit when idle",
        ),
        keybind_row("i", "enter insert mode"),
        keybind_row("j / k", "scroll down / up"),
        keybind_row("PgDn / PgUp", "page scroll down / up"),
        keybind_row("End / Home", "jump to bottom / top"),
        keybind_row("d", "toggle side panels"),
        keybind_row("e", "expand tools"),
        keybind_row("c", "compact tools"),
        keybind_row(
            "Tab",
            "cycle panels (Chat/Skills/Memory/Resources/SubAgents)",
        ),
        keybind_row("a", "focus Sub-Agents panel"),
        keybind_row("S", "settings: browse providers, MCP servers, agents"),
        keybind_row("?", "toggle this help"),
        Row::new([Cell::from(""), Cell::from("")]),
        Row::new([
            Cell::from(Span::styled(
                "Sub-Agents panel (focused)",
                theme.panel_title,
            )),
            Cell::from(""),
        ]),
        keybind_row("j / k", "navigate agent list"),
        keybind_row("Enter", "view selected agent transcript"),
        keybind_row("Esc", "close panel focus"),
        Row::new([Cell::from(""), Cell::from("")]),
        Row::new([
            Cell::from(Span::styled("Subagent transcript view", theme.panel_title)),
            Cell::from(""),
        ]),
        keybind_row("Esc", "return to main conversation"),
        Row::new([Cell::from(""), Cell::from("")]),
        Row::new([
            Cell::from(Span::styled("Settings panel (focused)", theme.panel_title)),
            Cell::from(""),
        ]),
        keybind_row("h / l", "switch tab (Providers/MCP/Agents)"),
        keybind_row("j / k", "move selection"),
        keybind_row("Esc", "close panel focus"),
        Row::new([
            Cell::from(Span::styled("Insert mode", theme.panel_title)),
            Cell::from(""),
        ]),
        keybind_row("Enter", "send message"),
        keybind_row("Shift+Enter", "insert newline"),
        keybind_row("Ctrl+J", "insert newline"),
        keybind_row("Esc", "return to normal mode"),
        keybind_row("Ctrl+U", "clear input"),
        keybind_row("Ctrl+K", "clear queue"),
        keybind_row("Up / Down", "navigate history"),
        keybind_row("Ctrl+F", "find in conversation (also works in Normal mode)"),
        Row::new([Cell::from(""), Cell::from("")]),
        Row::new([
            Cell::from(Span::styled("Confirm mode", theme.panel_title)),
            Cell::from(""),
        ]),
        keybind_row("y", "confirm"),
        keybind_row("n / Esc", "cancel"),
        Row::new([Cell::from(""), Cell::from("")]),
        Row::new([
            Cell::from(Span::styled("Slash commands", theme.panel_title)),
            Cell::from(""),
        ]),
        keybind_row("/theme", "list available themes"),
        keybind_row("/theme <name>", "switch theme, e.g. /theme gruvbox-dark"),
        keybind_row(
            "/theme <name> (cycle)",
            "palette: app:theme cycles zephyr → zephyr-light → high-contrast",
        ),
        keybind_row("/motion full", "wave animation on input separator row"),
        keybind_row("/motion minimal", "breeze spinner, no wave"),
        keybind_row("/motion off", "no animation (static row)"),
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
            .border_type(BorderType::Rounded)
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
            let theme = crate::theme::Theme::default();
            super::render(frame, area, &theme);
        });
        assert_snapshot!(output);
    }
}
