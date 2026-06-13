// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reducer and effect runner for the TUI action pipeline.
//!
//! `reduce` is the **sole state-mutation site** for all actions routed through
//! the keyboard and mouse paths (INV-R1). It returns a `Vec<Effect>` for any
//! work that cannot be expressed as pure `App` field writes (I/O, channel sends,
//! one-shot sender resolution).

use zeph_core::channel::ElicitationResponse;

use super::action::{Action, CursorMove, ElicitationEdit, PaletteEdit, ScrollDir, VertDir};
use super::{App, ChatMessage, InputMode, MessageRole, Panel};
use crate::command::TuiCommand;
use crate::file_picker::FilePickerState;
use crate::widgets::command_palette::CommandPaletteState;
use crate::widgets::slash_autocomplete::{SlashAutocompleteState, command_id_to_slash_form};

const MAX_INPUT_HISTORY: usize = 500;

/// A side-effect that `reduce` defers to `run_effects` for execution.
///
/// Effects represent work that cannot be done inside the reducer because it
/// requires I/O, channel sends, or borrowed state from outside `App`
/// (e.g. terminal backend handles).
#[derive(Debug)]
pub(crate) enum Effect {
    /// Forward the user's typed text to the agent loop.
    SendUserInput(String),
    /// Forward a [`TuiCommand`] to the agent command channel.
    SendCommand(TuiCommand),
    /// Copy `text` to the system clipboard.
    CopyToClipboard(String),
    /// Trigger the file indexer for the file picker.
    StartFileIndex,
    /// Enable or disable mouse capture in the terminal backend.
    ///
    /// Stored in `pending_mouse_capture` and drained by the `tui_loop`
    /// post-select block (C2 — never drained inside an event arm).
    SetMouseCapture(bool),
    /// Quit the TUI.
    Quit,
}

/// Apply `action` to `app` and return any side-effects to run.
///
/// This is the single mutation point for keyboard and mouse paths (INV-R1).
/// The function must not perform I/O, channel sends, or any blocking work
/// (INV-R2). Callers must pass the returned effects to [`run_effects`].
#[allow(clippy::too_many_lines)]
pub(crate) fn reduce(app: &mut App, action: Action) -> Vec<Effect> {
    match action {
        // ── Scroll ─────────────────────────────────────────────────────────────
        Action::ScrollLines(delta) => {
            let cur = app.sessions.current().scroll_offset;
            let next = if delta > 0 {
                cur.saturating_sub(delta.unsigned_abs() as usize)
            } else {
                cur.saturating_add(delta.unsigned_abs() as usize)
            };
            app.sessions.current_mut().scroll_offset = next;
            vec![]
        }
        Action::ScrollPage(dir) => {
            use crate::app::keys::SCROLL_STEP_PAGE;
            let cur = app.sessions.current().scroll_offset;
            let next = match dir {
                ScrollDir::Up => cur.saturating_add(SCROLL_STEP_PAGE),
                ScrollDir::Down => cur.saturating_sub(SCROLL_STEP_PAGE),
            };
            app.begin_scroll(next);
            vec![]
        }
        Action::ScrollToTop => {
            let top = if let Some(cache) = &app.sessions.current().transcript_cache {
                cache.entries.len()
            } else {
                app.sessions.current().messages.len()
            };
            app.sessions.current_mut().scroll_offset = top;
            vec![]
        }
        Action::ScrollToBottom => {
            app.sessions.current_mut().scroll_offset = 0;
            vec![]
        }

        // ── Panel toggles ───────────────────────────────────────────────────────
        Action::ToggleToolExpanded => {
            app.tool_expanded = !app.tool_expanded;
            app.sessions.current_mut().render_cache.clear();
            vec![]
        }
        Action::CycleToolDensity => {
            app.tool_density = app.tool_density.cycle();
            app.sessions.current_mut().render_cache.clear();
            vec![]
        }
        Action::ToggleSidePanels => {
            app.show_side_panels = !app.show_side_panels;
            vec![]
        }
        Action::ToggleHelp => {
            app.show_help = !app.show_help;
            vec![]
        }
        Action::SetHelp(v) => {
            app.show_help = v;
            vec![]
        }
        Action::CyclePanelFocus => {
            app.active_panel = match app.active_panel {
                Panel::Chat => Panel::Skills,
                Panel::Skills => Panel::Memory,
                Panel::Memory => Panel::Resources,
                Panel::Resources => Panel::SubAgents,
                Panel::SubAgents | Panel::Tasks => Panel::Fleet,
                Panel::Fleet => Panel::Durable,
                Panel::Durable => Panel::Chat,
            };
            vec![]
        }
        Action::SetActivePanel(p) => {
            app.active_panel = p;
            if p == Panel::SubAgents
                && app.subagent_sidebar.selected().is_none()
                && !app.metrics.sub_agents.is_empty()
            {
                app.subagent_sidebar.list_state.select(Some(0));
            }
            vec![]
        }
        Action::TogglePanelCollapse(idx) => {
            app.toggle_panel_collapse(idx);
            vec![]
        }
        Action::ToggleTaskPanel => {
            app.show_task_panel = !app.show_task_panel;
            vec![]
        }
        Action::TogglePlanView => {
            app.sessions.current_mut().plan_view_active = !app.sessions.current().plan_view_active;
            vec![]
        }

        // ── Session / view ──────────────────────────────────────────────────────
        Action::SetViewTarget(target) => {
            app.set_view_target(target);
            vec![]
        }
        Action::ClearTranscript => {
            if app.sessions.current().view_target.is_main() {
                app.sessions.current_mut().messages.clear();
            }
            app.sessions.current_mut().render_cache.clear();
            app.sessions.current_mut().scroll_offset = 0;
            vec![]
        }
        Action::Quit => {
            vec![Effect::Quit]
        }
        Action::CancelAgent => {
            if let Some(ref signal) = app.cancel_signal {
                signal.notify_waiters();
            }
            vec![]
        }

        // ── Input mode ──────────────────────────────────────────────────────────
        Action::EnterInsert => {
            app.sessions.current_mut().input_mode = InputMode::Insert;
            vec![]
        }
        Action::EnterNormal => {
            app.sessions.current_mut().input_mode = InputMode::Normal;
            vec![]
        }
        Action::InsertChar(c) => {
            let was_empty = app.sessions.current().input.is_empty();
            let pos = app.sessions.current().cursor_position;
            let byte_offset = app.byte_offset_of_char(pos);
            app.sessions.current_mut().paste_state = None;
            app.sessions.current_mut().input.insert(byte_offset, c);
            app.sessions.current_mut().cursor_position += 1;
            if c == '/' && was_empty && app.slash_autocomplete.is_none() {
                app.slash_autocomplete = Some(SlashAutocompleteState::new());
            }
            vec![]
        }
        Action::InsertNewline => {
            app.insert_newline_at_cursor();
            vec![]
        }
        Action::InsertText(text) => {
            app.handle_paste(&text);
            vec![]
        }
        Action::DeleteCharBackward => {
            app.sessions.current_mut().paste_state = None;
            let pos = app.sessions.current().cursor_position;
            if pos > 0 {
                let byte_offset = app.byte_offset_of_char(pos - 1);
                app.sessions.current_mut().input.remove(byte_offset);
                app.sessions.current_mut().cursor_position -= 1;
            }
            vec![]
        }
        Action::DeleteCharForward => {
            app.sessions.current_mut().paste_state = None;
            let pos = app.sessions.current().cursor_position;
            let len = app.char_count();
            if pos < len {
                let byte_offset = app.byte_offset_of_char(pos);
                app.sessions.current_mut().input.remove(byte_offset);
            }
            vec![]
        }
        Action::DeleteWordBackward => {
            app.sessions.current_mut().paste_state = None;
            let boundary = app.prev_word_boundary();
            let pos = app.sessions.current().cursor_position;
            if boundary < pos {
                let start = app.byte_offset_of_char(boundary);
                let end = app.byte_offset_of_char(pos);
                app.sessions.current_mut().input.drain(start..end);
                app.sessions.current_mut().cursor_position = boundary;
            }
            vec![]
        }
        Action::MoveCursor(mv) => {
            app.sessions.current_mut().paste_state = None;
            match mv {
                CursorMove::Left => {
                    let pos = app.sessions.current().cursor_position;
                    app.sessions.current_mut().cursor_position = pos.saturating_sub(1);
                }
                CursorMove::Right => {
                    let pos = app.sessions.current().cursor_position;
                    let len = app.char_count();
                    if pos < len {
                        app.sessions.current_mut().cursor_position += 1;
                    }
                }
                CursorMove::WordLeft => {
                    let boundary = app.prev_word_boundary();
                    app.sessions.current_mut().cursor_position = boundary;
                }
                CursorMove::WordRight => {
                    let boundary = app.next_word_boundary();
                    app.sessions.current_mut().cursor_position = boundary;
                }
                CursorMove::Home => {
                    app.sessions.current_mut().cursor_position = 0;
                }
                CursorMove::End => {
                    let len = app.char_count();
                    app.sessions.current_mut().cursor_position = len;
                }
            }
            vec![]
        }
        Action::ClearInput => {
            app.sessions.current_mut().paste_state = None;
            app.sessions.current_mut().input.clear();
            app.sessions.current_mut().cursor_position = 0;
            vec![]
        }
        Action::SubmitInput => {
            let text = app.sessions.current().input.trim().to_owned();
            if text.is_empty() {
                return vec![];
            }
            // Check for local session slash commands first.
            if let Some(cmd) = App::parse_session_slash_pub(&text) {
                app.sessions.current_mut().input.clear();
                app.sessions.current_mut().cursor_position = 0;
                app.sessions.current_mut().history_index = None;
                app.sessions.current_mut().draft_input.clear();
                app.sessions.current_mut().paste_state = None;
                return vec![Effect::SendCommand(cmd)];
            }
            app.sessions.current_mut().show_splash = false;
            app.sessions.current_mut().input_history.push(text.clone());
            if app.sessions.current().input_history.len() > MAX_INPUT_HISTORY {
                let excess = app.sessions.current().input_history.len() - MAX_INPUT_HISTORY;
                app.sessions.current_mut().input_history.drain(0..excess);
            }
            let paste_lines = app
                .sessions
                .current_mut()
                .paste_state
                .take()
                .map(|p| p.line_count);
            let mut msg = ChatMessage::new(MessageRole::User, text.clone());
            msg.paste_line_count = paste_lines;
            app.sessions.current_mut().messages.push(msg);
            app.trim_messages();
            app.sessions.current_mut().input.clear();
            app.sessions.current_mut().cursor_position = 0;
            app.sessions.current_mut().scroll_offset = 0;
            app.sessions.current_mut().history_index = None;
            app.sessions.current_mut().draft_input.clear();
            app.editing_queued = false;
            app.pending_count += 1;
            vec![Effect::SendUserInput(text)]
        }
        Action::HistoryPrev => {
            app.handle_history_up();
            vec![]
        }
        Action::HistoryNext => {
            app.sessions.current_mut().paste_state = None;
            let Some(i) = app.sessions.current().history_index else {
                return vec![];
            };
            let prefix = app.sessions.current().draft_input.clone();
            let found = app.sessions.current().input_history[i + 1..]
                .iter()
                .position(|e| prefix.is_empty() || e.starts_with(&prefix))
                .map(|offset| i + 1 + offset);
            if let Some(idx) = found {
                app.sessions.current_mut().history_index = Some(idx);
                let text = app.sessions.current().input_history[idx].clone();
                app.sessions.current_mut().input = text;
            } else {
                app.sessions.current_mut().history_index = None;
                app.sessions.current_mut().input =
                    std::mem::take(&mut app.sessions.current_mut().draft_input);
            }
            app.sessions.current_mut().cursor_position = app.char_count();
            vec![]
        }
        Action::PrefillInput(prefix) => {
            app.sessions.current_mut().input.clear();
            app.sessions.current_mut().input.push_str(&prefix);
            app.sessions.current_mut().cursor_position = app.char_count();
            vec![]
        }

        // ── Command palette ─────────────────────────────────────────────────────
        Action::OpenCommandPalette => {
            app.command_palette = Some(CommandPaletteState::new());
            vec![]
        }
        Action::CloseCommandPalette => {
            app.command_palette = None;
            vec![]
        }
        Action::PaletteMove(dir) => {
            if let Some(ref mut p) = app.command_palette {
                match dir {
                    VertDir::Up => p.move_up(),
                    VertDir::Down => p.move_down(),
                }
            }
            vec![]
        }
        Action::PaletteInput(edit) => {
            if let Some(ref mut p) = app.command_palette {
                match edit {
                    PaletteEdit::PushChar(c) => p.push_char(c),
                    PaletteEdit::PopChar => p.pop_char(),
                }
            }
            vec![]
        }
        Action::PaletteAccept => {
            let cmd = app
                .command_palette
                .as_ref()
                .and_then(CommandPaletteState::selected_entry)
                .map(|e| e.command.clone());
            app.command_palette = None;
            if let Some(cmd) = cmd {
                return reduce(app, Action::Dispatch(cmd));
            }
            vec![]
        }

        // ── File picker ─────────────────────────────────────────────────────────
        Action::OpenFilePicker => {
            vec![Effect::StartFileIndex]
        }
        Action::CloseFilePicker => {
            app.file_picker_state = None;
            vec![]
        }
        Action::FilePickerMove(dir) => {
            if let Some(ref mut s) = app.file_picker_state {
                match dir {
                    VertDir::Up => s.move_selection(-1),
                    VertDir::Down => s.move_selection(1),
                }
            }
            vec![]
        }
        Action::FilePickerInput(edit) => {
            match edit {
                PaletteEdit::PushChar(c) => {
                    if let Some(ref mut s) = app.file_picker_state {
                        s.push_char(c);
                    }
                }
                PaletteEdit::PopChar => {
                    let dismissed = app.file_picker_state.as_mut().is_none_or(|s| !s.pop_char());
                    if dismissed {
                        app.file_picker_state = None;
                    }
                }
            }
            vec![]
        }
        Action::FilePickerAccept => {
            let selected = app
                .file_picker_state
                .as_ref()
                .and_then(FilePickerState::selected_path)
                .map(str::to_owned);
            app.file_picker_state = None;
            if let Some(path_str) = selected {
                let pos = app.sessions.current().cursor_position;
                let byte_offset = app.byte_offset_of_char(pos);
                app.sessions
                    .current_mut()
                    .input
                    .insert_str(byte_offset, &path_str);
                app.sessions.current_mut().cursor_position += path_str.chars().count();
            }
            vec![]
        }

        // ── Slash autocomplete ──────────────────────────────────────────────────
        Action::SlashAutocompleteMove(dir) => {
            if let Some(ref mut s) = app.slash_autocomplete {
                match dir {
                    VertDir::Up => s.move_up(),
                    VertDir::Down => s.move_down(),
                }
            }
            vec![]
        }
        Action::SlashAutocompleteAccept => {
            let entry_id = app
                .slash_autocomplete
                .as_ref()
                .and_then(SlashAutocompleteState::selected_entry)
                .map(|e| e.id);
            app.slash_autocomplete = None;
            if let Some(id) = entry_id {
                let slash_form = command_id_to_slash_form(id);
                app.sessions.current_mut().input = slash_form;
                app.sessions.current_mut().cursor_position = app.char_count();
            }
            vec![]
        }
        Action::SlashAutocompleteAcceptAndSubmit => {
            // Accept selection into input, then chain a SubmitInput.
            let mut effects = reduce(app, Action::SlashAutocompleteAccept);
            effects.extend(reduce(app, Action::SubmitInput));
            effects
        }
        Action::SlashAutocompletePopChar => {
            let dismiss = app
                .slash_autocomplete
                .as_mut()
                .is_none_or(SlashAutocompleteState::pop_char);
            if dismiss {
                app.sessions.current_mut().input.clear();
                app.sessions.current_mut().cursor_position = 0;
                app.slash_autocomplete = None;
            } else {
                let query = app
                    .slash_autocomplete
                    .as_ref()
                    .map_or(String::new(), |s| s.query.clone());
                app.sessions.current_mut().input = format!("/{query}");
                app.sessions.current_mut().cursor_position = app.char_count();
                if app
                    .slash_autocomplete
                    .as_ref()
                    .is_none_or(|s| s.filtered.is_empty())
                {
                    app.slash_autocomplete = None;
                }
            }
            vec![]
        }
        Action::SlashAutocompletePushChar(c) => {
            if let Some(s) = app.slash_autocomplete.as_mut() {
                s.push_char(c);
            }
            let query = app
                .slash_autocomplete
                .as_ref()
                .map_or(String::new(), |s| s.query.clone());
            app.sessions.current_mut().input = format!("/{query}");
            app.sessions.current_mut().cursor_position = app.char_count();
            if app
                .slash_autocomplete
                .as_ref()
                .is_none_or(|s| s.filtered.is_empty())
            {
                app.slash_autocomplete = None;
            }
            vec![]
        }
        Action::CloseSlashAutocomplete => {
            app.slash_autocomplete = None;
            vec![]
        }

        // ── Reverse search ──────────────────────────────────────────────────────
        Action::OpenReverseSearch => {
            let history = app.sessions.current().input_history.clone();
            app.reverse_search = Some(crate::widgets::reverse_search::ReverseSearchState::new(
                &history,
            ));
            vec![]
        }
        Action::ReverseSearchInput(edit) => {
            let history = app.sessions.current().input_history.clone();
            if let Some(ref mut s) = app.reverse_search {
                match edit {
                    PaletteEdit::PushChar(c) => s.push_char(c, &history),
                    PaletteEdit::PopChar => s.pop_char(&history),
                }
            }
            vec![]
        }
        Action::ReverseSearchNext => {
            if let Some(ref mut s) = app.reverse_search {
                s.select_next();
            }
            vec![]
        }
        Action::ReverseSearchPrev => {
            if let Some(ref mut s) = app.reverse_search {
                s.select_previous();
            }
            vec![]
        }
        Action::ReverseSearchAccept => {
            let selected = app.reverse_search.as_ref().and_then(|s| {
                let hist = &app.sessions.current().input_history;
                s.selected_entry(hist).map(str::to_owned)
            });
            app.reverse_search = None;
            if let Some(text) = selected {
                app.sessions.current_mut().input = text;
                app.sessions.current_mut().cursor_position = app.char_count();
            }
            vec![]
        }
        Action::CloseReverseSearch => {
            app.reverse_search = None;
            vec![]
        }

        // ── Confirm dialog ──────────────────────────────────────────────────────
        Action::ConfirmRespond(answer) => {
            if let Some(mut state) = app.confirm_state.take()
                && let Some(tx) = state.response_tx.take()
            {
                let _ = tx.send(answer);
            }
            vec![]
        }

        // ── Elicitation dialog ──────────────────────────────────────────────────
        Action::ElicitationField(edit) => {
            let Some(state) = app.elicitation_state.as_mut() else {
                return vec![];
            };
            match edit {
                ElicitationEdit::PushChar(c) => state.dialog.push_char(c),
                ElicitationEdit::PopChar => state.dialog.pop_char(),
                ElicitationEdit::NextField => state.dialog.next_field(),
                ElicitationEdit::PrevField => state.dialog.prev_field(),
                ElicitationEdit::ToggleBool => state.dialog.toggle_bool(),
                ElicitationEdit::EnumNext => state.dialog.enum_next(),
                ElicitationEdit::EnumPrev => state.dialog.enum_prev(),
            }
            vec![]
        }
        Action::ElicitationSubmit => {
            if let Some(mut state) = app.elicitation_state.take()
                && let Some(value) = state.dialog.build_submission()
                && let Some(tx) = state.response_tx.take()
            {
                let _ = tx.send(ElicitationResponse::Accepted(value));
            }
            vec![]
        }
        Action::ElicitationCancel => {
            if let Some(mut state) = app.elicitation_state.take()
                && let Some(tx) = state.response_tx.take()
            {
                let _ = tx.send(ElicitationResponse::Cancelled);
            }
            vec![]
        }

        // ── Mouse mode ──────────────────────────────────────────────────────────
        Action::SetMouse(enabled) => {
            app.mouse_enabled = enabled;
            let hint = if enabled {
                "Mouse mode: on — text selection via Shift+drag"
            } else {
                "Mouse mode: off"
            };
            app.push_system_message_pub(hint.to_owned());
            vec![Effect::SetMouseCapture(enabled)]
        }

        // ── Clipboard ────────────────────────────────────────────────────────────
        Action::CopyLastAssistant => {
            let text = app.last_assistant_content_pub();
            if let Some(text) = text {
                vec![Effect::CopyToClipboard(text)]
            } else {
                app.push_system_message_pub("No assistant message to copy.".to_owned());
                vec![]
            }
        }
        Action::CopyLastCodeBlock(n) => {
            let blocks = app.last_assistant_code_blocks_pub();
            let text = if blocks.is_empty() {
                None
            } else if n == 0 {
                blocks.last().cloned()
            } else {
                blocks.get(n.saturating_sub(1)).cloned()
            };
            if let Some(text) = text {
                vec![Effect::CopyToClipboard(text)]
            } else {
                app.push_system_message_pub("No code block found.".to_owned());
                vec![]
            }
        }

        // ── Command dispatch ────────────────────────────────────────────────────
        Action::Dispatch(cmd) => {
            match &cmd {
                TuiCommand::Quit => return vec![Effect::Quit],
                TuiCommand::Help => {
                    app.show_help = true;
                    return vec![];
                }
                TuiCommand::SetMotion(m) => {
                    app.motion = *m;
                    let label = match m {
                        zeph_config::Motion::Full => "full (wave animation)",
                        zeph_config::Motion::Minimal => "minimal (breeze spinner)",
                        zeph_config::Motion::Off => "off (static)",
                    };
                    app.push_system_message_pub(format!("Motion set to: {label}"));
                    return vec![];
                }
                TuiCommand::SetMouse(b) => {
                    return reduce(app, Action::SetMouse(*b));
                }
                TuiCommand::ToggleMouse => {
                    let cur = app.mouse_enabled;
                    return reduce(app, Action::SetMouse(!cur));
                }
                _ => {}
            }
            // All other commands are dispatched back to the existing execute_command handler.
            app.execute_command(cmd);
            vec![]
        }
    }
}

/// Execute the side-effects produced by [`reduce`].
///
/// This function is allowed to perform I/O, channel sends, and other effectful
/// operations. It must be called from the event loop, not from inside `reduce`.
pub(crate) fn run_effects(app: &mut App, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::SendUserInput(text) => {
                let _ = app.user_input_tx.try_send(text);
            }
            Effect::SendCommand(cmd) => {
                if let Some(ref tx) = app.command_tx {
                    let _ = tx.try_send(cmd);
                }
            }
            Effect::CopyToClipboard(text) => match app.clipboard.copy(&text) {
                Ok(()) => app.push_system_message_pub("Copied to clipboard.".to_owned()),
                Err(e) => app.push_system_message_pub(format!("Copy failed: {e}")),
            },
            Effect::StartFileIndex => {
                app.open_file_picker();
            }
            Effect::SetMouseCapture(b) => {
                app.pending_mouse_capture = Some(b);
            }
            Effect::Quit => {
                app.should_quit = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::App;

    fn make_app() -> App {
        let (tx, _rx) = mpsc::channel(1);
        let (_atx, arx) = mpsc::channel(1);
        App::new(tx, arx)
    }

    #[test]
    fn scroll_lines_down() {
        let mut app = make_app();
        app.sessions.current_mut().scroll_offset = 5;
        let effects = reduce(&mut app, Action::ScrollLines(3));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().scroll_offset, 2);
    }

    #[test]
    fn scroll_lines_up() {
        let mut app = make_app();
        app.sessions.current_mut().scroll_offset = 2;
        let effects = reduce(&mut app, Action::ScrollLines(-3));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().scroll_offset, 5);
    }

    #[test]
    fn scroll_lines_clamps_at_zero() {
        let mut app = make_app();
        app.sessions.current_mut().scroll_offset = 1;
        let effects = reduce(&mut app, Action::ScrollLines(100));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().scroll_offset, 0);
    }

    #[test]
    fn scroll_to_bottom() {
        let mut app = make_app();
        app.sessions.current_mut().scroll_offset = 42;
        let effects = reduce(&mut app, Action::ScrollToBottom);
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().scroll_offset, 0);
    }

    #[test]
    fn scroll_to_top() {
        let mut app = make_app();
        app.sessions
            .current_mut()
            .messages
            .push(crate::ChatMessage::new(
                crate::MessageRole::User,
                "hello".to_owned(),
            ));
        let effects = reduce(&mut app, Action::ScrollToTop);
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().scroll_offset, 1);
    }

    #[test]
    fn toggle_tool_expanded() {
        let mut app = make_app();
        assert!(!app.tool_expanded);
        let effects = reduce(&mut app, Action::ToggleToolExpanded);
        assert!(effects.is_empty());
        assert!(app.tool_expanded);
    }

    #[test]
    fn toggle_side_panels() {
        let mut app = make_app();
        assert!(app.show_side_panels);
        reduce(&mut app, Action::ToggleSidePanels);
        assert!(!app.show_side_panels);
        reduce(&mut app, Action::ToggleSidePanels);
        assert!(app.show_side_panels);
    }

    #[test]
    fn toggle_help() {
        let mut app = make_app();
        assert!(!app.show_help);
        reduce(&mut app, Action::ToggleHelp);
        assert!(app.show_help);
        reduce(&mut app, Action::ToggleHelp);
        assert!(!app.show_help);
    }

    #[test]
    fn set_help_explicit() {
        let mut app = make_app();
        reduce(&mut app, Action::SetHelp(true));
        assert!(app.show_help);
        reduce(&mut app, Action::SetHelp(false));
        assert!(!app.show_help);
    }

    #[test]
    fn quit_emits_effect() {
        let mut app = make_app();
        let effects = reduce(&mut app, Action::Quit);
        assert!(matches!(effects.as_slice(), [Effect::Quit]));
    }

    #[test]
    fn set_mouse_emits_capture_effect() {
        let mut app = make_app();
        let effects = reduce(&mut app, Action::SetMouse(true));
        assert!(app.mouse_enabled);
        assert!(matches!(
            effects.as_slice(),
            [Effect::SetMouseCapture(true)]
        ));
    }

    #[test]
    fn set_mouse_off_emits_disable() {
        let mut app = make_app();
        app.mouse_enabled = true;
        let effects = reduce(&mut app, Action::SetMouse(false));
        assert!(!app.mouse_enabled);
        assert!(matches!(
            effects.as_slice(),
            [Effect::SetMouseCapture(false)]
        ));
    }

    #[test]
    fn run_effects_set_mouse_capture_stores_pending() {
        let mut app = make_app();
        run_effects(&mut app, vec![Effect::SetMouseCapture(true)]);
        assert_eq!(app.pending_mouse_capture, Some(true));
    }

    #[test]
    fn run_effects_quit_sets_should_quit() {
        let mut app = make_app();
        run_effects(&mut app, vec![Effect::Quit]);
        assert!(app.should_quit);
    }

    #[test]
    fn enter_insert_sets_mode() {
        let mut app = make_app();
        let effects = reduce(&mut app, Action::EnterInsert);
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().input_mode, InputMode::Insert);
    }

    #[test]
    fn cycle_panel_focus_wraps() {
        let mut app = make_app();
        assert_eq!(app.active_panel, Panel::Chat);
        reduce(&mut app, Action::CyclePanelFocus);
        assert_eq!(app.active_panel, Panel::Skills);
    }

    #[test]
    fn dispatch_toggle_mouse_flips_flag() {
        let mut app = make_app();
        assert!(!app.mouse_enabled);
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ToggleMouse));
        assert!(app.mouse_enabled);
        assert!(matches!(
            effects.as_slice(),
            [Effect::SetMouseCapture(true)]
        ));
    }
}
