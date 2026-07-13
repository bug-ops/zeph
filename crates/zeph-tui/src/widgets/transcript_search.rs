// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `Ctrl+F` in-transcript search overlay (issue #6023).
//!
//! Mirrors [`crate::widgets::reverse_search::ReverseSearchState`]'s
//! `push_char`/`pop_char`/`refilter`/next/prev pattern, but searches the currently visible
//! conversation transcript (`ChatMessage.content` + `tool_name`) instead of the input
//! history, and highlights-and-scrolls rather than replacing the input buffer.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::widgets::{Block, Borders, Clear};

use crate::theme::Theme;
use crate::types::ChatMessage;

/// State for the `Ctrl+F` transcript search overlay.
///
/// Holds the current query and the set of matching message indices within the
/// transcript passed to [`TranscriptSearchState::new`]/[`push_char`](Self::push_char)/
/// [`pop_char`](Self::pop_char) — the corpus is **not** owned by this state, mirroring
/// how `ReverseSearchState` receives `history` on every mutation.
pub struct TranscriptSearchState {
    /// The typed search query, as entered.
    pub query: String,
    /// Lowercased query, cached so matching never re-lowercases the query per message.
    query_lower: String,
    /// Indices into the transcript slice (`visible_messages()`) that match `query`.
    pub matches: Vec<usize>,
    /// Index into `matches` for the currently selected/highlighted match.
    pub selected: usize,
    /// `scroll_offset` captured when the overlay was opened, restored on Esc-cancel.
    pub pre_search_scroll_offset: usize,
}

impl TranscriptSearchState {
    /// Create a new, empty search state. `matches` starts empty (FR-010: no query
    /// typed yet SHALL show zero matches, unlike `ReverseSearchState::new` which
    /// shows the full history — transcript search has no equivalent "browse
    /// everything" default because messages are already fully visible).
    #[must_use]
    pub fn new(current_scroll: usize) -> Self {
        Self {
            query: String::new(),
            query_lower: String::new(),
            matches: Vec::new(),
            selected: 0,
            pre_search_scroll_offset: current_scroll,
        }
    }

    /// Append a character to the query and recompute matches.
    pub fn push_char(&mut self, c: char, messages: &[ChatMessage]) {
        self.query.push(c);
        self.query_lower = self.query.to_lowercase();
        self.refilter(messages);
    }

    /// Remove the last character from the query and recompute matches.
    pub fn pop_char(&mut self, messages: &[ChatMessage]) {
        self.query.pop();
        self.query_lower = self.query.to_lowercase();
        self.refilter(messages);
    }

    /// Advance `selected` to the next match, wrapping at the end.
    pub fn select_next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.matches.len();
    }

    /// Move `selected` to the previous match, wrapping at the beginning.
    pub fn select_previous(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.matches.len() - 1);
    }

    /// Returns the transcript message index of the currently selected match, or `None`
    /// when there are no matches.
    #[must_use]
    pub fn selected_message_index(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }

    /// Returns the cached lowercased query, for highlight span-splitting in the chat
    /// renderer (avoids re-lowercasing the query on every rendered message).
    #[must_use]
    pub fn query_lower(&self) -> &str {
        &self.query_lower
    }

    fn refilter(&mut self, messages: &[ChatMessage]) {
        self.matches = Self::compute_matches(&self.query_lower, messages);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    fn compute_matches(query_lower: &str, messages: &[ChatMessage]) -> Vec<usize> {
        if query_lower.is_empty() {
            return Vec::new();
        }
        messages
            .iter()
            .enumerate()
            .filter(|(_, msg)| message_matches(msg, query_lower))
            .map(|(i, _)| i)
            .collect()
    }
}

/// Returns `true` if `msg.content` or `msg.tool_name` contains `query_lower` as a
/// case-insensitive substring (US-002: tool calls must be findable by name too).
fn message_matches(msg: &ChatMessage, query_lower: &str) -> bool {
    if msg.content.to_lowercase().contains(query_lower) {
        return true;
    }
    msg.tool_name
        .as_ref()
        .is_some_and(|name| name.as_str().to_lowercase().contains(query_lower))
}

/// Render the transcript-search bar anchored above `input_area`, mirroring
/// `reverse_search::render`'s popup placement and styling.
pub fn render(state: &TranscriptSearchState, frame: &mut Frame, input_area: Rect, theme: &Theme) {
    let width: u16 = 60;
    let height: u16 = 3;
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

    let match_info = if state.query.is_empty() {
        String::new()
    } else if state.matches.is_empty() {
        "  no matches".to_owned()
    } else {
        format!("  {}/{}", state.selected + 1, state.matches.len())
    };

    let title = format!(" Find: {}{} ", state.query, match_info);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(title)
        .title_alignment(Alignment::Center);

    frame.render_widget(block, popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::render_to_string;
    use crate::types::MessageRole;

    fn messages(items: &[&str]) -> Vec<ChatMessage> {
        items
            .iter()
            .map(|s| ChatMessage::new(MessageRole::Assistant, (*s).to_owned()))
            .collect()
    }

    #[test]
    fn new_state_has_no_matches_before_any_query() {
        let state = TranscriptSearchState::new(5);
        assert!(state.matches.is_empty());
        assert_eq!(state.selected, 0);
        assert_eq!(state.pre_search_scroll_offset, 5);
    }

    #[test]
    fn push_char_filters_case_insensitively() {
        let msgs = messages(&["Hello World", "goodbye", "HELLO there"]);
        let mut state = TranscriptSearchState::new(0);
        state.push_char('h', &msgs);
        state.push_char('e', &msgs);
        state.push_char('l', &msgs);
        state.push_char('l', &msgs);
        assert_eq!(state.matches, vec![0, 2]);
    }

    #[test]
    fn pop_char_recomputes_wider_matches() {
        let msgs = messages(&["hello", "help", "world"]);
        let mut state = TranscriptSearchState::new(0);
        state.push_char('h', &msgs);
        state.push_char('e', &msgs);
        state.push_char('l', &msgs);
        state.push_char('p', &msgs);
        assert_eq!(state.matches, vec![1]);
        state.pop_char(&msgs);
        assert_eq!(state.matches, vec![0, 1]);
    }

    #[test]
    fn empty_query_has_zero_matches_not_all_messages() {
        // FR-010: differs from ReverseSearchState, which shows everything on an
        // empty query — here an empty query means "nothing typed yet", not "browse all".
        let msgs = messages(&["a", "b", "c"]);
        let state = TranscriptSearchState::new(0);
        assert!(state.matches.is_empty());
        let mut state2 = TranscriptSearchState::new(0);
        state2.push_char('a', &msgs);
        state2.pop_char(&msgs);
        assert!(state2.matches.is_empty());
    }

    #[test]
    fn matches_tool_name_not_just_content() {
        let mut msgs = messages(&["some output"]);
        msgs[0].tool_name = Some(zeph_common::ToolName::new("shell"));
        let mut state = TranscriptSearchState::new(0);
        for c in "shell".chars() {
            state.push_char(c, &msgs);
        }
        assert_eq!(state.matches, vec![0]);
    }

    #[test]
    fn select_next_wraps() {
        let msgs = messages(&["x", "x", "x"]);
        let mut state = TranscriptSearchState::new(0);
        state.push_char('x', &msgs);
        assert_eq!(state.matches.len(), 3);
        state.select_next();
        state.select_next();
        state.select_next();
        assert_eq!(state.selected, 0, "must wrap back to the first match");
    }

    #[test]
    fn select_previous_wraps() {
        let msgs = messages(&["x", "x"]);
        let mut state = TranscriptSearchState::new(0);
        state.push_char('x', &msgs);
        state.select_previous();
        assert_eq!(state.selected, 1, "must wrap to the last match");
    }

    #[test]
    fn select_next_noop_on_empty_matches() {
        let mut state = TranscriptSearchState::new(0);
        state.select_next();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn selected_message_index_reflects_current_selection() {
        let msgs = messages(&["needle here", "no match", "needle again"]);
        let mut state = TranscriptSearchState::new(0);
        for c in "needle".chars() {
            state.push_char(c, &msgs);
        }
        assert_eq!(state.selected_message_index(), Some(0));
        state.select_next();
        assert_eq!(state.selected_message_index(), Some(2));
    }

    #[test]
    fn refilter_clamps_selected_when_matches_shrink() {
        let msgs = messages(&["cat", "cats", "dog"]);
        let mut state = TranscriptSearchState::new(0);
        state.push_char('c', &msgs);
        state.select_next(); // now at index 1 (of 2 matches: cat, cats)
        assert_eq!(state.selected, 1);
        for c in "atxyz".chars() {
            state.push_char(c, &msgs);
        }
        // no message contains "catxyz" -> matches empty, selected clamped to 0
        assert!(state.matches.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn render_search_bar_snapshot_shows_query_and_count() {
        let msgs = messages(&["needle", "needle"]);
        let mut state = TranscriptSearchState::new(0);
        for c in "needle".chars() {
            state.push_char(c, &msgs);
        }
        let output = render_to_string(80, 24, |frame, area| {
            let theme = Theme::default();
            render(&state, frame, area, &theme);
        });
        assert!(output.contains("needle"));
        assert!(output.contains("1/2"));
    }

    #[test]
    fn render_no_matches_snapshot() {
        let msgs = messages(&["hello"]);
        let mut state = TranscriptSearchState::new(0);
        for c in "zzz".chars() {
            state.push_char(c, &msgs);
        }
        let output = render_to_string(80, 24, |frame, area| {
            let theme = Theme::default();
            render(&state, frame, area, &theme);
        });
        assert!(output.contains("no matches"));
    }
}
