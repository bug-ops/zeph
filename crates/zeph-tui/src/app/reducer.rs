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
use super::{App, ChatMessage, InputMode, MessageRole, Panel, format_security_report};
use crate::command::TuiCommand;
use crate::file_picker::FilePickerState;
use crate::widgets::command_palette::CommandPaletteState;
use crate::widgets::slash_autocomplete::SlashAutocompleteState;

const MAX_INPUT_HISTORY: usize = 500;

/// A side-effect that `reduce` defers to `run_effects` for execution.
///
/// Effects represent work that cannot be done inside the reducer because it
/// requires I/O, channel sends, or borrowed state from outside `App`
/// (e.g. terminal backend handles).
#[derive(Debug, PartialEq)]
pub(crate) enum Effect {
    /// Forward the user's typed text to the agent loop.
    SendUserInput(String),
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
            let next = match app.active_panel {
                Panel::Chat => Panel::Skills,
                Panel::Skills => Panel::Memory,
                Panel::Memory => Panel::Resources,
                Panel::Resources => Panel::SubAgents,
                Panel::SubAgents | Panel::Tasks => Panel::Fleet,
                Panel::Fleet => Panel::Durable,
                Panel::Durable => Panel::Settings,
                Panel::Settings => Panel::Chat,
            };
            // Routed through set_active_panel so cycling into SubAgents/Fleet/Durable/
            // Settings clears show_task_panel the same way every other entry point does
            // (#6061).
            app.set_active_panel(next);
            vec![]
        }
        Action::SetActivePanel(p) => {
            app.set_active_panel(p);
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
            if app.show_task_panel {
                app.show_task_panel = false;
            } else {
                // Claim active_panel so Fleet/Durable are displaced instead of stacking (#6061).
                app.set_active_panel(Panel::Tasks);
            }
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
            // Check for local session slash commands first. Route through
            // `Action::Dispatch` (the same path used by the command palette and
            // slash autocomplete) so every `TuiCommand` variant gets its correct
            // in-process handling instead of being force-fed to the agent bridge.
            if let Some(cmd) = App::parse_session_slash_pub(&text) {
                app.sessions.current_mut().input.clear();
                app.sessions.current_mut().cursor_position = 0;
                app.sessions.current_mut().history_index = None;
                app.sessions.current_mut().draft_input.clear();
                app.sessions.current_mut().paste_state = None;
                return reduce(app, Action::Dispatch(cmd));
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
            // Dispatch the selected entry's command directly rather than
            // reconstituting slash text and re-parsing it: most registry ids
            // (e.g. `skill:list`) have no textual form the parser recognizes,
            // so a round-trip through text silently sent the raw text as a
            // chat message instead of running the command (#5779).
            let cmd = app
                .slash_autocomplete
                .as_ref()
                .and_then(SlashAutocompleteState::selected_entry)
                .map(|e| e.command.clone());
            app.slash_autocomplete = None;
            app.sessions.current_mut().input.clear();
            app.sessions.current_mut().cursor_position = 0;
            if let Some(cmd) = cmd {
                return reduce(app, Action::Dispatch(cmd));
            }
            vec![]
        }
        Action::SlashAutocompleteAcceptAndSubmit => {
            // Dispatch already executes the command (or prompts for further
            // input via `prefill_input`), so no separate submit step is needed.
            reduce(app, Action::SlashAutocompleteAccept)
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

        // ── Transcript search (issue #6023) ─────────────────────────────────────
        Action::OpenTranscriptSearch => {
            let pre_search_scroll_offset = app.scroll_offset();
            app.transcript_search = Some(
                crate::widgets::transcript_search::TranscriptSearchState::new(
                    pre_search_scroll_offset,
                ),
            );
            vec![]
        }
        Action::TranscriptSearchInput(edit) => {
            let messages = app.visible_messages();
            if let Some(ref mut s) = app.transcript_search {
                match edit {
                    PaletteEdit::PushChar(c) => s.push_char(c, &messages),
                    PaletteEdit::PopChar => s.pop_char(&messages),
                }
            }
            if let Some(target) = app.transcript_search.as_ref().and_then(
                crate::widgets::transcript_search::TranscriptSearchState::selected_message_index,
            ) && let Some(offset) = app.line_offset_of_message(target)
            {
                app.begin_scroll(offset);
            }
            vec![]
        }
        Action::TranscriptSearchNext => {
            if let Some(ref mut s) = app.transcript_search {
                s.select_next();
            }
            if let Some(target) = app.transcript_search.as_ref().and_then(
                crate::widgets::transcript_search::TranscriptSearchState::selected_message_index,
            ) && let Some(offset) = app.line_offset_of_message(target)
            {
                app.begin_scroll(offset);
            }
            vec![]
        }
        Action::TranscriptSearchPrev => {
            if let Some(ref mut s) = app.transcript_search {
                s.select_previous();
            }
            if let Some(target) = app.transcript_search.as_ref().and_then(
                crate::widgets::transcript_search::TranscriptSearchState::selected_message_index,
            ) && let Some(offset) = app.line_offset_of_message(target)
            {
                app.begin_scroll(offset);
            }
            vec![]
        }
        Action::TranscriptSearchAccept => {
            // Leave the transcript scrolled at the accepted match (FR-007) — only the
            // overlay closes, scroll_offset is left as-is.
            app.transcript_search = None;
            vec![]
        }
        Action::CloseTranscriptSearch => {
            let restore = app
                .transcript_search
                .as_ref()
                .map(|s| s.pre_search_scroll_offset);
            app.transcript_search = None;
            if let Some(offset) = restore {
                app.begin_scroll(offset);
            }
            vec![]
        }

        // ── Settings view (issue #6024) ─────────────────────────────────────────
        Action::SettingsTabNext => {
            app.settings.next_tab();
            vec![]
        }
        Action::SettingsTabPrev => {
            app.settings.previous_tab();
            vec![]
        }
        Action::SettingsSelectMove(dir) => {
            let count = app.settings_active_tab_len();
            match dir {
                VertDir::Down => app.settings.select_next(count),
                VertDir::Up => app.settings.select_previous(count),
            }
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
                TuiCommand::ToggleEqualizer => {
                    app.show_equalizer = !app.show_equalizer;
                    return vec![];
                }

                // ── Group A — pure state mutations ──────────────────────────────
                TuiCommand::NewSession => {
                    app.sessions.current_mut().messages.clear();
                    app.push_system_message_pub("New conversation started.".to_owned());
                    return vec![];
                }
                TuiCommand::TaskPanel => {
                    if app.show_task_panel {
                        app.show_task_panel = false;
                    } else {
                        // Claim active_panel so Fleet/Durable are displaced instead of
                        // stacking on the task panel (#6061).
                        app.set_active_panel(Panel::Tasks);
                    }
                    return vec![];
                }
                TuiCommand::FleetPanel => {
                    app.set_active_panel(Panel::Fleet);
                    return vec![];
                }
                TuiCommand::DurablePanel => {
                    app.set_active_panel(Panel::Durable);
                    return vec![];
                }
                TuiCommand::Settings => {
                    app.set_active_panel(Panel::Settings);
                    return vec![];
                }
                TuiCommand::TranscriptSearch => {
                    return reduce(app, Action::OpenTranscriptSearch);
                }
                TuiCommand::PlanToggleView => {
                    app.sessions.current_mut().plan_view_active =
                        !app.sessions.current().plan_view_active;
                    return vec![];
                }
                TuiCommand::SubagentSidebarDown => {
                    let count = app.metrics.sub_agents.len();
                    app.subagent_sidebar.select_next(count);
                    return vec![];
                }
                TuiCommand::SubagentSidebarUp => {
                    let count = app.metrics.sub_agents.len();
                    app.subagent_sidebar.select_prev(count);
                    return vec![];
                }
                TuiCommand::ListThemes => {
                    app.push_system_message_pub(
                        "Available themes: zephyr, zephyr-light, high-contrast, classic, \
                         catppuccin-mocha, gruvbox-dark, solarized-dark\n\
                         Usage: /theme <name>"
                            .to_owned(),
                    );
                    return vec![];
                }
                TuiCommand::ViewFilters => {
                    app.push_system_message_pub(
                        "Filter statistics are displayed in the Resources panel.".to_owned(),
                    );
                    return vec![];
                }
                TuiCommand::Ingest => {
                    app.push_system_message_pub(
                        "Use: zeph ingest <path> [--chunk-size N] [--collection NAME]".to_owned(),
                    );
                    return vec![];
                }
                TuiCommand::GatewayStatus => {
                    app.push_system_message_pub(
                        "Gateway status is not yet available in TUI mode.".to_owned(),
                    );
                    return vec![];
                }
                TuiCommand::DaemonStatus => {
                    let msg = match app.remote_daemon_url() {
                        Some(url) => format!("Connected to remote daemon at {url}"),
                        None => "Running in local mode (not connected to a remote daemon).\n\
                                 Use `zeph --tui --connect <URL>` to attach to a remote daemon."
                            .to_owned(),
                    };
                    app.push_system_message_pub(msg);
                    return vec![];
                }
                TuiCommand::DaemonConnect => {
                    app.push_system_message_pub(
                        "Live in-session daemon attach is not supported yet.\n\
                         To connect to a remote daemon, restart with:\n  zeph --tui --connect <URL>"
                            .to_owned(),
                    );
                    return vec![];
                }
                TuiCommand::DaemonDisconnect => {
                    app.push_system_message_pub(
                        "There is no live daemon connection to tear down in this mode.\n\
                         If you started with `--connect <URL>`, quit the TUI (q / Ctrl+C) to disconnect."
                            .to_owned(),
                    );
                    return vec![];
                }
                TuiCommand::MigrateConfig => {
                    app.push_system_message_pub(
                        "To preview missing config parameters, run:\n  zeph migrate-config --diff\n\
                         To apply changes in-place:\n  zeph migrate-config --in-place"
                            .to_owned(),
                    );
                    return vec![];
                }
                TuiCommand::KnowledgeIngestPrompt => {
                    app.push_system_message_pub(
                        "To ingest project artifacts: run \
                         `zeph knowledge ingest --source <specs|changelog|handoff|coverage|git-log>` \
                         from the CLI."
                            .to_owned(),
                    );
                    return vec![];
                }
                // ── Group B — pure formatter reads ──────────────────────────────
                TuiCommand::SkillList => {
                    let msg = app.format_skill_list();
                    app.push_system_message_pub(msg);
                    return vec![];
                }
                TuiCommand::McpList => {
                    let msg = app.format_mcp_list();
                    app.push_system_message_pub(msg);
                    return vec![];
                }
                TuiCommand::MemoryStats => {
                    let msg = app.format_memory_stats();
                    app.push_system_message_pub(msg);
                    return vec![];
                }
                TuiCommand::ViewCost => {
                    let msg = app.format_cost_stats();
                    app.push_system_message_pub(msg);
                    return vec![];
                }
                TuiCommand::ViewTools => {
                    let msg = app.format_tool_list();
                    app.push_system_message_pub(msg);
                    return vec![];
                }
                TuiCommand::ViewLatency => {
                    let msg = app.format_latency_stats();
                    app.push_system_message_pub(msg);
                    return vec![];
                }
                TuiCommand::SchedulerList => {
                    let msg = app.format_scheduler_list();
                    app.push_system_message_pub(msg);
                    return vec![];
                }
                TuiCommand::RouterStats => {
                    let msg = app.format_router_stats();
                    app.push_system_message_pub(msg);
                    return vec![];
                }
                TuiCommand::SecurityEvents => {
                    let msg = format_security_report(&app.metrics);
                    app.push_system_message_pub(msg);
                    return vec![];
                }

                // ── Group C — fixed-string agent input sends ────────────────────
                TuiCommand::PlanStatus => {
                    return vec![Effect::SendUserInput("/plan status".to_owned())];
                }
                TuiCommand::PlanConfirm => {
                    return vec![Effect::SendUserInput("/plan confirm".to_owned())];
                }
                TuiCommand::PlanCancel => {
                    return vec![Effect::SendUserInput("/plan cancel".to_owned())];
                }
                TuiCommand::PlanList => {
                    return vec![Effect::SendUserInput("/plan list".to_owned())];
                }
                TuiCommand::ExperimentStop => {
                    return vec![Effect::SendUserInput("/experiment stop".to_owned())];
                }
                TuiCommand::ExperimentStatus => {
                    return vec![Effect::SendUserInput("/experiment status".to_owned())];
                }
                TuiCommand::ExperimentReport => {
                    return vec![Effect::SendUserInput("/experiment report".to_owned())];
                }
                TuiCommand::ExperimentBest => {
                    return vec![Effect::SendUserInput("/experiment best".to_owned())];
                }
                TuiCommand::ServerCompactionStatus => {
                    return vec![Effect::SendUserInput("/server-compaction".to_owned())];
                }
                TuiCommand::ViewGuidelines => {
                    return vec![Effect::SendUserInput("/guidelines".to_owned())];
                }
                TuiCommand::ForgettingSweep => {
                    return vec![Effect::SendUserInput("/forgetting-sweep".to_owned())];
                }
                TuiCommand::TrajectoryStats => {
                    return vec![Effect::SendUserInput("/memory trajectory".to_owned())];
                }
                TuiCommand::WorktreeList => {
                    return vec![Effect::SendUserInput("/worktree list".to_owned())];
                }
                TuiCommand::WorktreeClean => {
                    return vec![Effect::SendUserInput("/worktree clean".to_owned())];
                }
                TuiCommand::MemoryTreeStats => {
                    return vec![Effect::SendUserInput("/memory tree".to_owned())];
                }
                TuiCommand::ViewLog => {
                    return vec![Effect::SendUserInput("/log".to_owned())];
                }
                TuiCommand::Undo => {
                    return vec![Effect::SendUserInput("/undo".to_owned())];
                }
                TuiCommand::Redo => {
                    return vec![Effect::SendUserInput("/redo".to_owned())];
                }
                TuiCommand::SendClearQueue => {
                    return vec![Effect::SendUserInput("/clear-queue".to_owned())];
                }
                TuiCommand::SendVerbatim(text) => {
                    return vec![Effect::SendUserInput(text.clone())];
                }
                TuiCommand::PrefillVerbatim(text) => {
                    app.sessions.current_mut().input.clear();
                    app.sessions.current_mut().input.push_str(text);
                    app.sessions.current_mut().cursor_position = app.char_count();
                    return vec![];
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
    use std::assert_matches;
    use tokio::sync::mpsc;

    use super::*;
    use crate::App;

    fn make_app() -> (App, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(32);
        let (_atx, arx) = mpsc::channel(1);
        (App::new(tx, arx), rx)
    }

    #[test]
    fn scroll_lines_down() {
        let (mut app, _rx) = make_app();
        app.sessions.current_mut().scroll_offset = 5;
        let effects = reduce(&mut app, Action::ScrollLines(3));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().scroll_offset, 2);
    }

    #[test]
    fn scroll_lines_up() {
        let (mut app, _rx) = make_app();
        app.sessions.current_mut().scroll_offset = 2;
        let effects = reduce(&mut app, Action::ScrollLines(-3));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().scroll_offset, 5);
    }

    #[test]
    fn scroll_lines_clamps_at_zero() {
        let (mut app, _rx) = make_app();
        app.sessions.current_mut().scroll_offset = 1;
        let effects = reduce(&mut app, Action::ScrollLines(100));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().scroll_offset, 0);
    }

    #[test]
    fn scroll_to_bottom() {
        let (mut app, _rx) = make_app();
        app.sessions.current_mut().scroll_offset = 42;
        let effects = reduce(&mut app, Action::ScrollToBottom);
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().scroll_offset, 0);
    }

    #[test]
    fn scroll_to_top() {
        let (mut app, _rx) = make_app();
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
        let (mut app, _rx) = make_app();
        assert!(!app.tool_expanded);
        let effects = reduce(&mut app, Action::ToggleToolExpanded);
        assert!(effects.is_empty());
        assert!(app.tool_expanded);
    }

    #[test]
    fn toggle_side_panels() {
        let (mut app, _rx) = make_app();
        assert!(app.show_side_panels);
        reduce(&mut app, Action::ToggleSidePanels);
        assert!(!app.show_side_panels);
        reduce(&mut app, Action::ToggleSidePanels);
        assert!(app.show_side_panels);
    }

    #[test]
    fn toggle_help() {
        let (mut app, _rx) = make_app();
        assert!(!app.show_help);
        reduce(&mut app, Action::ToggleHelp);
        assert!(app.show_help);
        reduce(&mut app, Action::ToggleHelp);
        assert!(!app.show_help);
    }

    #[test]
    fn set_help_explicit() {
        let (mut app, _rx) = make_app();
        reduce(&mut app, Action::SetHelp(true));
        assert!(app.show_help);
        reduce(&mut app, Action::SetHelp(false));
        assert!(!app.show_help);
    }

    #[test]
    fn quit_emits_effect() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Quit);
        assert_matches!(effects.as_slice(), [Effect::Quit]);
    }

    #[test]
    fn set_mouse_emits_capture_effect() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::SetMouse(true));
        assert!(app.mouse_enabled);
        assert_matches!(effects.as_slice(), [Effect::SetMouseCapture(true)]);
    }

    #[test]
    fn set_mouse_off_emits_disable() {
        let (mut app, _rx) = make_app();
        app.mouse_enabled = true;
        let effects = reduce(&mut app, Action::SetMouse(false));
        assert!(!app.mouse_enabled);
        assert_matches!(effects.as_slice(), [Effect::SetMouseCapture(false)]);
    }

    #[test]
    fn run_effects_set_mouse_capture_stores_pending() {
        let (mut app, _rx) = make_app();
        run_effects(&mut app, vec![Effect::SetMouseCapture(true)]);
        assert_eq!(app.pending_mouse_capture, Some(true));
    }

    #[test]
    fn run_effects_quit_sets_should_quit() {
        let (mut app, _rx) = make_app();
        run_effects(&mut app, vec![Effect::Quit]);
        assert!(app.should_quit);
    }

    #[test]
    fn enter_insert_sets_mode() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::EnterInsert);
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().input_mode, InputMode::Insert);
    }

    #[test]
    fn cycle_panel_focus_wraps() {
        let (mut app, _rx) = make_app();
        assert_eq!(app.active_panel, Panel::Chat);
        reduce(&mut app, Action::CyclePanelFocus);
        assert_eq!(app.active_panel, Panel::Skills);
    }

    // ── #6061 CyclePanelFocus (Tab key) must honor the mutual-exclusion invariant ──
    //
    // Flagged in the first coverage pass as untested: Tab-cycling into SubAgents/Fleet/
    // Durable while the task panel is open used to leave both "active" simultaneously,
    // since CyclePanelFocus assigned `active_panel` directly instead of going through
    // set_active_panel like every other entry point. Now centralized — these confirm
    // the fix closes that gap.

    #[test]
    fn cycle_panel_focus_into_subagents_clears_task_panel() {
        let (mut app, _rx) = make_app();
        app.active_panel = Panel::Resources;
        app.show_task_panel = true;
        reduce(&mut app, Action::CyclePanelFocus);
        assert_eq!(app.active_panel, Panel::SubAgents);
        assert!(!app.show_task_panel);
    }

    #[test]
    fn cycle_panel_focus_into_fleet_clears_task_panel() {
        let (mut app, _rx) = make_app();
        app.active_panel = Panel::SubAgents;
        app.show_task_panel = true;
        reduce(&mut app, Action::CyclePanelFocus);
        assert_eq!(app.active_panel, Panel::Fleet);
        assert!(!app.show_task_panel);
    }

    #[test]
    fn cycle_panel_focus_into_durable_clears_task_panel() {
        let (mut app, _rx) = make_app();
        app.active_panel = Panel::Fleet;
        app.show_task_panel = true;
        reduce(&mut app, Action::CyclePanelFocus);
        assert_eq!(app.active_panel, Panel::Durable);
        assert!(!app.show_task_panel);
    }

    #[test]
    fn cycle_panel_focus_into_skills_does_not_clear_task_panel() {
        // Negative case: cycling into a panel that does NOT share the subagents Rect
        // must leave show_task_panel untouched.
        let (mut app, _rx) = make_app();
        app.active_panel = Panel::Chat;
        app.show_task_panel = true;
        reduce(&mut app, Action::CyclePanelFocus);
        assert_eq!(app.active_panel, Panel::Skills);
        assert!(app.show_task_panel);
    }

    #[test]
    fn dispatch_toggle_mouse_flips_flag() {
        let (mut app, _rx) = make_app();
        assert!(!app.mouse_enabled);
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ToggleMouse));
        assert!(app.mouse_enabled);
        assert_matches!(effects.as_slice(), [Effect::SetMouseCapture(true)]);
    }

    // ── Group A tests ───────────────────────────────────────────────────────────

    #[test]
    fn dispatch_new_session_clears_messages() {
        let (mut app, _rx) = make_app();
        app.sessions
            .current_mut()
            .messages
            .push(ChatMessage::new(MessageRole::User, "hello".to_owned()));
        assert!(!app.sessions.current().messages.is_empty());
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::NewSession));
        assert!(effects.is_empty());
        // cleared, then exactly one system message appended
        assert_eq!(app.sessions.current().messages.len(), 1);
        assert_matches!(app.sessions.current().messages[0].role, MessageRole::System);
    }

    #[test]
    fn dispatch_new_session_fires_once() {
        // double-exec guard: if the arm lacked return, execute_command would also fire
        let (mut app, _rx) = make_app();
        reduce(&mut app, Action::Dispatch(TuiCommand::NewSession));
        // exactly one system message — not two
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_task_panel_toggles() {
        let (mut app, _rx) = make_app();
        assert!(!app.show_task_panel);
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::TaskPanel));
        assert!(effects.is_empty());
        assert!(app.show_task_panel);
        // exactly-once: toggle flipped once, not twice
        reduce(&mut app, Action::Dispatch(TuiCommand::TaskPanel));
        assert!(!app.show_task_panel);
    }

    #[test]
    fn dispatch_fleet_panel_sets_panel() {
        let (mut app, _rx) = make_app();
        assert_eq!(app.active_panel, Panel::Chat);
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::FleetPanel));
        assert!(effects.is_empty());
        assert_eq!(app.active_panel, Panel::Fleet);
    }

    #[test]
    fn dispatch_durable_panel_sets_panel() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::DurablePanel));
        assert!(effects.is_empty());
        assert_eq!(app.active_panel, Panel::Durable);
    }

    // ── #6061 active_panel / show_task_panel mutual exclusion ──────────────────

    #[test]
    fn dispatch_fleet_panel_clears_task_panel() {
        let (mut app, _rx) = make_app();
        app.show_task_panel = true;
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::FleetPanel));
        assert!(effects.is_empty());
        assert_eq!(app.active_panel, Panel::Fleet);
        assert!(
            !app.show_task_panel,
            "activating Fleet must clear the task-panel overlay so it can't bleed \
             through onto the shared Rect"
        );
    }

    #[test]
    fn dispatch_durable_panel_clears_task_panel() {
        let (mut app, _rx) = make_app();
        app.show_task_panel = true;
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::DurablePanel));
        assert!(effects.is_empty());
        assert_eq!(app.active_panel, Panel::Durable);
        assert!(
            !app.show_task_panel,
            "activating Durable must clear the task-panel overlay so it can't bleed \
             through onto the shared Rect"
        );
    }

    #[test]
    fn dispatch_task_panel_toggle_on_claims_active_panel() {
        let (mut app, _rx) = make_app();
        app.active_panel = Panel::Fleet;
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::TaskPanel));
        assert!(effects.is_empty());
        assert!(app.show_task_panel);
        assert_eq!(
            app.active_panel,
            Panel::Tasks,
            "toggling the task panel on must displace whatever panel (Fleet/Durable) \
             previously owned the shared Rect"
        );
    }

    #[test]
    fn dispatch_task_panel_toggle_off_leaves_active_panel_untouched() {
        let (mut app, _rx) = make_app();
        app.show_task_panel = true;
        app.active_panel = Panel::Tasks;
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::TaskPanel));
        assert!(effects.is_empty());
        assert!(!app.show_task_panel);
        // Toggling off must not itself reassign active_panel — only the "on" transition claims it.
        assert_eq!(app.active_panel, Panel::Tasks);
    }

    #[test]
    fn action_set_active_panel_fleet_clears_task_panel() {
        let (mut app, _rx) = make_app();
        app.show_task_panel = true;
        let effects = reduce(&mut app, Action::SetActivePanel(Panel::Fleet));
        assert!(effects.is_empty());
        assert_eq!(app.active_panel, Panel::Fleet);
        assert!(
            !app.show_task_panel,
            "SetActivePanel(Fleet) must clear show_task_panel (covers the `f` keyboard \
             shortcut path, distinct from the TuiCommand::FleetPanel command-palette path)"
        );
    }

    #[test]
    fn action_set_active_panel_durable_clears_task_panel() {
        let (mut app, _rx) = make_app();
        app.show_task_panel = true;
        let effects = reduce(&mut app, Action::SetActivePanel(Panel::Durable));
        assert!(effects.is_empty());
        assert_eq!(app.active_panel, Panel::Durable);
        assert!(
            !app.show_task_panel,
            "SetActivePanel(Durable) must clear show_task_panel (covers the `D` keyboard \
             shortcut path)"
        );
    }

    #[test]
    fn action_set_active_panel_subagents_clears_task_panel() {
        // S1: SubAgents renders as the interactive base layer of the same shared Rect as
        // Fleet/Durable (not just an overlay), so it must also displace the task panel.
        // Exercised through the full reduce() path (not just the App::set_active_panel
        // unit test) since this arm has SubAgents-specific side effects (sidebar
        // selection) that could theoretically interfere with the invariant.
        let (mut app, _rx) = make_app();
        app.show_task_panel = true;
        let effects = reduce(&mut app, Action::SetActivePanel(Panel::SubAgents));
        assert!(effects.is_empty());
        assert_eq!(app.active_panel, Panel::SubAgents);
        assert!(
            !app.show_task_panel,
            "SetActivePanel(SubAgents) must clear show_task_panel (covers the `a` keyboard \
             shortcut path)"
        );
    }

    #[test]
    fn action_set_active_panel_chat_does_not_clear_task_panel() {
        // Negative case: only Fleet/Durable share the subagents Rect with the task panel —
        // switching to an unrelated panel must not incidentally clear show_task_panel.
        let (mut app, _rx) = make_app();
        app.show_task_panel = true;
        let effects = reduce(&mut app, Action::SetActivePanel(Panel::Skills));
        assert!(effects.is_empty());
        assert_eq!(app.active_panel, Panel::Skills);
        assert!(app.show_task_panel);
    }

    #[test]
    fn action_toggle_task_panel_on_claims_active_panel() {
        let (mut app, _rx) = make_app();
        app.active_panel = Panel::Durable;
        let effects = reduce(&mut app, Action::ToggleTaskPanel);
        assert!(effects.is_empty());
        assert!(app.show_task_panel);
        assert_eq!(
            app.active_panel,
            Panel::Tasks,
            "the `t` keyboard shortcut (Action::ToggleTaskPanel) must displace Fleet/Durable \
             the same way TuiCommand::TaskPanel does"
        );
    }

    #[test]
    fn dispatch_plan_toggle_view_flips_flag() {
        let (mut app, _rx) = make_app();
        assert!(!app.sessions.current().plan_view_active);
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::PlanToggleView));
        assert!(effects.is_empty());
        assert!(app.sessions.current().plan_view_active);
    }

    #[test]
    fn dispatch_subagent_sidebar_down_advances_selection() {
        let (mut app, _rx) = make_app();
        // populate sub_agents so count > 0
        app.metrics.sub_agents = vec![
            zeph_core::metrics::SubAgentMetrics::default(),
            zeph_core::metrics::SubAgentMetrics::default(),
        ];
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::SubagentSidebarDown));
        assert!(effects.is_empty());
        // selection must have advanced — sidebar wraps within count
        assert!(app.subagent_sidebar.selected().is_some());
    }

    #[test]
    fn dispatch_subagent_sidebar_up_decrements_selection() {
        let (mut app, _rx) = make_app();
        app.metrics.sub_agents = vec![
            zeph_core::metrics::SubAgentMetrics::default(),
            zeph_core::metrics::SubAgentMetrics::default(),
        ];
        // prime selection at index 1
        app.subagent_sidebar.list_state.select(Some(1));
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::SubagentSidebarUp));
        assert!(effects.is_empty());
    }

    #[test]
    fn dispatch_list_themes_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ListThemes));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
        assert_matches!(app.sessions.current().messages[0].role, MessageRole::System);
    }

    #[test]
    fn dispatch_view_filters_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ViewFilters));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_ingest_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::Ingest));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_gateway_status_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::GatewayStatus));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_daemon_connect_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::DaemonConnect));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_migrate_config_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::MigrateConfig));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_knowledge_ingest_prompt_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(
            &mut app,
            Action::Dispatch(TuiCommand::KnowledgeIngestPrompt),
        );
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    // ── Group B tests ───────────────────────────────────────────────────────────

    #[test]
    fn dispatch_skill_list_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::SkillList));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_mcp_list_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::McpList));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_memory_stats_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::MemoryStats));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_view_cost_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ViewCost));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_view_tools_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ViewTools));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_view_latency_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ViewLatency));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_scheduler_list_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::SchedulerList));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_router_stats_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::RouterStats));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_security_events_pushes_system_message() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::SecurityEvents));
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().messages.len(), 1);
    }

    #[test]
    fn dispatch_daemon_status_reports_local_mode_when_disconnected() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::DaemonStatus));
        assert!(effects.is_empty());
        let msg = &app.sessions.current().messages.last().unwrap().content;
        assert!(msg.contains("local mode"));
        assert!(msg.contains("--connect"));
    }

    #[test]
    fn dispatch_daemon_status_reports_connected_when_remote_url_set() {
        let (mut app, _rx) = make_app();
        app = app.with_remote_daemon_url("http://example.com:9000");
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::DaemonStatus));
        assert!(effects.is_empty());
        let msg = &app.sessions.current().messages.last().unwrap().content;
        assert!(msg.contains("Connected"));
        assert!(msg.contains("http://example.com:9000"));
    }

    #[test]
    fn dispatch_daemon_connect_and_disconnect_give_distinct_messages() {
        let (mut app, _rx) = make_app();
        reduce(&mut app, Action::Dispatch(TuiCommand::DaemonConnect));
        let connect_msg = app
            .sessions
            .current()
            .messages
            .last()
            .unwrap()
            .content
            .clone();

        reduce(&mut app, Action::Dispatch(TuiCommand::DaemonDisconnect));
        let disconnect_msg = app
            .sessions
            .current()
            .messages
            .last()
            .unwrap()
            .content
            .clone();

        assert_ne!(connect_msg, disconnect_msg);
        assert!(connect_msg.contains("--connect"));
        assert!(disconnect_msg.contains("quit"));
    }

    // ── Group C tests ───────────────────────────────────────────────────────────

    #[test]
    fn dispatch_plan_status_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::PlanStatus));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/plan status".to_owned())]
        );
    }

    #[test]
    fn dispatch_plan_confirm_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::PlanConfirm));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/plan confirm".to_owned())]
        );
    }

    #[test]
    fn dispatch_plan_cancel_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::PlanCancel));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/plan cancel".to_owned())]
        );
    }

    #[test]
    fn dispatch_plan_list_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::PlanList));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/plan list".to_owned())]
        );
    }

    #[test]
    fn dispatch_experiment_stop_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ExperimentStop));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/experiment stop".to_owned())]
        );
    }

    #[test]
    fn dispatch_experiment_status_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ExperimentStatus));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/experiment status".to_owned())]
        );
    }

    #[test]
    fn dispatch_experiment_report_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ExperimentReport));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/experiment report".to_owned())]
        );
    }

    #[test]
    fn dispatch_experiment_best_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ExperimentBest));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/experiment best".to_owned())]
        );
    }

    #[test]
    fn dispatch_server_compaction_status_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(
            &mut app,
            Action::Dispatch(TuiCommand::ServerCompactionStatus),
        );
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/server-compaction".to_owned())]
        );
    }

    #[test]
    fn dispatch_view_guidelines_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ViewGuidelines));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/guidelines".to_owned())]
        );
    }

    #[test]
    fn dispatch_forgetting_sweep_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ForgettingSweep));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/forgetting-sweep".to_owned())]
        );
    }

    #[test]
    fn dispatch_trajectory_stats_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::TrajectoryStats));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/memory trajectory".to_owned())]
        );
    }

    // ── #6132 WorktreeList/WorktreeClean now forward to the real /worktree
    //    slash command instead of the static CLI-redirect message from #6131 ──

    #[test]
    fn dispatch_worktree_list_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::WorktreeList));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/worktree list".to_owned())]
        );
    }

    #[test]
    fn dispatch_worktree_clean_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::WorktreeClean));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/worktree clean".to_owned())]
        );
    }

    #[test]
    fn dispatch_memory_tree_stats_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::MemoryTreeStats));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/memory tree".to_owned())]
        );
    }

    #[test]
    fn dispatch_view_log_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ViewLog));
        assert_eq!(effects, vec![Effect::SendUserInput("/log".to_owned())]);
    }

    #[test]
    fn dispatch_undo_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::Undo));
        assert_eq!(effects, vec![Effect::SendUserInput("/undo".to_owned())]);
    }

    #[test]
    fn dispatch_redo_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::Redo));
        assert_eq!(effects, vec![Effect::SendUserInput("/redo".to_owned())]);
    }

    #[test]
    fn dispatch_send_clear_queue_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::SendClearQueue));
        assert_eq!(
            effects,
            vec![Effect::SendUserInput("/clear-queue".to_owned())]
        );
    }

    #[test]
    fn dispatch_send_verbatim_sends_slash_command() {
        let (mut app, _rx) = make_app();
        let effects = reduce(
            &mut app,
            Action::Dispatch(TuiCommand::SendVerbatim("/model".to_owned())),
        );
        assert_eq!(effects, vec![Effect::SendUserInput("/model".to_owned())]);
    }

    #[test]
    fn dispatch_prefill_verbatim_fills_input_without_submitting() {
        // #5875 F1: mandatory-argument zeph_commands entries (e.g. /image) must prefill
        // rather than submit an incomplete command.
        let (mut app, _rx) = make_app();
        let effects = reduce(
            &mut app,
            Action::Dispatch(TuiCommand::PrefillVerbatim("/image ".to_owned())),
        );
        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().input, "/image ");
        assert!(
            app.sessions
                .current()
                .messages
                .iter()
                .all(|m| m.role != MessageRole::User),
            "prefilled command must not be sent as a chat message"
        );
    }

    #[test]
    fn run_effects_send_user_input_sends_to_channel() {
        let (mut app, mut rx) = make_app();
        run_effects(
            &mut app,
            vec![Effect::SendUserInput("/plan status".to_owned())],
        );
        // Effect sent exactly one message — channel has the value
        let msg = rx.try_recv().expect("channel must have one message");
        assert_eq!(msg, "/plan status");
        // No second message (exactly-once)
        assert!(rx.try_recv().is_err());
    }

    // ── Slash autocomplete accept dispatches directly (#5779) ───────────────

    fn select_entry(app: &mut App, query: &str, expected_id: &str) {
        let mut state = SlashAutocompleteState::new();
        for c in query.chars() {
            state.push_char(c);
        }
        assert_eq!(
            state.selected_entry().map(|e| e.id),
            Some(expected_id),
            "query {query:?} must resolve to {expected_id:?} as the top match"
        );
        app.slash_autocomplete = Some(state);
    }

    #[test]
    fn slash_autocomplete_accept_dispatches_command_directly() {
        let (mut app, _rx) = make_app();
        select_entry(&mut app, "skill:list", "skill:list");

        let effects = reduce(&mut app, Action::SlashAutocompleteAccept);

        assert!(effects.is_empty());
        assert!(app.slash_autocomplete.is_none());
        assert!(app.sessions.current().input.is_empty());
        // The command ran in-process (pushed a system message) instead of being
        // sent to the LLM as the literal text "/skill list".
        assert!(
            app.sessions
                .current()
                .messages
                .iter()
                .all(|m| m.role != MessageRole::User)
        );
        assert!(
            app.sessions
                .current()
                .messages
                .iter()
                .any(|m| m.role == MessageRole::System)
        );
    }

    #[test]
    fn slash_autocomplete_accept_and_submit_dispatches_command_directly() {
        let (mut app, _rx) = make_app();
        select_entry(&mut app, "tasks", "tasks");
        assert!(!app.show_task_panel);

        let effects = reduce(&mut app, Action::SlashAutocompleteAcceptAndSubmit);

        assert!(effects.is_empty());
        assert!(app.show_task_panel);
        assert!(app.sessions.current().input.is_empty());
        assert!(
            app.sessions
                .current()
                .messages
                .iter()
                .all(|m| m.role != MessageRole::User)
        );
    }

    #[test]
    fn slash_autocomplete_accept_and_submit_on_arg_command_prefills_without_submitting() {
        // Commands that need an argument (e.g. "agent:cancel") prefill the input
        // for further typing instead of being submitted as an incomplete command.
        let (mut app, _rx) = make_app();
        select_entry(&mut app, "agent:cancel", "agent:cancel");

        let effects = reduce(&mut app, Action::SlashAutocompleteAcceptAndSubmit);

        assert!(effects.is_empty());
        assert_eq!(app.sessions.current().input, "/agent cancel ");
        assert!(
            app.sessions
                .current()
                .messages
                .iter()
                .all(|m| m.role != MessageRole::User),
            "incomplete prefilled command must not be sent as a chat message"
        );
    }

    // ── SubmitInput routes parsed slash commands in-process (#5782) ─────────

    #[test]
    fn submit_input_motion_command_applies_directly_without_bridge() {
        let (mut app, _rx) = make_app();
        app.sessions.current_mut().input = "/motion minimal".to_owned();

        let effects = reduce(&mut app, Action::SubmitInput);

        assert!(effects.is_empty());
        assert_eq!(app.motion, zeph_config::Motion::Minimal);
    }

    #[test]
    fn submit_input_session_close_executes_in_process() {
        let (mut app, _rx) = make_app();
        app.sessions.current_mut().input = "/session close".to_owned();

        let effects = reduce(&mut app, Action::SubmitInput);

        assert!(effects.is_empty());
        // Single-session default: close() refuses and reports in-process,
        // proving the command ran locally rather than being dropped by the bridge.
        assert!(app.sessions.current().messages.iter().any(|m| {
            m.content
                .contains("Cannot close the last remaining session")
        }));
    }

    #[test]
    fn submit_input_acp_dirs_forwards_via_agent_channel_in_process() {
        // `AcpDirsList` is handled by `execute_command`'s `handle_acp_command`, which
        // sends directly on `user_input_tx` rather than returning an `Effect`. This
        // exercises a third distinct in-process code path (alongside direct state
        // mutation and `Effect::SendUserInput`) that `forward_tui_commands`'s
        // `_ => continue` wildcard used to silently swallow (#5782).
        let (mut app, mut rx) = make_app();
        app.sessions.current_mut().input = "/acp dirs".to_owned();

        let effects = reduce(&mut app, Action::SubmitInput);

        assert!(effects.is_empty());
        let msg = rx.try_recv().expect("channel must have one message");
        assert_eq!(msg, "/acp dirs");
    }

    #[test]
    fn submit_input_subagent_spawn_forwards_command_via_agent_channel() {
        let (mut app, mut rx) = make_app();
        app.sessions.current_mut().input = "/subagent spawn review the diff".to_owned();

        let effects = reduce(&mut app, Action::SubmitInput);

        assert!(effects.is_empty());
        let msg = rx.try_recv().expect("channel must have one message");
        assert_eq!(msg, "/subagent spawn review the diff");
    }

    // ── Transcript search reducer wiring (issue #6023) ──────────────────────────
    //
    // These exercise `reduce()` end-to-end through the public `Action` surface — not
    // the underlying `TranscriptSearchState`/`line_offset_of_message` helpers directly
    // (those have their own unit tests in `widgets/transcript_search.rs` and
    // `widgets/chat.rs`) — so a wiring regression (e.g. a handler that stops calling
    // `begin_scroll`, or stops restoring `pre_search_scroll_offset`) would be caught
    // here even if the underlying helpers stay individually correct.

    /// Build an app with a populated transcript, splash disabled, and a real
    /// `last_layout` (via `AppLayout::compute`, no `Frame` needed) so
    /// `App::line_offset_of_message` — and therefore the reducer's `begin_scroll`
    /// calls — actually resolve to `Some` instead of short-circuiting on `None`.
    fn make_app_with_transcript() -> (App, mpsc::Receiver<String>) {
        let (mut app, rx) = make_app();
        app.sessions.current_mut().show_splash = false;
        // Disable smooth-scroll so begin_scroll writes scroll_offset synchronously
        // instead of an animated scroll_anim that only resolves over several ticks —
        // these tests assert on scroll_offset directly.
        app.delights.smooth_scroll = false;
        for i in 0..30 {
            app.sessions
                .current_mut()
                .messages
                .push(crate::ChatMessage::new(
                    crate::MessageRole::Assistant,
                    format!("filler message number {i}"),
                ));
        }
        app.sessions
            .current_mut()
            .messages
            .push(crate::ChatMessage::new(
                crate::MessageRole::Assistant,
                "the needle is here".to_owned(),
            ));
        for i in 0..30 {
            app.sessions
                .current_mut()
                .messages
                .push(crate::ChatMessage::new(
                    crate::MessageRole::Assistant,
                    format!("trailer message number {i}"),
                ));
        }
        let area = ratatui::layout::Rect::new(0, 0, 100, 20);
        app.last_layout = Some(crate::layout::AppLayout::compute(
            area,
            app.show_side_panels(),
            app.desired_input_height(),
            app.effective_collapsed(),
        ));
        (app, rx)
    }

    #[test]
    fn open_transcript_search_captures_pre_search_scroll_offset() {
        let (mut app, _rx) = make_app_with_transcript();
        app.sessions.current_mut().scroll_offset = 7;

        let effects = reduce(&mut app, Action::OpenTranscriptSearch);

        assert!(effects.is_empty());
        let state = app
            .transcript_search
            .as_ref()
            .expect("overlay must be open");
        assert_eq!(state.pre_search_scroll_offset, 7);
        assert!(state.matches.is_empty(), "no query typed yet");
    }

    #[test]
    fn transcript_search_input_scrolls_to_off_screen_match() {
        // SC-002: a query matching text in an off-screen earlier message must scroll
        // the transcript so that message becomes visible.
        let (mut app, _rx) = make_app_with_transcript();
        reduce(&mut app, Action::OpenTranscriptSearch);
        let before_scroll = app.sessions.current().scroll_offset;

        for c in "needle".chars() {
            reduce(
                &mut app,
                Action::TranscriptSearchInput(PaletteEdit::PushChar(c)),
            );
        }

        let state = app.transcript_search.as_ref().expect("overlay stays open");
        assert_eq!(
            state.matches.len(),
            1,
            "exactly one message contains 'needle'"
        );
        assert_ne!(
            app.sessions.current().scroll_offset,
            before_scroll,
            "matching a message must move the scroll position (begin_scroll was invoked)"
        );
    }

    #[test]
    fn transcript_search_next_and_prev_move_scroll_between_matches() {
        let (mut app, _rx) = make_app_with_transcript();
        app.sessions
            .current_mut()
            .messages
            .push(crate::ChatMessage::new(
                crate::MessageRole::Assistant,
                "needle again near the bottom".to_owned(),
            ));
        reduce(&mut app, Action::OpenTranscriptSearch);
        for c in "needle".chars() {
            reduce(
                &mut app,
                Action::TranscriptSearchInput(PaletteEdit::PushChar(c)),
            );
        }
        let state = app.transcript_search.as_ref().unwrap();
        assert_eq!(state.matches.len(), 2);
        let offset_after_input = app.sessions.current().scroll_offset;

        reduce(&mut app, Action::TranscriptSearchNext);
        let offset_after_next = app.sessions.current().scroll_offset;
        assert_ne!(
            offset_after_next, offset_after_input,
            "advancing to the next match must move the scroll target"
        );

        reduce(&mut app, Action::TranscriptSearchPrev);
        let offset_after_prev = app.sessions.current().scroll_offset;
        assert_eq!(
            offset_after_prev, offset_after_input,
            "stepping back must return to the first match's scroll target"
        );
    }

    #[test]
    fn close_transcript_search_restores_pre_search_scroll_offset() {
        // FR-006: Esc cancels search and restores the scroll position from before it
        // was opened, discarding any scroll movement search performed while active.
        let (mut app, _rx) = make_app_with_transcript();
        app.sessions.current_mut().scroll_offset = 3;
        reduce(&mut app, Action::OpenTranscriptSearch);
        for c in "needle".chars() {
            reduce(
                &mut app,
                Action::TranscriptSearchInput(PaletteEdit::PushChar(c)),
            );
        }
        assert_ne!(
            app.sessions.current().scroll_offset,
            3,
            "search must have moved the scroll position for this test to be meaningful"
        );

        let effects = reduce(&mut app, Action::CloseTranscriptSearch);

        assert!(effects.is_empty());
        assert!(app.transcript_search.is_none(), "overlay must close");
        assert_eq!(
            app.sessions.current().scroll_offset,
            3,
            "Esc must restore the pre-search scroll_offset"
        );
    }

    #[test]
    fn transcript_search_accept_closes_overlay_and_leaves_scroll_at_match() {
        // FR-007: Enter accepts the current match, closes the overlay, and leaves the
        // transcript scrolled at the match — it must NOT restore the pre-search offset.
        let (mut app, _rx) = make_app_with_transcript();
        app.sessions.current_mut().scroll_offset = 3;
        reduce(&mut app, Action::OpenTranscriptSearch);
        for c in "needle".chars() {
            reduce(
                &mut app,
                Action::TranscriptSearchInput(PaletteEdit::PushChar(c)),
            );
        }
        let scroll_at_match = app.sessions.current().scroll_offset;
        assert_ne!(scroll_at_match, 3);

        let effects = reduce(&mut app, Action::TranscriptSearchAccept);

        assert!(effects.is_empty());
        assert!(app.transcript_search.is_none(), "overlay must close");
        assert_eq!(
            app.sessions.current().scroll_offset,
            scroll_at_match,
            "Enter must leave the transcript scrolled at the accepted match, not restore pre-search state"
        );
    }

    #[test]
    fn transcript_search_dispatch_command_opens_overlay() {
        let (mut app, _rx) = make_app_with_transcript();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::TranscriptSearch));
        assert!(effects.is_empty());
        assert!(app.transcript_search.is_some());
    }

    // ── Settings view reducer wiring (issue #6024) ──────────────────────────────

    #[test]
    fn settings_tab_next_and_prev_cycle_through_reducer() {
        let (mut app, _rx) = make_app();
        assert_eq!(
            app.settings.tab,
            crate::widgets::settings::SettingsTab::Providers
        );

        reduce(&mut app, Action::SettingsTabNext);
        assert_eq!(app.settings.tab, crate::widgets::settings::SettingsTab::Mcp);

        reduce(&mut app, Action::SettingsTabNext);
        assert_eq!(
            app.settings.tab,
            crate::widgets::settings::SettingsTab::Agents
        );

        reduce(&mut app, Action::SettingsTabPrev);
        assert_eq!(app.settings.tab, crate::widgets::settings::SettingsTab::Mcp);
    }

    #[test]
    fn settings_select_move_advances_and_clamps_via_settings_active_tab_len() {
        let (mut app, _rx) = make_app();
        app.metrics.providers = vec![
            zeph_core::metrics::ProviderSummary::default(),
            zeph_core::metrics::ProviderSummary::default(),
        ]
        .into();

        reduce(&mut app, Action::SettingsSelectMove(VertDir::Down));
        assert_eq!(app.settings.selected_index(), 1);

        // Clamped at count - 1 (2 providers => max index 1), proving the reducer wires
        // the live provider count through settings_active_tab_len rather than an
        // unbounded increment.
        reduce(&mut app, Action::SettingsSelectMove(VertDir::Down));
        assert_eq!(app.settings.selected_index(), 1);

        reduce(&mut app, Action::SettingsSelectMove(VertDir::Up));
        assert_eq!(app.settings.selected_index(), 0);
    }

    #[test]
    fn settings_dispatch_command_opens_settings_panel() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::Settings));
        assert!(effects.is_empty());
        assert_eq!(app.active_panel, Panel::Settings);
    }
}
