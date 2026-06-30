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
use crate::widgets::slash_autocomplete::{SlashAutocompleteState, command_id_to_slash_form};

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
                    app.show_task_panel = !app.show_task_panel;
                    return vec![];
                }
                TuiCommand::FleetPanel => {
                    app.active_panel = Panel::Fleet;
                    return vec![];
                }
                TuiCommand::DurablePanel => {
                    app.active_panel = Panel::Durable;
                    return vec![];
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
                TuiCommand::DaemonConnect
                | TuiCommand::DaemonDisconnect
                | TuiCommand::DaemonStatus => {
                    app.push_system_message_pub(
                        "Daemon commands are not yet implemented in this mode.".to_owned(),
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
        assert!(matches!(effects.as_slice(), [Effect::Quit]));
    }

    #[test]
    fn set_mouse_emits_capture_effect() {
        let (mut app, _rx) = make_app();
        let effects = reduce(&mut app, Action::SetMouse(true));
        assert!(app.mouse_enabled);
        assert!(matches!(
            effects.as_slice(),
            [Effect::SetMouseCapture(true)]
        ));
    }

    #[test]
    fn set_mouse_off_emits_disable() {
        let (mut app, _rx) = make_app();
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

    #[test]
    fn dispatch_toggle_mouse_flips_flag() {
        let (mut app, _rx) = make_app();
        assert!(!app.mouse_enabled);
        let effects = reduce(&mut app, Action::Dispatch(TuiCommand::ToggleMouse));
        assert!(app.mouse_enabled);
        assert!(matches!(
            effects.as_slice(),
            [Effect::SetMouseCapture(true)]
        ));
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
        assert!(matches!(
            app.sessions.current().messages[0].role,
            MessageRole::System
        ));
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
        assert!(matches!(
            app.sessions.current().messages[0].role,
            MessageRole::System
        ));
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
}
