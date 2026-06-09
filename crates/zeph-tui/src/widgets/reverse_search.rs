// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState};

use crate::layout::truncate_to_width;
use crate::theme::Theme;

const MAX_VISIBLE: usize = 8;

/// Reverse-search state for Ctrl+R prompt history search.
///
/// Holds the current query and a filtered list of indices into the session's
/// `input_history`. History is current-session only (populated on submit,
/// empty at startup).
pub struct ReverseSearchState {
    /// The typed search query.
    pub query: String,
    /// Indices into `input_history` that contain `query` as a substring, newest-first.
    pub matches: Vec<usize>,
    /// Index into `matches` for the currently highlighted entry.
    pub selected: usize,
}

impl ReverseSearchState {
    /// Create a new state and populate `matches` from the full history (newest-first).
    #[must_use]
    pub fn new(history: &[String]) -> Self {
        let matches = Self::compute_matches("", history);
        Self {
            query: String::new(),
            matches,
            selected: 0,
        }
    }

    /// Append a character to the query and recompute matches.
    pub fn push_char(&mut self, c: char, history: &[String]) {
        self.query.push(c);
        self.refilter(history);
    }

    /// Remove the last character from the query and recompute matches.
    pub fn pop_char(&mut self, history: &[String]) {
        self.query.pop();
        self.refilter(history);
    }

    /// Advance `selected` to the next older match (wraps at the end).
    ///
    /// Mirrors bash `Ctrl+R` behaviour: repeated presses cycle through older entries.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_tui::widgets::reverse_search::ReverseSearchState;
    ///
    /// let history = vec!["first".to_owned(), "second".to_owned(), "third".to_owned()];
    /// let mut state = ReverseSearchState::new(&history);
    /// // starts at newest (selected = 0)
    /// state.select_next();
    /// assert_eq!(state.selected, 1);
    /// state.select_next();
    /// assert_eq!(state.selected, 2);
    /// // wraps back to 0
    /// state.select_next();
    /// assert_eq!(state.selected, 0);
    /// ```
    pub fn select_next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.matches.len();
    }

    /// Move `selected` to the previous (newer) match (wraps at the beginning).
    ///
    /// Mirrors bash `Ctrl+S` behaviour: moves back toward more recent matches.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_tui::widgets::reverse_search::ReverseSearchState;
    ///
    /// let history = vec!["first".to_owned(), "second".to_owned(), "third".to_owned()];
    /// let mut state = ReverseSearchState::new(&history);
    /// // wraps from 0 to last
    /// state.select_previous();
    /// assert_eq!(state.selected, 2);
    /// state.select_previous();
    /// assert_eq!(state.selected, 1);
    /// ```
    pub fn select_previous(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.matches.len() - 1);
    }

    /// Return the history entry for the currently selected match, or `None` if history is empty.
    #[must_use]
    pub fn selected_entry<'h>(&self, history: &'h [String]) -> Option<&'h str> {
        self.matches
            .get(self.selected)
            .and_then(|&idx| history.get(idx))
            .map(String::as_str)
    }

    fn refilter(&mut self, history: &[String]) {
        self.matches = Self::compute_matches(&self.query, history);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    fn compute_matches(query: &str, history: &[String]) -> Vec<usize> {
        // Iterate newest-first (reverse index order).
        (0..history.len())
            .rev()
            .filter(|&i| query.is_empty() || history[i].contains(query))
            .collect()
    }
}

/// Render the reverse-search overlay anchored above `input_area`.
pub fn render(state: &ReverseSearchState, history: &[String], frame: &mut Frame, input_area: Rect) {
    let theme = Theme::default();

    let visible = if state.matches.is_empty() {
        1
    } else {
        state.matches.len().min(MAX_VISIBLE)
    };
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

    let title = if state.query.is_empty() {
        " History ".to_owned()
    } else {
        format!(" History: {} ", state.query)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(title)
        .title_alignment(Alignment::Center);

    frame.render_widget(block, popup);

    let list_area = popup.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });

    if state.matches.is_empty() {
        let item = ListItem::new(Line::from(Span::styled(
            "(no history)",
            Style::default().fg(ratatui::style::Color::DarkGray),
        )));
        frame.render_widget(List::new(vec![item]), list_area);
        return;
    }

    let end = state.matches.len().min(MAX_VISIBLE);
    let items: Vec<ListItem> = state.matches[..end]
        .iter()
        .enumerate()
        .map(|(i, &idx)| {
            let entry = history.get(idx).map_or("", String::as_str);
            let style = if i == state.selected {
                Style::default().bg(theme.highlight.fg.unwrap_or(ratatui::style::Color::Blue))
            } else {
                Style::default()
            };
            let max_chars = (actual_width as usize).saturating_sub(5);
            let display = truncate_to_width(entry, max_chars);
            ListItem::new(Line::from(Span::styled(display, style)))
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

    fn make_history(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn new_with_empty_history_has_no_matches() {
        let state = ReverseSearchState::new(&[]);
        assert!(state.matches.is_empty());
        assert_eq!(state.selected, 0);
        assert!(state.query.is_empty());
    }

    #[test]
    fn new_with_history_shows_all_newest_first() {
        let history = make_history(&["first", "second", "third"]);
        let state = ReverseSearchState::new(&history);
        // matches should be indices in reverse order: [2, 1, 0]
        assert_eq!(state.matches, vec![2, 1, 0]);
    }

    #[test]
    fn push_char_filters_by_substring() {
        let history = make_history(&["foo bar", "baz", "foo baz"]);
        let mut state = ReverseSearchState::new(&history);
        state.push_char('f', &history);
        // "foo baz" (idx 2) and "foo bar" (idx 0) both match, newest-first
        assert_eq!(state.matches, vec![2, 0]);
    }

    #[test]
    fn pop_char_restores_wider_matches() {
        let history = make_history(&["hello", "world"]);
        let mut state = ReverseSearchState::new(&history);
        state.push_char('h', &history);
        assert_eq!(state.matches.len(), 1);
        state.pop_char(&history);
        assert_eq!(state.matches.len(), 2);
    }

    #[test]
    fn select_next_wraps_at_end() {
        let history = make_history(&["a", "b"]);
        let mut state = ReverseSearchState::new(&history);
        state.select_next();
        assert_eq!(state.selected, 1);
        state.select_next(); // wraps back to 0
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn select_next_noop_on_empty() {
        let mut state = ReverseSearchState::new(&[]);
        state.select_next();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn select_previous_wraps_at_start() {
        let history = make_history(&["a", "b", "c"]);
        let mut state = ReverseSearchState::new(&history);
        // selected = 0, wraps to last
        state.select_previous();
        assert_eq!(state.selected, 2);
        state.select_previous();
        assert_eq!(state.selected, 1);
        state.select_previous();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn select_previous_noop_on_empty() {
        let mut state = ReverseSearchState::new(&[]);
        state.select_previous();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn select_next_then_previous_returns_to_start() {
        let history = make_history(&["a", "b", "c"]);
        let mut state = ReverseSearchState::new(&history);
        state.select_next();
        assert_eq!(state.selected, 1);
        state.select_previous();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn selected_entry_returns_correct_item() {
        let history = make_history(&["first", "second"]);
        let state = ReverseSearchState::new(&history);
        // newest-first: matches = [1, 0], selected = 0 → history[1] = "second"
        assert_eq!(state.selected_entry(&history), Some("second"));
    }

    #[test]
    fn slash_in_query_does_not_open_slash_autocomplete() {
        // This test verifies the data path: push_char('/') is handled by ReverseSearchState,
        // not by the slash autocomplete trigger. The overlay receives the '/' as query input.
        let history = make_history(&["/clear", "/help", "hello"]);
        let mut state = ReverseSearchState::new(&history);
        state.push_char('/', &history);
        assert_eq!(state.query, "/");
        // Only entries containing '/' match
        assert_eq!(state.matches, vec![1, 0]);
    }

    #[test]
    fn render_reverse_search_snapshot_empty() {
        let history = vec![];
        let state = ReverseSearchState::new(&history);
        let output = render_to_string(80, 24, |frame, area| {
            render(&state, &history, frame, area);
        });
        assert!(output.contains("History"));
        assert!(output.contains("no history"));
    }

    #[test]
    fn render_reverse_search_snapshot_with_items() {
        let history = make_history(&["first prompt", "second prompt"]);
        let state = ReverseSearchState::new(&history);
        let output = render_to_string(80, 24, |frame, area| {
            render(&state, &history, frame, area);
        });
        assert!(output.contains("History"));
        assert!(output.contains("second prompt"));
    }

    #[test]
    fn render_long_cyrillic_entry_does_not_panic() {
        // Regression test for S1: byte-slice truncation panicked on multibyte UTF-8.
        // Cyrillic chars are 2 bytes each; a 60-char string exceeds the 55-char overlay width
        // and forces the truncation branch. Must not panic.
        let long_cyrillic = "Привет ".repeat(10); // ~70 chars, well over overlay width
        let history = vec![long_cyrillic];
        let state = ReverseSearchState::new(&history);
        // Must not panic — that is the primary assertion (previously panicked at byte boundary).
        let output = render_to_string(60, 24, |frame, area| {
            render(&state, &history, frame, area);
        });
        assert!(output.contains("History"), "overlay must render: {output}");
    }
}
