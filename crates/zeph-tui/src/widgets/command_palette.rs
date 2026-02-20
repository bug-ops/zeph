use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::command::{CommandEntry, filter_commands};
use crate::layout::centered_rect;
use crate::theme::Theme;

pub struct CommandPaletteState {
    pub query: String,
    pub cursor: usize,
    pub selected: usize,
    pub filtered: Vec<&'static CommandEntry>,
}

impl CommandPaletteState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            query: String::new(),
            cursor: 0,
            selected: 0,
            filtered: filter_commands(""),
        }
    }

    pub fn push_char(&mut self, c: char) {
        let byte_offset = self
            .query
            .char_indices()
            .nth(self.cursor)
            .map_or(self.query.len(), |(i, _)| i);
        self.query.insert(byte_offset, c);
        self.cursor += 1;
        self.refilter();
    }

    pub fn pop_char(&mut self) {
        if self.cursor > 0 {
            let byte_offset = self
                .query
                .char_indices()
                .nth(self.cursor - 1)
                .map_or(self.query.len(), |(i, _)| i);
            self.query.remove(byte_offset);
            self.cursor -= 1;
            self.refilter();
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1).min(self.filtered.len() - 1);
        }
    }

    #[must_use]
    pub fn selected_entry(&self) -> Option<&'static CommandEntry> {
        self.filtered.get(self.selected).copied()
    }

    fn refilter(&mut self) {
        self.filtered = filter_commands(&self.query);
        if self.filtered.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len() - 1);
        }
    }
}

impl Default for CommandPaletteState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn render(state: &CommandPaletteState, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    #[allow(clippy::cast_possible_truncation)]
    let height = (state.filtered.len() as u16 + 4).clamp(6, 20);
    let popup = centered_rect(60, height, area);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(" Command Palette ")
        .title_alignment(Alignment::Center);

    frame.render_widget(block, popup);

    let inner = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });

    if inner.height < 2 {
        return;
    }

    let query_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };

    let query_line = Line::from(vec![
        Span::styled(": ", theme.highlight),
        Span::raw(&state.query),
    ]);
    frame.render_widget(Paragraph::new(query_line), query_area);

    if inner.height < 3 {
        return;
    }

    let list_area = Rect {
        x: inner.x,
        y: inner.y + 2,
        width: inner.width,
        height: inner.height - 2,
    };

    let items: Vec<ListItem> = state
        .filtered
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let style = if i == state.selected {
                Style::default().bg(theme.highlight.fg.unwrap_or(ratatui::style::Color::Blue))
            } else {
                Style::default()
            };
            let shortcut_str = entry.shortcut.map_or(String::new(), |s| format!(" [{s}]"));
            let shortcut_style = style.patch(Style::default().fg(ratatui::style::Color::DarkGray));
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<20}", entry.id), style.patch(theme.panel_title)),
                Span::styled(format!("  {}", entry.label), style),
                Span::styled(shortcut_str, shortcut_style),
            ]))
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(state.selected));

    frame.render_stateful_widget(List::new(items), list_area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::render_to_string;

    #[test]
    fn new_state_has_all_commands() {
        let state = CommandPaletteState::new();
        assert!(state.filtered.len() >= 11);
        assert_eq!(state.selected, 0);
        assert!(state.query.is_empty());
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn push_char_updates_query_and_filters() {
        let mut state = CommandPaletteState::new();
        state.push_char('s');
        state.push_char('k');
        assert_eq!(state.query, "sk");
        assert_eq!(state.cursor, 2);
        assert!(!state.filtered.is_empty());
        assert_eq!(state.filtered[0].id, "skill:list");
    }

    #[test]
    fn pop_char_removes_last_char() {
        let mut state = CommandPaletteState::new();
        state.push_char('s');
        state.push_char('k');
        state.pop_char();
        assert_eq!(state.query, "s");
        assert_eq!(state.cursor, 1);
    }

    #[test]
    fn pop_char_on_empty_is_noop() {
        let mut state = CommandPaletteState::new();
        state.pop_char();
        assert!(state.query.is_empty());
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn move_down_increments_selection() {
        let mut state = CommandPaletteState::new();
        assert_eq!(state.selected, 0);
        state.move_down();
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn move_down_clamps_at_last() {
        let mut state = CommandPaletteState::new();
        let last = state.filtered.len() - 1;
        state.selected = last;
        state.move_down();
        assert_eq!(state.selected, last);
    }

    #[test]
    fn move_up_decrements_selection() {
        let mut state = CommandPaletteState::new();
        state.selected = 3;
        state.move_up();
        assert_eq!(state.selected, 2);
    }

    #[test]
    fn move_up_clamps_at_zero() {
        let mut state = CommandPaletteState::new();
        state.selected = 0;
        state.move_up();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn selected_entry_returns_correct_command() {
        let state = CommandPaletteState::new();
        let entry = state.selected_entry().unwrap();
        assert_eq!(entry.id, "skill:list");
    }

    #[test]
    fn selected_entry_returns_none_when_empty_filter() {
        let mut state = CommandPaletteState::new();
        for c in "xxxxxxxxxx".chars() {
            state.push_char(c);
        }
        assert!(state.selected_entry().is_none());
    }

    #[test]
    fn refilter_clamps_selection_to_new_len() {
        let mut state = CommandPaletteState::new();
        state.selected = 5;
        state.push_char('s');
        state.push_char('k');
        assert!(state.selected < state.filtered.len().max(1));
    }

    #[test]
    fn render_command_palette_snapshot() {
        let state = CommandPaletteState::new();
        let output = render_to_string(80, 24, |frame, area| {
            render(&state, frame, area);
        });
        assert!(output.contains("Command Palette"));
        assert!(output.contains("skill:list"));
        assert!(output.contains("mcp:list"));
    }

    #[test]
    fn render_with_query() {
        let mut state = CommandPaletteState::new();
        state.push_char('v');
        state.push_char('i');
        state.push_char('e');
        state.push_char('w');
        let output = render_to_string(80, 24, |frame, area| {
            render(&state, frame, area);
        });
        assert!(
            output.contains("view:cost")
                || output.contains("view:config")
                || output.contains("view:tools")
        );
    }
}
