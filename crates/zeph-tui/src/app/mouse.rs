// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Mouse event decoding and hit-testing for opt-in mouse capture (#5103).
//!
//! All mouse handling is routed through [`App::handle_mouse`], which translates
//! raw crossterm [`MouseEvent`]s into semantic [`Action`]s and dispatches them
//! through the reducer. The reducer is the sole mutation site (INV-R1).

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::app::Panel;
use crate::layout::AppLayout;

use super::App;
use super::action::{Action, ScrollDir};
use super::reducer::{reduce, run_effects};

impl App {
    /// Decode a raw [`MouseEvent`] against the last known layout and dispatch
    /// the corresponding [`Action`] through the reducer.
    ///
    /// Returns immediately when `last_layout` is `None` (before the first
    /// frame is rendered — C3 / INV-M1).
    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) {
        let Some(layout) = self.last_layout else {
            return;
        };
        let Some(action) = decode_mouse(event, &layout) else {
            return;
        };
        let effects = reduce(self, action);
        run_effects(self, effects);
    }
}

/// Translate a raw [`MouseEvent`] into a semantic [`Action`] using the current
/// layout for hit-testing.
///
/// Returns `None` for events that have no TUI-level meaning (e.g. button
/// releases, or clicks in areas with no defined behaviour).
fn decode_mouse(event: MouseEvent, layout: &AppLayout) -> Option<Action> {
    match event.kind {
        MouseEventKind::ScrollUp => Some(Action::ScrollLines(-3)),
        MouseEventKind::ScrollDown => Some(Action::ScrollLines(3)),
        MouseEventKind::Down(MouseButton::Left) => {
            let col = event.column;
            let row = event.row;
            // Side-panel click: focus the appropriate panel section.
            if rect_contains(layout.skills, col, row) {
                return Some(Action::SetActivePanel(Panel::Skills));
            }
            if rect_contains(layout.memory, col, row) {
                return Some(Action::SetActivePanel(Panel::Memory));
            }
            if rect_contains(layout.resources, col, row) {
                return Some(Action::SetActivePanel(Panel::Resources));
            }
            if rect_contains(layout.subagents, col, row) {
                return Some(Action::SetActivePanel(Panel::SubAgents));
            }
            // Chat area click: move focus to the chat panel.
            if rect_contains(layout.chat, col, row) {
                return Some(Action::SetActivePanel(Panel::Chat));
            }
            // Input area click: switch to insert mode.
            if rect_contains(layout.input, col, row) {
                return Some(Action::EnterInsert);
            }
            None
        }
        // Right-click in chat: page scroll up (ergonomic shortcut).
        MouseEventKind::Down(MouseButton::Right) => {
            if rect_contains(layout.chat, event.column, event.row) {
                Some(Action::ScrollPage(ScrollDir::Up))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Returns `true` when `(col, row)` falls within `rect`.
///
/// Uses half-open intervals: `[x, x+width)` and `[y, y+height)`.
fn rect_contains(rect: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}
