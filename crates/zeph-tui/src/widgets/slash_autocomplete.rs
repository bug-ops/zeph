// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState};

use crate::command::{CommandEntry, filter_commands};
use crate::theme::Theme;

pub const MAX_VISIBLE: usize = 8;

pub struct SlashAutocompleteState {
    pub query: String,
    pub selected: usize,
    pub filtered: Vec<&'static CommandEntry>,
    pub scroll_offset: usize,
}

impl SlashAutocompleteState {
    #[must_use]
    pub fn new() -> Self {
        let mut s = Self {
            query: String::new(),
            selected: 0,
            filtered: Vec::new(),
            scroll_offset: 0,
        };
        s.refilter();
        s
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    /// Removes last char from query. Returns `true` if query is now empty (caller should dismiss).
    pub fn pop_char(&mut self) -> bool {
        if self.query.is_empty() {
            return true;
        }
        self.query.pop();
        self.refilter();
        self.query.is_empty()
    }

    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.filtered.len() - 1;
        } else {
            self.selected -= 1;
        }
        self.adjust_scroll();
    }

    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.selected == self.filtered.len() - 1 {
            self.selected = 0;
        } else {
            self.selected += 1;
        }
        self.adjust_scroll();
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
        self.scroll_offset = self
            .scroll_offset
            .min(self.filtered.len().saturating_sub(MAX_VISIBLE));
        self.adjust_scroll();
    }

    fn adjust_scroll(&mut self) {
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + MAX_VISIBLE {
            self.scroll_offset = self.selected + 1 - MAX_VISIBLE;
        }
        let max_offset = self.filtered.len().saturating_sub(MAX_VISIBLE);
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
    }
}

impl Default for SlashAutocompleteState {
    fn default() -> Self {
        Self::new()
    }
}

/// Converts a command id to slash-form: `"skill:list"` → `"/skill list"`.
#[must_use]
pub fn command_id_to_slash_form(id: &str) -> String {
    format!("/{}", id.replace(':', " "))
}

pub fn render(state: &SlashAutocompleteState, frame: &mut Frame, input_area: Rect) {
    if state.filtered.is_empty() {
        return;
    }

    let theme = Theme::default();

    let visible = state.filtered.len().min(MAX_VISIBLE);
    #[allow(clippy::cast_possible_truncation)]
    let height = (visible as u16) + 2;

    let width: u16 = 60;
    let x = if input_area.width > width {
        input_area.x + (input_area.width - width) / 2
    } else {
        input_area.x
    };
    let actual_width = width.min(input_area.width);

    let y = input_area.y.saturating_sub(height);

    let popup = Rect {
        x,
        y,
        width: actual_width,
        height,
    };

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(" Commands ")
        .title_alignment(Alignment::Center);

    frame.render_widget(block, popup);

    let list_area = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });

    let end = (state.scroll_offset + MAX_VISIBLE).min(state.filtered.len());
    let items: Vec<ListItem> = state.filtered[state.scroll_offset..end]
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let abs_i = i + state.scroll_offset;
            let style = if abs_i == state.selected {
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
    list_state.select(Some(state.selected.saturating_sub(state.scroll_offset)));

    frame.render_stateful_widget(List::new(items), list_area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::render_to_string;

    #[test]
    fn new_opens_with_all_commands() {
        let state = SlashAutocompleteState::new();
        let expected = filter_commands("");
        assert_eq!(state.filtered.len(), expected.len());
        assert_eq!(state.selected, 0);
        assert!(state.query.is_empty());
    }

    #[test]
    fn push_char_filters() {
        let mut state = SlashAutocompleteState::new();
        state.push_char('s');
        state.push_char('k');
        let expected = filter_commands("sk");
        assert_eq!(state.filtered.len(), expected.len());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn command_id_to_slash_form_converts() {
        assert_eq!(command_id_to_slash_form("skill:list"), "/skill list");
        assert_eq!(command_id_to_slash_form("ingest"), "/ingest");
        assert_eq!(command_id_to_slash_form("app:quit"), "/app quit");
    }

    #[test]
    fn pop_char_returns_true_when_empty() {
        let mut state = SlashAutocompleteState::new();
        assert!(state.pop_char());
    }

    #[test]
    fn pop_char_returns_false_when_query_not_empty_after_pop() {
        let mut state = SlashAutocompleteState::new();
        state.push_char('s');
        state.push_char('k');
        // After removing 'k', query is "s" (non-empty) → returns false
        assert!(!state.pop_char());
        assert_eq!(state.query, "s");
    }

    #[test]
    fn move_down_wraps() {
        let mut state = SlashAutocompleteState::new();
        assert!(!state.filtered.is_empty());
        state.selected = state.filtered.len() - 1;
        state.move_down();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn move_up_wraps() {
        let mut state = SlashAutocompleteState::new();
        assert!(!state.filtered.is_empty());
        state.selected = 0;
        state.move_up();
        assert_eq!(state.selected, state.filtered.len() - 1);
    }

    #[test]
    fn empty_filter_auto_dismisses_signal() {
        let mut state = SlashAutocompleteState::new();
        for c in "xxxxxxxxxxx".chars() {
            state.push_char(c);
        }
        assert!(state.filtered.is_empty());
    }

    #[test]
    fn render_slash_autocomplete_snapshot() {
        let state = SlashAutocompleteState::new();
        let output = render_to_string(80, 24, |frame, area| {
            render(&state, frame, area);
        });
        assert!(output.contains("Commands"));
        assert!(output.contains("skill:list"));
    }

    #[test]
    fn scroll_offset_adjusts_on_move_down() {
        let mut state = SlashAutocompleteState::new();
        if state.filtered.len() <= MAX_VISIBLE {
            return;
        }
        for _ in 0..MAX_VISIBLE {
            state.move_down();
        }
        assert!(state.scroll_offset > 0);
    }
}
