// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Semantic actions emitted by input decoders and consumed by [`super::reducer::reduce`].

use crate::app::{AgentViewTarget, Panel};
use crate::command::TuiCommand;

/// Scroll direction for page-level scroll actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ScrollDir {
    Up,
    Down,
}

/// Vertical cursor or field navigation direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum VertDir {
    Up,
    Down,
}

/// Sub-edits for the command-palette text field.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum PaletteEdit {
    /// Push one character into the filter field.
    PushChar(char),
    /// Delete the last character from the filter field.
    PopChar,
}

/// Sub-edits for elicitation dialog fields (C4 — all 7 variants required).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ElicitationEdit {
    /// Push one character into the current text field.
    PushChar(char),
    /// Delete the last character from the current text field.
    PopChar,
    /// Move focus to the next field.
    NextField,
    /// Move focus to the previous field.
    PrevField,
    /// Toggle a boolean field.
    ToggleBool,
    /// Cycle an enum field to the next option.
    EnumNext,
    /// Cycle an enum field to the previous option.
    EnumPrev,
}

/// A semantic intent produced by a keyboard or mouse decoder and consumed by `reduce`.
///
/// Variants cover the full key/mouse surface; agent-event handlers remain outside this
/// enum (they are already a self-contained reducer over disjoint state).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Action {
    // ── Scroll ─────────────────────────────────────────────────────────────────
    /// Scroll the focused panel by `lines` lines (negative = up, positive = down).
    ScrollLines(i32),
    /// Scroll by one page in the given direction.
    ScrollPage(ScrollDir),
    /// Jump to the very top of the transcript.
    ScrollToTop,
    /// Jump to the very bottom of the transcript (offset = 0).
    ScrollToBottom,

    // ── Panel toggles ──────────────────────────────────────────────────────────
    /// Toggle tool-output expansion.
    ToggleToolExpanded,
    /// Cycle the tool density preset.
    CycleToolDensity,
    /// Toggle the side-panel column.
    ToggleSidePanels,
    /// Show help overlay.
    ToggleHelp,
    /// Explicitly set help visibility.
    SetHelp(bool),
    /// Tab-cycle the focused side panel.
    CyclePanelFocus,
    /// Set the focused panel directly (e.g. from a mouse click or key shortcut).
    SetActivePanel(Panel),
    /// Toggle per-section collapse for the given panel index (0=skills, 1=memory, 2=resources, 3=subagents).
    TogglePanelCollapse(usize),
    /// Toggle the task-registry overlay panel.
    ToggleTaskPanel,
    /// Toggle the plan/transcript view for the current session.
    TogglePlanView,

    // ── Session / view ─────────────────────────────────────────────────────────
    /// Switch the chat view to the given target (`Main` or `SubAgent`).
    SetViewTarget(AgentViewTarget),
    /// Clear the current session transcript.
    ClearTranscript,
    /// Quit the TUI.
    Quit,
    /// Cancel the current agent turn (Ctrl-C equivalent).
    CancelAgent,

    // ── Input mode ─────────────────────────────────────────────────────────────
    /// Transition the input box to Insert mode.
    EnterInsert,
    /// Transition to Normal mode.
    EnterNormal,
    /// Insert a character at the cursor.
    InsertChar(char),
    /// Insert a newline at the cursor.
    InsertNewline,
    /// Insert a string (paste).
    InsertText(String),
    /// Delete the character before the cursor.
    DeleteCharBackward,
    /// Delete the character after the cursor.
    DeleteCharForward,
    /// Delete the word before the cursor.
    DeleteWordBackward,
    /// Move the cursor.
    MoveCursor(CursorMove),
    /// Clear the input buffer.
    ClearInput,
    /// Submit the current input to the agent.
    SubmitInput,
    /// Navigate backward in input history.
    HistoryPrev,
    /// Navigate forward in input history.
    HistoryNext,
    /// Pre-fill the input buffer with a prefix string (for /commands that prompt).
    PrefillInput(String),

    // ── Command palette ────────────────────────────────────────────────────────
    /// Open the command palette.
    OpenCommandPalette,
    /// Close the command palette without accepting.
    CloseCommandPalette,
    /// Move the palette selection.
    PaletteMove(VertDir),
    /// Type in the palette filter field.
    PaletteInput(PaletteEdit),
    /// Accept the currently selected palette entry.
    PaletteAccept,

    // ── File picker ────────────────────────────────────────────────────────────
    /// Open the file picker.
    OpenFilePicker,
    /// Close the file picker without selecting.
    CloseFilePicker,
    /// Move the file picker selection.
    FilePickerMove(VertDir),
    /// Type in the file picker filter field.
    FilePickerInput(PaletteEdit),
    /// Accept the currently selected file.
    FilePickerAccept,

    // ── Slash autocomplete ─────────────────────────────────────────────────────
    /// Move the autocomplete selection.
    SlashAutocompleteMove(VertDir),
    /// Accept the selected autocomplete entry (Tab — insert without submit).
    SlashAutocompleteAccept,
    /// Accept the selected entry and immediately submit the input (Enter).
    SlashAutocompleteAcceptAndSubmit,
    /// Pop one character from the autocomplete filter.
    SlashAutocompletePopChar,
    /// Push one character into the autocomplete filter.
    SlashAutocompletePushChar(char),
    /// Close the autocomplete overlay.
    CloseSlashAutocomplete,

    // ── Reverse search ─────────────────────────────────────────────────────────
    /// Open the reverse history search overlay.
    OpenReverseSearch,
    /// Type in the reverse-search field.
    ReverseSearchInput(PaletteEdit),
    /// Cycle to the next match.
    ReverseSearchNext,
    /// Cycle to the previous match.
    ReverseSearchPrev,
    /// Accept the current reverse-search result.
    ReverseSearchAccept,
    /// Close the reverse-search overlay.
    CloseReverseSearch,

    // ── Transcript search (issue #6023) ────────────────────────────────────────
    /// Open the `Ctrl+F` transcript-search overlay.
    OpenTranscriptSearch,
    /// Type in the transcript-search query field.
    TranscriptSearchInput(PaletteEdit),
    /// Advance to the next match, wrapping.
    TranscriptSearchNext,
    /// Move to the previous match, wrapping.
    TranscriptSearchPrev,
    /// Accept the current match: close the overlay, leaving the scroll position.
    TranscriptSearchAccept,
    /// Close the overlay without accepting, restoring the pre-search scroll position.
    CloseTranscriptSearch,

    // ── Settings view (issue #6024) ─────────────────────────────────────────────
    /// Switch the settings view to the next tab (Providers → MCP → Agents), wrapping.
    SettingsTabNext,
    /// Switch the settings view to the previous tab, wrapping.
    SettingsTabPrev,
    /// Move the active tab's row selection up or down.
    SettingsSelectMove(VertDir),

    // ── Confirm dialog ─────────────────────────────────────────────────────────
    /// Respond to the current confirm dialog (true = yes, false = no).
    ConfirmRespond(bool),

    // ── Elicitation dialog ─────────────────────────────────────────────────────
    /// Edit a field in the elicitation dialog.
    ElicitationField(ElicitationEdit),
    /// Submit the elicitation dialog.
    ElicitationSubmit,
    /// Cancel the elicitation dialog.
    ElicitationCancel,

    // ── Mouse mode ─────────────────────────────────────────────────────────────
    /// Enable or disable opt-in mouse capture.
    SetMouse(bool),

    // ── Clipboard ──────────────────────────────────────────────────────────────
    /// Copy the last assistant message to the clipboard.
    CopyLastAssistant,
    /// Copy the Nth code block from the last assistant message (0 = last).
    CopyLastCodeBlock(usize),

    // ── Command dispatch ────────────────────────────────────────────────────────
    /// Pass-through for commands that live outside the reducer
    /// (agent commands, slash commands forwarded to the channel).
    Dispatch(TuiCommand),
}

/// Cursor movement kinds for [`Action::MoveCursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum CursorMove {
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
}
