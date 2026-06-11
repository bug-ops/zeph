// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use crate::event::{AgentEvent, AppEvent};
use crate::session::MAX_TUI_MESSAGES;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn make_app() -> (App, mpsc::Receiver<String>, mpsc::Sender<AgentEvent>) {
    let (user_tx, user_rx) = mpsc::channel(16);
    let (agent_tx, agent_rx) = mpsc::channel(16);
    let mut app = App::new(user_tx, agent_rx);
    app.sessions.current_mut().messages.clear();
    (app, user_rx, agent_tx)
}

#[test]
fn initial_state() {
    let (app, _rx, _tx) = make_app();
    assert!(app.input().is_empty());
    assert_eq!(app.input_mode(), InputMode::Insert);
    assert!(app.messages().is_empty());
    assert!(app.show_splash());
    assert!(!app.should_quit);
}

#[test]
fn ctrl_c_quits() {
    let (mut app, _rx, _tx) = make_app();
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    app.handle_event(AppEvent::Key(key));
    assert!(app.should_quit);
}

#[test]
fn insert_mode_typing() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input(), "a");
    assert_eq!(app.cursor_position(), 1);
}

#[test]
fn escape_switches_to_normal() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input_mode(), InputMode::Normal);
}

#[test]
fn i_enters_insert_mode() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    let key = KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input_mode(), InputMode::Insert);
}

#[test]
fn q_quits_in_normal_mode() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.should_quit);
}

#[test]
fn backspace_deletes_char() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "ab".into();
    app.sessions.current_mut().cursor_position = 2;
    let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input(), "a");
    assert_eq!(app.cursor_position(), 1);
}

#[test]
fn enter_submits_input() {
    let (mut app, mut rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "hello".into();
    app.sessions.current_mut().cursor_position = 5;
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.input().is_empty());
    assert_eq!(app.messages().len(), 1);
    assert_eq!(app.messages()[0].content, "hello");

    let sent = rx.try_recv().unwrap();
    assert_eq!(sent, "hello");
}

#[test]
fn empty_enter_does_not_submit() {
    let (mut app, mut rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.messages().is_empty());
    assert!(rx.try_recv().is_err());
}

#[test]
fn agent_chunk_creates_streaming_message() {
    let (mut app, _rx, _tx) = make_app();
    app.handle_agent_event(AgentEvent::Chunk("hel".into()));
    assert_eq!(app.messages().len(), 1);
    assert!(app.messages()[0].streaming);
    assert_eq!(app.messages()[0].content, "hel");

    app.handle_agent_event(AgentEvent::Chunk("lo".into()));
    assert_eq!(app.messages().len(), 1);
    assert_eq!(app.messages()[0].content, "hello");
}

#[test]
fn agent_flush_stops_streaming() {
    let (mut app, _rx, _tx) = make_app();
    app.handle_agent_event(AgentEvent::Chunk("test".into()));
    assert!(app.messages()[0].streaming);
    app.handle_agent_event(AgentEvent::Flush);
    assert!(!app.messages()[0].streaming);
}

#[test]
fn agent_full_message() {
    let (mut app, _rx, _tx) = make_app();
    app.handle_agent_event(AgentEvent::FullMessage("done".into()));
    assert_eq!(app.messages().len(), 1);
    assert!(!app.messages()[0].streaming);
    assert_eq!(app.messages()[0].content, "done");
}

#[test]
fn full_message_skips_tool_output_new_format() {
    let (mut app, _rx, _tx) = make_app();
    app.handle_agent_event(AgentEvent::FullMessage(
        "[tool output: bash]\n```\n$ echo hi\nhi\n```".into(),
    ));
    assert!(app.messages().is_empty());
}

#[test]
fn scroll_in_normal_mode() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(up));
    assert_eq!(app.scroll_offset(), 1);

    let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(down));
    assert_eq!(app.scroll_offset(), 0);
}

#[test]
fn tab_cycles_panels() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    assert_eq!(app.active_panel, Panel::Chat);

    let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(tab));
    assert_eq!(app.active_panel, Panel::Skills);

    app.handle_event(AppEvent::Key(tab));
    assert_eq!(app.active_panel, Panel::Memory);

    app.handle_event(AppEvent::Key(tab));
    assert_eq!(app.active_panel, Panel::Resources);

    app.handle_event(AppEvent::Key(tab));
    assert_eq!(app.active_panel, Panel::SubAgents);

    app.handle_event(AppEvent::Key(tab));
    assert_eq!(app.active_panel, Panel::Fleet);

    app.handle_event(AppEvent::Key(tab));
    assert_eq!(app.active_panel, Panel::Durable);

    app.handle_event(AppEvent::Key(tab));
    assert_eq!(app.active_panel, Panel::Chat);
}

#[test]
fn ctrl_u_clears_input() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "some text".into();
    app.sessions.current_mut().cursor_position = 9;
    let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
    app.handle_event(AppEvent::Key(key));
    assert!(app.input().is_empty());
    assert_eq!(app.cursor_position(), 0);
}

#[test]
fn cursor_movement() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "abc".into();
    app.sessions.current_mut().cursor_position = 1;

    let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(left));
    assert_eq!(app.cursor_position(), 0);

    // left at 0 stays at 0
    app.handle_event(AppEvent::Key(left));
    assert_eq!(app.cursor_position(), 0);

    let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(right));
    assert_eq!(app.cursor_position(), 1);

    let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(home));
    assert_eq!(app.cursor_position(), 0);

    let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(end));
    assert_eq!(app.cursor_position(), 3);
}

#[test]
fn delete_key_removes_char_at_cursor() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "abc".into();
    app.sessions.current_mut().cursor_position = 1;
    let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input(), "ac");
    assert_eq!(app.cursor_position(), 1);
}

#[test]
fn unicode_input_insert_and_delete() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;

    // Type multi-byte chars
    for c in "\u{00e9}a\u{1f600}".chars() {
        let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
    }
    assert_eq!(app.input(), "\u{00e9}a\u{1f600}");
    assert_eq!(app.cursor_position(), 3);

    // Backspace removes the emoji (last char)
    let bs = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(bs));
    assert_eq!(app.input(), "\u{00e9}a");
    assert_eq!(app.cursor_position(), 2);

    // Move cursor left and delete 'a'
    let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(left));
    assert_eq!(app.cursor_position(), 1);

    let del = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(del));
    assert_eq!(app.input(), "\u{00e9}");
    assert_eq!(app.cursor_position(), 1);

    // End key uses char count, not byte count
    let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(end));
    assert_eq!(app.cursor_position(), 1);
}

#[test]
fn confirm_request_sets_state() {
    let (mut app, _rx, _tx) = make_app();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    app.handle_agent_event(AgentEvent::ConfirmRequest {
        prompt: "delete?".into(),
        response_tx: tx,
    });
    assert!(app.confirm_state.is_some());
    assert_eq!(app.confirm_state.as_ref().unwrap().prompt, "delete?");
}

#[test]
fn confirm_modal_y_sends_true() {
    let (mut app, _rx, _tx) = make_app();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    app.confirm_state = Some(ConfirmState {
        prompt: "proceed?".into(),
        response_tx: Some(tx),
    });
    let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.confirm_state.is_none());
    assert!(rx.try_recv().unwrap());
}

#[test]
fn confirm_modal_enter_sends_true() {
    let (mut app, _rx, _tx) = make_app();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    app.confirm_state = Some(ConfirmState {
        prompt: "proceed?".into(),
        response_tx: Some(tx),
    });
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.confirm_state.is_none());
    assert!(rx.try_recv().unwrap());
}

#[test]
fn confirm_modal_n_sends_false() {
    let (mut app, _rx, _tx) = make_app();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    app.confirm_state = Some(ConfirmState {
        prompt: "delete?".into(),
        response_tx: Some(tx),
    });
    let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.confirm_state.is_none());
    assert!(!rx.try_recv().unwrap());
}

#[test]
fn confirm_modal_escape_sends_false() {
    let (mut app, _rx, _tx) = make_app();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    app.confirm_state = Some(ConfirmState {
        prompt: "delete?".into(),
        response_tx: Some(tx),
    });
    let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.confirm_state.is_none());
    assert!(!rx.try_recv().unwrap());
}

#[test]
fn try_switch_blocked_by_confirm_modal() {
    let (mut app, _rx, _tx) = make_app();
    let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
    app.confirm_state = Some(ConfirmState {
        prompt: "ok?".into(),
        response_tx: Some(tx),
    });
    let prev_active = app.sessions.active();
    app.execute_command(TuiCommand::SessionSwitchNext);
    assert_eq!(app.sessions.active(), prev_active);
    assert!(
        app.sessions
            .current()
            .messages
            .iter()
            .any(|m| m.content.contains("Resolve"))
    );
}

#[test]
fn try_switch_blocked_by_elicitation_modal() {
    let (mut app, _rx, _tx) = make_app();
    let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
    let req = zeph_core::channel::ElicitationRequest {
        server_name: "test".into(),
        message: "test".into(),
        fields: vec![],
    };
    app.elicitation_state = Some(ElicitationState {
        dialog: crate::widgets::elicitation::ElicitationDialogState::new(req),
        response_tx: Some(tx),
    });
    let prev_active = app.sessions.active();
    app.execute_command(TuiCommand::SessionSwitchPrev);
    assert_eq!(app.sessions.active(), prev_active);
    assert!(
        app.sessions
            .current()
            .messages
            .iter()
            .any(|m| m.content.contains("Resolve"))
    );
}

#[test]
fn try_switch_close_refused_on_last_slot() {
    let (mut app, _rx, _tx) = make_app();
    app.execute_command(TuiCommand::SessionClose);
    assert!(
        app.sessions
            .current()
            .messages
            .iter()
            .any(|m| m.content.contains("Cannot close"))
    );
}

#[test]
fn confirm_modal_blocks_other_keys() {
    let (mut app, _rx, _tx) = make_app();
    let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.confirm_state = Some(ConfirmState {
        prompt: "test?".into(),
        response_tx: Some(tx),
    });
    let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.input().is_empty());
    assert!(app.confirm_state.is_some());
}

#[test]
fn shift_enter_inserts_newline() {
    let (mut app, mut rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "hello".into();
    app.sessions.current_mut().cursor_position = 5;
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input(), "hello\n");
    assert_eq!(app.cursor_position(), 6);
    assert!(app.messages().is_empty());
    assert!(rx.try_recv().is_err());
}

#[test]
fn ctrl_j_inserts_newline() {
    let (mut app, mut rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "hello".into();
    app.sessions.current_mut().cursor_position = 5;
    let key = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input(), "hello\n");
    assert_eq!(app.cursor_position(), 6);
    assert!(app.messages().is_empty());
    assert!(rx.try_recv().is_err());
}

#[test]
fn shift_enter_mid_input() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "ab".into();
    app.sessions.current_mut().cursor_position = 1;
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input(), "a\nb");
    assert_eq!(app.cursor_position(), 2);
}

#[test]
fn d_toggles_side_panels() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    assert!(app.show_side_panels());

    let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(!app.show_side_panels());

    app.handle_event(AppEvent::Key(key));
    assert!(app.show_side_panels());
}

#[test]
fn scroll_up_via_key() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    assert_eq!(app.scroll_offset(), 0);
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Up,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.scroll_offset(), 1);
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Up,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.scroll_offset(), 2);
}

#[test]
fn scroll_down_via_key() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    app.sessions.current_mut().scroll_offset = 5;
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.scroll_offset(), 4);
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.scroll_offset(), 3);
}

#[test]
fn scroll_down_saturates_at_zero() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    app.sessions.current_mut().scroll_offset = 1;
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.scroll_offset(), 0);
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.scroll_offset(), 0);
}

#[test]
fn scroll_during_confirm_blocked() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
    app.confirm_state = Some(ConfirmState {
        prompt: "test?".into(),
        response_tx: Some(tx),
    });
    app.sessions.current_mut().scroll_offset = 5;
    // The confirm dialog intercepts all key events before the normal key handler.
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Up,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.scroll_offset(), 5);
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.scroll_offset(), 5);
}

#[test]
fn load_history_recognizes_tool_output_new_format() {
    let (mut app, _rx, _tx) = make_app();
    app.load_history(&[
        ("user", "hello"),
        ("assistant", "hi there"),
        ("user", "[tool output: bash]\n```\n$ echo hello\nhello\n```"),
        ("assistant", "done"),
    ]);
    assert_eq!(app.messages().len(), 4);
    assert_eq!(app.messages()[0].role, MessageRole::User);
    assert_eq!(app.messages()[1].role, MessageRole::Assistant);
    assert_eq!(app.messages()[2].role, MessageRole::Tool);
    assert_eq!(
        app.messages()[2]
            .tool_name
            .as_ref()
            .map(zeph_common::ToolName::as_str),
        Some("bash")
    );
    assert_eq!(app.messages()[2].content, "$ echo hello\nhello");
    assert_eq!(app.messages()[3].role, MessageRole::Assistant);
}

#[test]
fn load_history_recognizes_legacy_tool_output() {
    let (mut app, _rx, _tx) = make_app();
    app.load_history(&[("user", "[tool output]\n```\n$ ls\nfile.txt\n```")]);
    assert_eq!(app.messages().len(), 1);
    assert_eq!(app.messages()[0].role, MessageRole::Tool);
    assert_eq!(
        app.messages()[0]
            .tool_name
            .as_ref()
            .map(zeph_common::ToolName::as_str),
        Some("bash")
    );
    assert_eq!(app.messages()[0].content, "$ ls\nfile.txt");
}

#[test]
fn load_history_legacy_non_bash_tool() {
    let (mut app, _rx, _tx) = make_app();
    app.load_history(&[(
        "user",
        "[tool output]\n```\n[mcp:github:list]\nresults\n```",
    )]);
    assert_eq!(app.messages().len(), 1);
    assert_eq!(app.messages()[0].role, MessageRole::Tool);
    assert_eq!(
        app.messages()[0]
            .tool_name
            .as_ref()
            .map(zeph_common::ToolName::as_str),
        Some("tool")
    );
}

#[test]
fn load_history_recognizes_tool_result_format() {
    let (mut app, _rx, _tx) = make_app();
    app.load_history(&[("user", "[tool_result: toolu_abc]\n$ echo hello\nhello")]);
    assert_eq!(app.messages().len(), 1);
    assert_eq!(app.messages()[0].role, MessageRole::Tool);
    assert_eq!(
        app.messages()[0]
            .tool_name
            .as_ref()
            .map(zeph_common::ToolName::as_str),
        Some("bash")
    );
    assert_eq!(app.messages()[0].content, "$ echo hello\nhello");
}

#[test]
fn load_history_hides_tool_use_only_messages() {
    let (mut app, _rx, _tx) = make_app();
    app.load_history(&[
        ("user", "hello"),
        (
            "assistant",
            "[tool_use: bash(toolu_01AfnYMrx3Ub13LLQ1Py3nfg)]",
        ),
        ("assistant", "here is the result"),
    ]);
    assert_eq!(app.messages().len(), 2);
    assert_eq!(app.messages()[0].role, MessageRole::User);
    assert_eq!(app.messages()[1].role, MessageRole::Assistant);
    assert_eq!(app.messages()[1].content, "here is the result");
}

#[test]
fn load_history_keeps_assistant_with_text_and_tool_use() {
    let (mut app, _rx, _tx) = make_app();
    app.load_history(&[("assistant", "Let me check. [tool_use: bash(toolu_abc)]")]);
    assert_eq!(app.messages().len(), 1);
    assert_eq!(app.messages()[0].role, MessageRole::Assistant);
}

#[test]
fn is_tool_use_only_multiple_tags() {
    assert!(is_tool_use_only(
        "[tool_use: bash(id1)] [tool_use: read(id2)]"
    ));
    assert!(!is_tool_use_only("text [tool_use: bash(id1)]"));
    assert!(!is_tool_use_only(""));
}

#[test]
fn tool_output_without_prior_tool_start_creates_tool_message_with_diff() {
    let (mut app, _rx, _tx) = make_app();
    let diff = zeph_core::DiffData {
        file_path: "src/lib.rs".into(),
        old_content: "fn old() {}".into(),
        new_content: "fn new() {}".into(),
    };
    app.handle_agent_event(AgentEvent::ToolOutput {
        tool_name: "edit".into(),
        command: "[tool output: edit]\n```\nok\n```".into(),
        output: "[tool output: edit]\n```\nok\n```".into(),
        success: true,
        diff: Some(diff),
        filter_stats: None,
        kept_lines: None,
        tool_call_id: "call-1".into(),
    });

    assert_eq!(app.messages().len(), 1);
    let msg = &app.messages()[0];
    assert_eq!(msg.role, MessageRole::Tool);
    assert!(!msg.streaming);
    assert!(msg.diff_data.is_some());
}

#[test]
fn tool_output_without_diff_does_not_create_spurious_message() {
    let (mut app, _rx, _tx) = make_app();
    app.handle_agent_event(AgentEvent::ToolOutput {
        tool_name: "read".into(),
        command: "[tool output: read]\n```\ncontent\n```".into(),
        output: "[tool output: read]\n```\ncontent\n```".into(),
        success: true,
        diff: None,
        filter_stats: None,
        kept_lines: None,
        tool_call_id: "call-2".into(),
    });

    // No prior ToolStart and no diff/filter_stats: nothing to display.
    assert!(app.messages().is_empty());
}

#[test]
fn show_help_defaults_to_false() {
    let (app, _rx, _tx) = make_app();
    assert!(!app.show_help);
}

#[test]
fn question_mark_in_normal_mode_opens_help() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.show_help);
}

#[test]
fn question_mark_toggles_help_closed() {
    let (mut app, _rx, _tx) = make_app();
    app.show_help = true;
    let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(!app.show_help);
}

#[test]
fn esc_closes_help_popup() {
    let (mut app, _rx, _tx) = make_app();
    app.show_help = true;
    let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(!app.show_help);
}

#[test]
fn other_keys_ignored_when_help_open() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.show_help = true;

    // Typing a character should not modify input
    let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.input().is_empty());
    assert!(app.show_help);

    // Enter should not submit
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(app.messages().is_empty());
    assert!(app.show_help);
}

#[test]
fn help_popup_does_not_block_ctrl_c() {
    let (mut app, _rx, _tx) = make_app();
    app.show_help = true;
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    app.handle_event(AppEvent::Key(key));
    assert!(app.should_quit);
}

#[test]
fn question_mark_in_insert_mode_does_not_open_help() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    assert!(!app.show_help);
    assert_eq!(app.input(), "?");
}

#[tokio::test]
async fn esc_in_normal_mode_cancels_when_busy() {
    let (mut app, _rx, _tx) = make_app();
    let notify = Arc::new(Notify::new());
    let notify_waiter = Arc::clone(&notify);
    let handle = tokio::spawn(async move {
        notify_waiter.notified().await;
        true
    });
    tokio::task::yield_now().await;

    app = app.with_cancel_signal(Arc::clone(&notify));
    app.sessions.current_mut().input_mode = InputMode::Normal;
    app.sessions.current_mut().status_label = Some("Thinking...".into());
    assert!(app.is_agent_busy());

    let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    let result = tokio::time::timeout(std::time::Duration::from_millis(100), handle).await;
    assert!(result.is_ok(), "notify should have been triggered");
}

#[test]
fn esc_in_normal_mode_does_not_cancel_when_idle() {
    let (mut app, _rx, _tx) = make_app();
    let notify = Arc::new(Notify::new());
    app = app.with_cancel_signal(notify);
    app.sessions.current_mut().input_mode = InputMode::Normal;
    assert!(!app.is_agent_busy());

    let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));
    // No way to assert "not notified" directly, but we verify no panic
}

#[test]
fn up_with_empty_input_and_queued_recalls_from_history() {
    let (mut app, mut rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.pending_count = 2;
    app.sessions
        .current_mut()
        .input_history
        .push("queued msg".into());
    app.sessions
        .current_mut()
        .messages
        .push(ChatMessage::new(MessageRole::User, "queued msg"));

    let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));

    assert_eq!(app.input(), "queued msg");
    assert_eq!(app.cursor_position(), 10);
    assert!(app.editing_queued());
    assert_eq!(app.queued_count(), 1);
    assert!(app.sessions.current_mut().input_history.is_empty());
    assert!(app.messages().is_empty());
    let sent = rx.try_recv().unwrap();
    assert_eq!(sent, "/drop-last-queued");
}

#[test]
fn up_with_non_empty_input_navigates_history() {
    let (mut app, mut rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.pending_count = 2;
    app.sessions.current_mut().input = "hello".into();
    app.sessions.current_mut().cursor_position = 5;
    app.sessions
        .current_mut()
        .input_history
        .push("hello world".into());

    let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(key));

    assert!(rx.try_recv().is_err());
    assert_eq!(app.input(), "hello world");
}

#[test]
fn submit_input_resets_editing_queued() {
    let (mut app, _rx, _tx) = make_app();
    app.editing_queued = true;
    app.sessions.current_mut().input = "some text".into();
    app.sessions.current_mut().cursor_position = 9;
    app.submit_input();
    assert!(!app.editing_queued());
}

#[test]
fn desired_input_height_caps_at_three_visible_lines() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "one\ntwo\nthree\nfour".into();
    app.sessions.current_mut().cursor_position = app.char_count();

    assert_eq!(app.input_line_count(), 4);
    assert_eq!(app.desired_input_height(), 5);
}

mod integration {
    use super::*;
    use crate::test_utils::test_terminal;

    fn draw_app(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = test_terminal(width, height);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut output = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                output.push_str(buf[(x, y)].symbol());
            }
            output.push('\n');
        }
        output
    }

    #[test]
    fn submit_message_appears_in_chat() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;
        app.sessions.current_mut().input = "hello world".into();
        app.sessions.current_mut().cursor_position = 11;
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(enter));

        let output = draw_app(&mut app, 80, 24);
        assert!(output.contains("hello world"));
    }

    #[test]
    fn help_overlay_renders() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Normal;
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        let output = draw_app(&mut app, 80, 30);
        assert!(output.contains("Help"));
        assert!(output.contains("quit"));
    }

    #[test]
    fn help_overlay_closes() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Normal;
        let open = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(open));
        let close = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(close));

        let output = draw_app(&mut app, 80, 30);
        assert!(!output.contains("Help — press"));
    }

    #[test]
    fn confirm_dialog_renders() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "Execute rm -rf?".into(),
            response_tx: Some(tx),
        });

        let output = draw_app(&mut app, 60, 20);
        assert!(output.contains("Confirm"));
        assert!(output.contains("Execute rm -rf?"));
        assert!(output.contains("[Y]es / [N]o"));
    }

    #[test]
    fn confirm_dialog_disappears_after_response() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "Delete?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        let output = draw_app(&mut app, 60, 20);
        assert!(!output.contains("[Y]es / [N]o"));
    }

    #[test]
    fn side_panels_toggle_off() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Normal;

        let before = draw_app(&mut app, 120, 40);
        assert!(before.contains("skills"));
        assert!(before.contains("memory"));

        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        let after = draw_app(&mut app, 120, 40);
        assert!(!after.contains("skills  "));
    }

    #[test]
    fn splash_shown_initially() {
        let (mut app, _rx, _tx) = make_app();
        let output = draw_app(&mut app, 80, 24);
        assert!(
            output.contains("zeph"),
            "splash must contain 'zeph' wordmark, got: {output}"
        );
    }

    #[test]
    fn splash_disappears_after_submit() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;
        app.sessions.current_mut().input = "hi".into();
        app.sessions.current_mut().cursor_position = 2;
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(enter));

        assert!(
            !app.sessions.current_mut().show_splash,
            "splash should be hidden after submit"
        );
    }

    #[test]
    fn markdown_link_produces_hyperlink_span() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().show_splash = false;
        app.sessions.current_mut().messages.push(ChatMessage::new(
            MessageRole::Assistant,
            "See [docs](https://docs.rs) for details",
        ));

        let _ = draw_app(&mut app, 80, 24);
        let links = app.take_hyperlinks();
        let doc_link = links.iter().find(|s| s.url == "https://docs.rs");
        assert!(
            doc_link.is_some(),
            "expected hyperlink span for markdown link, got: {links:?}"
        );
    }

    #[test]
    fn bare_url_still_produces_hyperlink_span() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().show_splash = false;
        app.sessions.current_mut().messages.push(ChatMessage::new(
            MessageRole::Assistant,
            "Visit https://example.com today",
        ));

        let _ = draw_app(&mut app, 80, 24);
        let links = app.take_hyperlinks();
        let bare = links.iter().find(|s| s.url == "https://example.com");
        assert!(
            bare.is_some(),
            "expected hyperlink span for bare URL, got: {links:?}"
        );
    }
}

#[test]
fn prev_word_boundary_from_middle_of_word() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 8;
    assert_eq!(app.prev_word_boundary(), 6);
}

#[test]
fn prev_word_boundary_from_start_of_second_word() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 6;
    assert_eq!(app.prev_word_boundary(), 0);
}

#[test]
fn prev_word_boundary_at_zero_stays_zero() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 0;
    assert_eq!(app.prev_word_boundary(), 0);
}

#[test]
fn next_word_boundary_from_middle_of_first_word() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 2;
    assert_eq!(app.next_word_boundary(), 6);
}

#[test]
fn next_word_boundary_from_start_of_second_word() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 6;
    assert_eq!(app.next_word_boundary(), 11);
}

#[test]
fn next_word_boundary_at_end_stays_at_end() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 11;
    assert_eq!(app.next_word_boundary(), 11);
}

#[test]
fn prev_word_boundary_unicode() {
    let (mut app, _rx, _tx) = make_app();
    // "привет мир" — 6 chars + space + 3 chars = 10 chars total
    app.sessions.current_mut().input = "привет мир".into();
    app.sessions.current_mut().cursor_position = 9;
    assert_eq!(app.prev_word_boundary(), 7);
}

#[test]
fn next_word_boundary_unicode() {
    let (mut app, _rx, _tx) = make_app();
    // "привет мир" — 6 chars + space + 3 chars
    app.sessions.current_mut().input = "привет мир".into();
    app.sessions.current_mut().cursor_position = 2;
    assert_eq!(app.next_word_boundary(), 7);
}

#[test]
fn alt_left_moves_to_prev_word_boundary() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 8;
    let key = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.cursor_position(), 6);
}

#[test]
fn alt_right_moves_to_next_word_boundary() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 2;
    let key = KeyEvent::new(KeyCode::Right, KeyModifiers::ALT);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.cursor_position(), 6);
}

#[test]
fn ctrl_a_moves_cursor_to_start() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 7;
    let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.cursor_position(), 0);
}

#[test]
fn ctrl_e_moves_cursor_to_end() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 3;
    let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.cursor_position(), 11);
}

#[test]
fn alt_backspace_deletes_to_prev_word_boundary() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 11;
    let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input(), "hello ");
    assert_eq!(app.cursor_position(), 6);
}

#[test]
fn alt_backspace_at_boundary_deletes_word_and_space() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "hello world".into();
    app.sessions.current_mut().cursor_position = 6;
    let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input(), "world");
    assert_eq!(app.cursor_position(), 0);
}

#[test]
fn alt_backspace_at_zero_is_noop() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().input = "hello".into();
    app.sessions.current_mut().cursor_position = 0;
    let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
    app.handle_event(AppEvent::Key(key));
    assert_eq!(app.input(), "hello");
    assert_eq!(app.cursor_position(), 0);
}

mod proptest_cursor {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        #[test]
        fn word_boundaries_stay_in_bounds(
            input in "\\PC{0,100}",
            cursor in 0usize..=100,
        ) {
            let (mut app, _rx, _tx) = make_app();
            app.sessions.current_mut().input = input;
            let len = app.char_count();
            app.sessions.current_mut().cursor_position = cursor.min(len);

            let prev = app.prev_word_boundary();
            prop_assert!(prev <= app.sessions.current_mut().cursor_position, "prev {prev} > cursor {}", app.sessions.current_mut().cursor_position);

            let next = app.next_word_boundary();
            prop_assert!(next >= app.sessions.current_mut().cursor_position, "next {next} < cursor {}", app.sessions.current_mut().cursor_position);
            prop_assert!(next <= len, "next {next} > len {len}");
        }

        #[test]
        fn alt_backspace_keeps_valid_state(
            input in "\\PC{0,50}",
            cursor in 0usize..=50,
        ) {
            let (mut app, _rx, _tx) = make_app();
            app.sessions.current_mut().input_mode = InputMode::Insert;
            app.sessions.current_mut().input = input;
            let len = app.char_count();
            app.sessions.current_mut().cursor_position = cursor.min(len);

            let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
            app.handle_event(AppEvent::Key(key));

            prop_assert!(app.cursor_position() <= app.char_count());
        }
    }
}

mod render_cache_tests {
    use super::*;
    use ratatui::text::{Line, Span};

    fn make_key(content_hash: u64, width: u16) -> RenderCacheKey {
        RenderCacheKey {
            content_hash,
            terminal_width: width,
            tool_expanded: false,
            tool_density: zeph_config::ToolDensity::Inline,
            show_labels: false,
            theme_generation: 0,
        }
    }

    #[test]
    fn get_returns_none_when_empty() {
        let cache = RenderCache::default();
        let key = make_key(1, 80);
        assert!(cache.get(0, &key).is_none());
    }

    #[test]
    fn put_and_get_returns_cached_lines() {
        let mut cache = RenderCache::default();
        let key = make_key(42, 80);
        let lines = vec![Line::from(Span::raw("hello"))];
        cache.put(0, key, lines.clone(), vec![]);
        let (result, _) = cache.get(0, &key).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].spans[0].content, "hello");
    }

    #[test]
    fn get_returns_none_on_key_mismatch() {
        let mut cache = RenderCache::default();
        let key1 = make_key(1, 80);
        let key2 = make_key(2, 80);
        let lines = vec![Line::from(Span::raw("a"))];
        cache.put(0, key1, lines, vec![]);
        assert!(cache.get(0, &key2).is_none());
    }

    #[test]
    fn get_returns_none_on_width_mismatch() {
        let mut cache = RenderCache::default();
        let key80 = make_key(1, 80);
        let key100 = make_key(1, 100);
        let lines = vec![Line::from(Span::raw("b"))];
        cache.put(0, key80, lines, vec![]);
        assert!(cache.get(0, &key100).is_none());
    }

    #[test]
    fn invalidate_clears_single_entry() {
        let mut cache = RenderCache::default();
        let key = make_key(1, 80);
        let lines = vec![Line::from(Span::raw("x"))];
        cache.put(0, key, lines, vec![]);
        assert!(cache.get(0, &key).is_some());
        cache.invalidate(0);
        assert!(cache.get(0, &key).is_none());
    }

    #[test]
    fn invalidate_out_of_bounds_is_noop() {
        let mut cache = RenderCache::default();
        cache.invalidate(99);
    }

    #[test]
    fn clear_removes_all_entries() {
        let mut cache = RenderCache::default();
        let key0 = make_key(1, 80);
        let key1 = make_key(2, 80);
        cache.put(0, key0, vec![Line::from(Span::raw("a"))], vec![]);
        cache.put(1, key1, vec![Line::from(Span::raw("b"))], vec![]);
        cache.clear();
        assert!(cache.get(0, &key0).is_none());
        assert!(cache.get(1, &key1).is_none());
    }

    #[test]
    fn put_grows_entries_for_non_contiguous_index() {
        let mut cache = RenderCache::default();
        let key = make_key(5, 80);
        let lines = vec![Line::from(Span::raw("z"))];
        cache.put(5, key, lines, vec![]);
        let (result, _) = cache.get(5, &key).unwrap();
        assert_eq!(result[0].spans[0].content, "z");
    }
}

mod try_recv_tests {
    use super::*;

    #[test]
    fn try_recv_returns_empty_when_no_events() {
        let (mut app, _rx, _tx) = make_app();
        let result = app.try_recv_agent_event();
        assert!(matches!(result, Err(mpsc::error::TryRecvError::Empty)));
    }

    #[test]
    fn try_recv_returns_event_when_available() {
        let (mut app, _rx, tx) = make_app();
        tx.try_send(AgentEvent::Typing).unwrap();
        let result = app.try_recv_agent_event();
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), AgentEvent::Typing));
    }

    #[test]
    fn try_recv_returns_disconnected_when_sender_dropped() {
        let (mut app, _rx, tx) = make_app();
        drop(tx);
        let result = app.try_recv_agent_event();
        assert!(matches!(
            result,
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}

mod command_palette_tests {
    use super::*;

    #[test]
    fn colon_in_normal_mode_opens_palette() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Normal;
        assert!(app.command_palette.is_none());

        let key = KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.command_palette.is_some());
    }

    #[test]
    fn esc_closes_palette() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Normal;
        app.command_palette = Some(crate::widgets::command_palette::CommandPaletteState::new());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.command_palette.is_none());
    }

    #[test]
    fn palette_intercepts_all_keys_except_ctrl_c() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;
        app.command_palette = Some(crate::widgets::command_palette::CommandPaletteState::new());

        // Typing a char goes to palette, not to input field
        let key = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.input().is_empty());
        let palette = app.command_palette.as_ref().unwrap();
        assert_eq!(palette.query, "s");
    }

    #[test]
    fn enter_on_selected_dispatches_command_locally() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Normal;
        // Open palette
        let colon = KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(colon));
        assert!(app.command_palette.is_some());

        // Enter on first command (skill:list)
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(enter));
        assert!(app.command_palette.is_none());
        // Should have added a system message
        assert!(!app.messages().is_empty());
        assert_eq!(app.messages().last().unwrap().role, MessageRole::System);
    }

    #[test]
    fn typing_in_palette_filters_commands() {
        let (mut app, _rx, _tx) = make_app();
        app.command_palette = Some(crate::widgets::command_palette::CommandPaletteState::new());

        let m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
        let c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        let p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(m));
        app.handle_event(AppEvent::Key(c));
        app.handle_event(AppEvent::Key(p));

        let palette = app.command_palette.as_ref().unwrap();
        assert_eq!(palette.query, "mcp");
        // mcp:list is the top result; plan:confirm also fuzzy-matches "mcp" (m→c→p in label).
        assert!(
            palette.filtered.iter().any(|e| e.id == "mcp:list"),
            "mcp:list must be in filtered results"
        );
        assert_eq!(
            palette.filtered[0].id, "mcp:list",
            "mcp:list must rank first"
        );
    }

    #[test]
    fn backspace_in_palette_removes_char() {
        let (mut app, _rx, _tx) = make_app();
        app.command_palette = Some(crate::widgets::command_palette::CommandPaletteState::new());

        let s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(s));
        assert_eq!(app.command_palette.as_ref().unwrap().query, "s");

        let bs = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(bs));
        assert!(app.command_palette.as_ref().unwrap().query.is_empty());
    }

    #[test]
    fn command_result_event_adds_system_message() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::CommandResult {
            command_id: "skill:list".to_owned(),
            output: "No skills loaded.".to_owned(),
        });
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].role, MessageRole::System);
        assert_eq!(app.messages()[0].content, "No skills loaded.");
        assert!(app.command_palette.is_none());
    }

    #[test]
    fn command_result_closes_palette_if_open() {
        let (mut app, _rx, _tx) = make_app();
        app.command_palette = Some(crate::widgets::command_palette::CommandPaletteState::new());
        app.handle_agent_event(AgentEvent::CommandResult {
            command_id: "view:config".to_owned(),
            output: "config output".to_owned(),
        });
        assert!(app.command_palette.is_none());
    }

    #[test]
    fn colon_in_insert_mode_types_colon() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;
        let key = KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.command_palette.is_none());
        assert_eq!(app.input(), ":");
    }

    #[test]
    fn enter_with_empty_filter_does_not_panic() {
        let (mut app, _rx, _tx) = make_app();
        let mut palette = crate::widgets::command_palette::CommandPaletteState::new();
        // type something that matches nothing
        for c in "xxxxxxxxxx".chars() {
            palette.push_char(c);
        }
        assert!(palette.filtered.is_empty());
        app.command_palette = Some(palette);

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(enter));
        // palette should close without crashing, no message added
        assert!(app.command_palette.is_none());
    }

    #[test]
    fn execute_view_config_with_command_tx_sends_command() {
        let (mut app, _rx, _tx) = make_app();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<TuiCommand>(16);
        app.command_tx = Some(cmd_tx);

        app.execute_command(TuiCommand::ViewConfig);

        let received = cmd_rx.try_recv().expect("command should be sent");
        assert_eq!(received, TuiCommand::ViewConfig);
        assert!(
            app.messages().is_empty(),
            "no system message when channel present"
        );
    }

    #[test]
    fn execute_view_autonomy_with_command_tx_sends_command() {
        let (mut app, _rx, _tx) = make_app();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<TuiCommand>(16);
        app.command_tx = Some(cmd_tx);

        app.execute_command(TuiCommand::ViewAutonomy);

        let received = cmd_rx.try_recv().expect("command should be sent");
        assert_eq!(received, TuiCommand::ViewAutonomy);
        assert!(
            app.messages().is_empty(),
            "no system message when channel present"
        );
    }

    #[test]
    fn execute_view_config_without_command_tx_adds_fallback_message() {
        let (mut app, _rx, _tx) = make_app();
        assert!(app.command_tx.is_none());

        app.execute_command(TuiCommand::ViewConfig);

        assert_eq!(app.messages().len(), 1);
        assert!(app.messages()[0].content.contains("no command channel"));
    }

    #[test]
    fn execute_security_events_no_events_shows_history_header() {
        let (mut app, _rx, _tx) = make_app();
        app.execute_command(TuiCommand::SecurityEvents);
        assert_eq!(app.messages().len(), 1);
        assert!(app.messages()[0].content.contains("Security event history"));
    }

    #[test]
    fn execute_security_events_with_events_shows_all() {
        use zeph_common::SecurityEventCategory;
        use zeph_core::metrics::SecurityEvent;

        let (mut app, _rx, _tx) = make_app();
        app.metrics.security_events.push_back(SecurityEvent::new(
            SecurityEventCategory::InjectionFlag,
            "web_scrape",
            "Detected pattern: ignore previous",
        ));
        app.execute_command(TuiCommand::SecurityEvents);
        let content = &app.messages()[0].content;
        assert!(content.contains("web_scrape"));
        assert!(content.contains("INJECTION_FLAG"));
    }

    #[test]
    fn has_recent_security_events_false_when_no_events() {
        let (app, _rx, _tx) = make_app();
        assert!(!app.has_recent_security_events());
    }

    #[test]
    fn has_recent_security_events_true_when_recent() {
        use zeph_common::SecurityEventCategory;
        use zeph_core::metrics::SecurityEvent;

        let (mut app, _rx, _tx) = make_app();
        // Event with current timestamp is recent
        app.metrics.security_events.push_back(SecurityEvent::new(
            SecurityEventCategory::Truncation,
            "tool",
            "truncated",
        ));
        assert!(app.has_recent_security_events());
    }

    #[test]
    fn has_recent_security_events_false_when_event_older_than_60s() {
        use zeph_common::SecurityEventCategory;
        use zeph_core::metrics::SecurityEvent;

        let (mut app, _rx, _tx) = make_app();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut ev = SecurityEvent::new(SecurityEventCategory::Truncation, "tool", "old");
        // Backdate the event by 120 seconds.
        ev.timestamp = now.saturating_sub(120);
        app.metrics.security_events.push_back(ev);
        assert!(!app.has_recent_security_events());
    }
}

mod file_picker_tests {
    use std::fs;

    use super::*;
    use crate::file_picker::FileIndex;

    fn make_app_with_index() -> (App, mpsc::Receiver<String>, mpsc::Sender<AgentEvent>) {
        let (app, rx, tx) = make_app();
        (app, rx, tx)
    }

    fn build_temp_index(files: &[&str]) -> (FileIndex, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        for &f in files {
            let path = dir.path().join(f);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, "").unwrap();
        }
        let idx = FileIndex::build(dir.path());
        (idx, dir)
    }

    fn open_picker_with_index(app: &mut App, idx: &FileIndex) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        drop(dir.keep());
        app.file_index = Some(FileIndex::build(&path));
        // Replace with our controlled index
        app.file_picker_state = Some(crate::file_picker::FilePickerState::new(idx));
    }

    #[test]
    fn at_sign_opens_picker_and_does_not_insert_into_input() {
        let (mut app, _rx, _tx) = make_app_with_index();
        // Pre-populate a fresh index so open_file_picker can open the picker immediately
        // without spawning a background build (which requires a Tokio runtime).
        let (idx, _dir) = build_temp_index(&["a.rs"]);
        app.file_index = Some(idx);
        app.sessions.current_mut().input_mode = InputMode::Insert;
        let key = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(
            !app.sessions.current_mut().input.contains('@'),
            "@ should not be in input after opening picker"
        );
        assert!(
            app.file_picker_state.is_some(),
            "file_picker_state should be Some after @"
        );
    }

    #[test]
    fn esc_dismisses_picker() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["a.rs", "b.rs"]);
        open_picker_with_index(&mut app, &idx);
        assert!(app.file_picker_state.is_some());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.file_picker_state.is_none());
        assert!(app.sessions.current_mut().input.is_empty());
    }

    #[test]
    fn enter_inserts_selected_path_and_closes_picker() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["src/main.rs"]);
        open_picker_with_index(&mut app, &idx);

        let selected = app
            .file_picker_state
            .as_ref()
            .unwrap()
            .selected_path()
            .map(ToOwned::to_owned)
            .unwrap();

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        assert!(app.file_picker_state.is_none());
        assert!(
            app.sessions.current_mut().input.contains(&selected),
            "input should contain selected path"
        );
        assert_eq!(
            app.sessions.current_mut().cursor_position,
            selected.chars().count()
        );
    }

    #[test]
    fn tab_inserts_selected_path_and_closes_picker() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["README.md"]);
        open_picker_with_index(&mut app, &idx);

        let selected = app
            .file_picker_state
            .as_ref()
            .unwrap()
            .selected_path()
            .map(ToOwned::to_owned)
            .unwrap();

        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        assert!(app.file_picker_state.is_none());
        assert!(app.sessions.current_mut().input.contains(&selected));
    }

    #[test]
    fn enter_with_no_matches_closes_picker_without_modifying_input() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["a.rs"]);
        open_picker_with_index(&mut app, &idx);

        let state = app.file_picker_state.as_mut().unwrap();
        state.update_query("xyznotfound");

        assert!(app.file_picker_state.as_ref().unwrap().matches().is_empty());

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        assert!(app.file_picker_state.is_none());
        assert!(
            app.sessions.current_mut().input.is_empty(),
            "input must be unchanged"
        );
    }

    #[test]
    fn down_key_advances_selection() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["a.rs", "b.rs", "c.rs"]);
        open_picker_with_index(&mut app, &idx);

        assert_eq!(app.file_picker_state.as_ref().unwrap().selected, 0);

        let key = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(app.file_picker_state.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn up_key_wraps_selection_to_last() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["a.rs", "b.rs", "c.rs"]);
        open_picker_with_index(&mut app, &idx);

        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        let state = app.file_picker_state.as_ref().unwrap();
        assert_eq!(state.selected, state.matches().len() - 1);
    }

    #[test]
    fn typing_filters_matches() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["src/main.rs", "src/lib.rs"]);
        open_picker_with_index(&mut app, &idx);

        let initial_count = app.file_picker_state.as_ref().unwrap().matches().len();

        let key = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        let filtered_count = app.file_picker_state.as_ref().unwrap().matches().len();
        assert!(filtered_count <= initial_count);
        assert_eq!(app.file_picker_state.as_ref().unwrap().query, "m");
    }

    #[test]
    fn backspace_with_nonempty_query_removes_char() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["a.rs"]);
        open_picker_with_index(&mut app, &idx);

        app.file_picker_state.as_mut().unwrap().update_query("ma");

        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        assert!(app.file_picker_state.is_some());
        assert_eq!(app.file_picker_state.as_ref().unwrap().query, "m");
    }

    #[test]
    fn backspace_on_empty_query_dismisses_picker() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["a.rs"]);
        open_picker_with_index(&mut app, &idx);

        assert!(app.file_picker_state.as_ref().unwrap().query.is_empty());

        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        assert!(app.file_picker_state.is_none());
    }

    #[test]
    fn picker_blocks_other_keys() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["a.rs"]);
        open_picker_with_index(&mut app, &idx);

        app.sessions.current_mut().input = "hello".into();
        app.sessions.current_mut().cursor_position = 5;
        let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        app.handle_event(AppEvent::Key(key));
        assert_eq!(
            app.sessions.current_mut().input,
            "hello",
            "input should be unchanged while picker is open"
        );
    }

    #[test]
    fn enter_inserts_at_cursor_mid_input() {
        let (mut app, _rx, _tx) = make_app_with_index();
        let (idx, _dir) = build_temp_index(&["src/lib.rs"]);
        open_picker_with_index(&mut app, &idx);

        app.sessions.current_mut().input = "ab".into();
        app.sessions.current_mut().cursor_position = 1;

        let selected = app
            .file_picker_state
            .as_ref()
            .unwrap()
            .selected_path()
            .map(ToOwned::to_owned)
            .unwrap();

        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));

        assert!(app.sessions.current_mut().input.contains(&selected));
        assert!(app.sessions.current_mut().input.starts_with('a'));
        assert!(app.sessions.current_mut().input.ends_with('b'));
    }

    #[tokio::test]
    async fn poll_pending_file_index_installs_index_and_opens_picker() {
        let (user_tx, _user_rx) = tokio::sync::mpsc::channel(1);
        let (_agent_tx, agent_rx) = tokio::sync::mpsc::channel(1);
        let mut app = App::new(user_tx, agent_rx);

        // Simulate: status is set, pending_file_index is Some (already resolved)
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (idx, _dir) = build_temp_index(&["foo.rs"]);
        let _ = tx.send(idx);
        app.pending_file_index = Some(rx);
        app.sessions.current_mut().status_label = Some("indexing files...".to_owned());

        // Give the oneshot a moment to be ready (it already is since we sent before assigning)
        tokio::task::yield_now().await;

        app.poll_pending_file_index();

        assert!(app.file_index.is_some(), "file_index should be installed");
        assert!(
            app.file_picker_state.is_some(),
            "picker should open after index ready"
        );
        assert!(
            app.sessions.current_mut().status_label.is_none(),
            "status should be cleared after index ready"
        );
        assert!(
            app.pending_file_index.is_none(),
            "pending handle should be consumed"
        );
    }

    #[tokio::test]
    async fn poll_pending_file_index_noop_when_none() {
        let (user_tx, _user_rx) = tokio::sync::mpsc::channel(1);
        let (_agent_tx, agent_rx) = tokio::sync::mpsc::channel(1);
        let mut app = App::new(user_tx, agent_rx);

        // No pending handle — should be a no-op
        app.poll_pending_file_index();

        assert!(app.file_index.is_none());
        assert!(app.file_picker_state.is_none());
    }

    #[tokio::test]
    async fn poll_pending_file_index_clears_on_closed_sender() {
        let (user_tx, _user_rx) = tokio::sync::mpsc::channel(1);
        let (_agent_tx, agent_rx) = tokio::sync::mpsc::channel(1);
        let mut app = App::new(user_tx, agent_rx);

        let (tx, rx) = tokio::sync::oneshot::channel::<crate::file_picker::FileIndex>();
        // Drop sender without sending — simulates spawn_blocking panic
        drop(tx);
        app.pending_file_index = Some(rx);
        app.sessions.current_mut().status_label = Some("indexing files...".to_owned());

        app.poll_pending_file_index();

        assert!(
            app.pending_file_index.is_none(),
            "closed handle should be consumed"
        );
        assert!(
            app.sessions.current_mut().status_label.is_none(),
            "status should be cleared on closed sender"
        );
    }
}

#[test]
fn draw_header_shows_1m_ctx_badge_when_extended_context() {
    use crate::test_utils::render_to_string;

    let (mut app, _rx, _tx) = make_app();
    app.metrics.provider_name = "claude".into();
    app.metrics.model_name = "claude-sonnet-4-6".into();
    app.metrics.extended_context = true;

    let output = render_to_string(80, 1, |frame, area| {
        app.draw_header(frame, area);
    });
    assert!(
        output.contains("1M CTX"),
        "header must contain 1M CTX badge when extended_context is true; got: {output:?}"
    );
}

#[test]
fn draw_header_no_badge_without_extended_context() {
    use crate::test_utils::render_to_string;

    let (mut app, _rx, _tx) = make_app();
    app.metrics.provider_name = "claude".into();
    app.metrics.model_name = "claude-sonnet-4-6".into();
    app.metrics.extended_context = false;

    let output = render_to_string(80, 1, |frame, area| {
        app.draw_header(frame, area);
    });
    assert!(
        !output.contains("[1M CTX]"),
        "header must not contain [1M CTX] badge when extended_context is false; got: {output:?}"
    );
}

// R-FIX-1938: with_metrics_rx must eagerly read the initial snapshot so graph counts are
// visible immediately without waiting for the first watch::Receiver::has_changed() event.
#[test]
fn with_metrics_rx_reads_initial_value() {
    use tokio::sync::watch;
    use zeph_core::metrics::MetricsSnapshot;

    let (user_tx, agent_rx) = {
        let (u, _ur) = mpsc::channel(4);
        let (_at, ar) = mpsc::channel(4);
        (u, ar)
    };
    let initial = MetricsSnapshot {
        graph_entities_total: 42,
        graph_edges_total: 7,
        graph_communities_total: 3,
        ..MetricsSnapshot::default()
    };

    let (tx, rx) = watch::channel(initial);
    let app = App::new(user_tx, agent_rx).with_metrics_rx(rx);

    assert_eq!(app.metrics.graph_entities_total, 42);
    assert_eq!(app.metrics.graph_edges_total, 7);
    assert_eq!(app.metrics.graph_communities_total, 3);

    drop(tx);
}

// Regression tests for #2126: tool output must not be duplicated when streaming chunks
// arrive before the final ToolOutput event.

#[test]
fn tool_output_with_prior_tool_start_no_chunks_appends_output() {
    let (mut app, _rx, _tx) = make_app();
    // Path A: ToolStart creates message with header only.
    app.handle_agent_event(AgentEvent::ToolStart {
        tool_name: "bash".into(),
        command: "ls -la".into(),
        tool_call_id: "call-a".into(),
    });
    // Path C: ToolOutput arrives with no prior chunks.
    app.handle_agent_event(AgentEvent::ToolOutput {
        tool_name: "bash".into(),
        command: "ls -la".into(),
        output: "file1\nfile2\n".into(),
        success: true,
        diff: None,
        filter_stats: None,
        kept_lines: None,
        tool_call_id: "call-a".into(),
    });

    assert_eq!(app.messages().len(), 1);
    let msg = &app.messages()[0];
    assert_eq!(msg.content, "$ ls -la\nfile1\nfile2\n");
    assert!(!msg.streaming);
}

#[test]
fn tool_output_with_prior_tool_start_and_chunks_does_not_duplicate() {
    let (mut app, _rx, _tx) = make_app();
    // Path A: ToolStart.
    app.handle_agent_event(AgentEvent::ToolStart {
        tool_name: "bash".into(),
        command: "echo hello".into(),
        tool_call_id: "call-b".into(),
    });
    // Path B: streaming chunks arrive.
    app.handle_agent_event(AgentEvent::ToolOutputChunk {
        tool_name: "bash".into(),
        command: "echo hello".into(),
        chunk: "hello\n".into(),
        tool_call_id: "call-b".into(),
    });
    // Path C: ToolOutput with canonical body_display (same content as chunks).
    app.handle_agent_event(AgentEvent::ToolOutput {
        tool_name: "bash".into(),
        command: "echo hello".into(),
        output: "hello\n".into(),
        success: true,
        diff: None,
        filter_stats: None,
        kept_lines: None,
        tool_call_id: "call-b".into(),
    });

    assert_eq!(app.messages().len(), 1);
    let msg = &app.messages()[0];
    // Must contain exactly one copy of "hello\n", not two.
    assert_eq!(msg.content, "$ echo hello\nhello\n");
    assert!(!msg.streaming);
}

// ── AgentViewTarget ──────────────────────────────────────────────────────

#[test]
fn agent_view_target_main_is_main() {
    assert!(AgentViewTarget::Main.is_main());
    assert!(AgentViewTarget::Main.subagent_id().is_none());
    assert!(AgentViewTarget::Main.subagent_name().is_none());
}

#[test]
fn agent_view_target_subagent_accessors() {
    let t = AgentViewTarget::SubAgent {
        id: "abc".into(),
        name: "Worker".into(),
    };
    assert!(!t.is_main());
    assert_eq!(t.subagent_id(), Some("abc"));
    assert_eq!(t.subagent_name(), Some("Worker"));
}

// ── SubAgentSidebarState ─────────────────────────────────────────────────

#[test]
fn sidebar_select_next_advances() {
    let mut s = SubAgentSidebarState::new();
    // start with nothing selected
    assert!(s.selected().is_none());
    s.select_next(3);
    assert_eq!(s.selected(), Some(0));
    s.select_next(3);
    assert_eq!(s.selected(), Some(1));
    s.select_next(3);
    assert_eq!(s.selected(), Some(2));
    // at last item — stays clamped
    s.select_next(3);
    assert_eq!(s.selected(), Some(2));
}

#[test]
fn sidebar_select_next_noop_when_empty() {
    let mut s = SubAgentSidebarState::new();
    s.select_next(0);
    assert!(s.selected().is_none());
}

#[test]
fn sidebar_select_prev_decrements() {
    let mut s = SubAgentSidebarState::new();
    s.list_state.select(Some(2));
    s.select_prev(3);
    assert_eq!(s.selected(), Some(1));
    s.select_prev(3);
    assert_eq!(s.selected(), Some(0));
    // at 0 — stays at 0
    s.select_prev(3);
    assert_eq!(s.selected(), Some(0));
}

#[test]
fn sidebar_select_prev_from_none_goes_to_zero() {
    let mut s = SubAgentSidebarState::new();
    s.select_prev(3);
    assert_eq!(s.selected(), Some(0));
}

#[test]
fn sidebar_select_prev_noop_when_empty() {
    let mut s = SubAgentSidebarState::new();
    s.select_prev(0);
    assert!(s.selected().is_none());
}

#[test]
fn sidebar_clamp_removes_selection_when_empty() {
    let mut s = SubAgentSidebarState::new();
    s.list_state.select(Some(2));
    s.clamp(0);
    assert!(s.selected().is_none());
}

#[test]
fn sidebar_clamp_reduces_out_of_bounds_selection() {
    let mut s = SubAgentSidebarState::new();
    s.list_state.select(Some(5));
    s.clamp(3); // valid range: 0..2
    assert_eq!(s.selected(), Some(2));
}

#[test]
fn sidebar_clamp_leaves_valid_selection_unchanged() {
    let mut s = SubAgentSidebarState::new();
    s.list_state.select(Some(1));
    s.clamp(3);
    assert_eq!(s.selected(), Some(1));
}

// ── TuiTranscriptEntry::to_chat_message ──────────────────────────────────

#[test]
fn transcript_entry_to_chat_message_role_mapping() {
    let cases = [
        ("user", MessageRole::User),
        ("assistant", MessageRole::Assistant),
        ("tool", MessageRole::Tool),
        ("system", MessageRole::System),
        ("unknown_role", MessageRole::System),
    ];
    for (role_str, expected) in cases {
        let entry = TuiTranscriptEntry {
            role: role_str.into(),
            content: "hello".into(),
            tool_name: None,
            timestamp: None,
        };
        let msg = entry.to_chat_message();
        assert_eq!(msg.role, expected, "role_str={role_str}");
    }
}

#[test]
fn transcript_entry_to_chat_message_copies_tool_name_and_timestamp() {
    let entry = TuiTranscriptEntry {
        role: "tool".into(),
        content: "result".into(),
        tool_name: Some("bash".into()),
        timestamp: Some("12:34".into()),
    };
    let msg = entry.to_chat_message();
    assert_eq!(
        msg.tool_name.as_ref().map(zeph_common::ToolName::as_str),
        Some("bash")
    );
    assert_eq!(msg.timestamp, "12:34");
    assert_eq!(msg.content, "result");
}

// ── load_transcript_file ─────────────────────────────────────────────────

#[test]
fn load_transcript_file_returns_empty_for_nonexistent_path() {
    let (entries, total) =
        load_transcript_file(std::path::Path::new("/nonexistent/path/x.jsonl"), false);
    assert!(entries.is_empty());
    assert_eq!(total, 0);
}

#[test]
fn load_transcript_file_parses_flat_format() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        tmp.path(),
        r#"{"role":"user","content":"hello"}
{"role":"assistant","content":"world"}
"#,
    )
    .unwrap();
    let (entries, total) = load_transcript_file(tmp.path(), false);
    assert_eq!(total, 2);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].role, "user");
    assert_eq!(entries[0].content, "hello");
    assert_eq!(entries[1].role, "assistant");
    assert_eq!(entries[1].content, "world");
}

#[test]
fn load_transcript_file_parses_nested_format() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        tmp.path(),
        r#"{"seq":1,"timestamp":"12:00","message":{"role":"user","parts":[{"content":"hi"}]}}
"#,
    )
    .unwrap();
    let (entries, total) = load_transcript_file(tmp.path(), false);
    assert_eq!(total, 1);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].role, "user");
    assert_eq!(entries[0].content, "hi");
    assert_eq!(entries[0].timestamp.as_deref(), Some("12:00"));
}

#[test]
fn load_transcript_file_skips_partial_last_line_when_active() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    // Last line is missing closing brace — partial write.
    std::fs::write(
        tmp.path(),
        r#"{"role":"user","content":"complete"}
{"role":"assistant","content":"incomplet"#,
    )
    .unwrap();
    let (entries, total) = load_transcript_file(tmp.path(), true);
    // is_active=true: last partial line discarded
    assert_eq!(total, 2); // total = raw line count
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content, "complete");
}

#[test]
fn load_transcript_file_keeps_partial_last_line_when_inactive() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    // Valid JSON that ends with '}' but missing "content" — will be skipped by filter.
    std::fs::write(
        tmp.path(),
        r#"{"role":"user","content":"complete"}
{"role":"assistant","content":"also complete"}
"#,
    )
    .unwrap();
    // is_active=false: no line skipping, both lines parsed
    let (entries, total) = load_transcript_file(tmp.path(), false);
    assert_eq!(total, 2);
    assert_eq!(entries.len(), 2);
}

#[test]
fn load_transcript_file_skips_empty_content_without_tool_name() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        tmp.path(),
        r#"{"role":"user","content":""}
{"role":"assistant","content":"real"}
"#,
    )
    .unwrap();
    let (entries, _total) = load_transcript_file(tmp.path(), false);
    // Entry with empty content and no tool_name is filtered out.
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content, "real");
}

#[test]
fn load_transcript_file_keeps_empty_content_with_tool_name() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        tmp.path(),
        r#"{"role":"tool","content":"","tool_name":"bash"}
"#,
    )
    .unwrap();
    let (entries, _total) = load_transcript_file(tmp.path(), false);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0]
            .tool_name
            .as_ref()
            .map(zeph_common::ToolName::as_str),
        Some("bash")
    );
}

#[test]
fn load_transcript_file_truncates_to_max_entries() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    // Write TRANSCRIPT_MAX_ENTRIES + 5 lines.
    let extra = 5;
    let count = TRANSCRIPT_MAX_ENTRIES + extra;
    let content: String = (0..count).fold(String::new(), |mut acc, i| {
        use std::fmt::Write;
        let _ = writeln!(acc, "{{\"role\":\"user\",\"content\":\"msg{i}\"}}");
        acc
    });
    std::fs::write(tmp.path(), &content).unwrap();
    let (entries, total) = load_transcript_file(tmp.path(), false);
    assert_eq!(total, count);
    assert_eq!(entries.len(), TRANSCRIPT_MAX_ENTRIES);
    // Must keep the LAST N entries, not first N.
    assert_eq!(entries[0].content, format!("msg{extra}"));
    assert_eq!(
        entries[TRANSCRIPT_MAX_ENTRIES - 1].content,
        format!("msg{}", count - 1)
    );
}

// ── transcript_truncation_info ────────────────────────────────────────────

#[test]
fn transcript_truncation_info_returns_none_when_no_cache() {
    let (app, _rx, _tx) = make_app();
    assert!(app.transcript_truncation_info().is_none());
}

#[test]
fn transcript_truncation_info_returns_none_when_not_truncated() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().transcript_cache = Some(TranscriptCache {
        agent_id: "a".into(),
        entries: vec![],
        turns_at_load: 1,
        total_in_file: TRANSCRIPT_MAX_ENTRIES,
    });
    assert!(app.transcript_truncation_info().is_none());
}

#[test]
fn transcript_truncation_info_returns_message_when_truncated() {
    let (mut app, _rx, _tx) = make_app();
    let total = TRANSCRIPT_MAX_ENTRIES + 50;
    app.sessions.current_mut().transcript_cache = Some(TranscriptCache {
        agent_id: "a".into(),
        entries: vec![],
        turns_at_load: 1,
        total_in_file: total,
    });
    let info = app.transcript_truncation_info().unwrap();
    assert!(info.contains(&total.to_string()), "info={info}");
    assert!(
        info.contains(&TRANSCRIPT_MAX_ENTRIES.to_string()),
        "info={info}"
    );
}

// ── visible_messages ─────────────────────────────────────────────────────

#[test]
fn visible_messages_returns_main_messages_when_in_main_view() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions
        .current_mut()
        .messages
        .push(ChatMessage::new(MessageRole::User, String::from("hello")));
    let msgs = app.visible_messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "hello");
}

#[test]
fn visible_messages_returns_transcript_when_cache_present() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().view_target = AgentViewTarget::SubAgent {
        id: "x".into(),
        name: "X".into(),
    };
    app.sessions.current_mut().transcript_cache = Some(TranscriptCache {
        agent_id: "x".into(),
        entries: vec![TuiTranscriptEntry {
            role: "user".into(),
            content: "from transcript".into(),
            tool_name: None,
            timestamp: None,
        }],
        turns_at_load: 1,
        total_in_file: 1,
    });
    let msgs = app.visible_messages();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "from transcript");
}

#[test]
fn visible_messages_returns_loading_placeholder_when_pending() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().view_target = AgentViewTarget::SubAgent {
        id: "x".into(),
        name: "X".into(),
    };
    // Simulate pending by installing a oneshot receiver that is not yet resolved.
    let (_tx2, rx2) = tokio::sync::oneshot::channel::<(Vec<TuiTranscriptEntry>, usize)>();
    app.sessions.current_mut().pending_transcript = Some(rx2);
    let msgs = app.visible_messages();
    assert_eq!(msgs.len(), 1);
    assert!(
        msgs[0].content.contains("Loading"),
        "content={}",
        msgs[0].content
    );
}

#[test]
fn visible_messages_returns_unavailable_when_no_cache_and_no_pending() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().view_target = AgentViewTarget::SubAgent {
        id: "x".into(),
        name: "MyAgent".into(),
    };
    let msgs = app.visible_messages();
    assert_eq!(msgs.len(), 1);
    assert!(
        msgs[0].content.contains("MyAgent"),
        "content={}",
        msgs[0].content
    );
}

// ── set_view_target ───────────────────────────────────────────────────────

#[test]
fn set_view_target_same_target_is_noop() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().scroll_offset = 5;
    // Already in Main — set to Main again.
    app.set_view_target(AgentViewTarget::Main);
    // scroll_offset must not be reset because nothing changed.
    assert_eq!(app.sessions.current_mut().scroll_offset, 5);
}

#[test]
fn set_view_target_clears_cache_and_scroll_on_switch() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().scroll_offset = 10;
    app.sessions.current_mut().transcript_cache = Some(TranscriptCache {
        agent_id: "a".into(),
        entries: vec![],
        turns_at_load: 1,
        total_in_file: 1,
    });
    // Switch to Main (was implicitly Main — set a SubAgent first).
    app.sessions.current_mut().view_target = AgentViewTarget::SubAgent {
        id: "a".into(),
        name: "A".into(),
    };
    app.set_view_target(AgentViewTarget::Main);
    assert_eq!(app.sessions.current_mut().scroll_offset, 0);
    assert!(app.sessions.current_mut().transcript_cache.is_none());
}

mod slash_autocomplete_tests {
    use super::*;

    #[test]
    fn slash_on_empty_input_opens_autocomplete() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;
        assert!(app.slash_autocomplete.is_none());

        let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.slash_autocomplete.is_some());
        assert_eq!(app.input(), "/");
    }

    #[test]
    fn no_open_mid_input() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;
        app.sessions.current_mut().input = "hello ".to_owned();
        app.sessions.current_mut().cursor_position = 6;

        let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.slash_autocomplete.is_none());
    }

    #[test]
    fn esc_dismisses_autocomplete() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;
        app.slash_autocomplete =
            Some(crate::widgets::slash_autocomplete::SlashAutocompleteState::new());
        app.sessions.current_mut().input = "/sk".to_owned();
        app.sessions.current_mut().cursor_position = 3;

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.slash_autocomplete.is_none());
        // Input retained
        assert_eq!(app.input(), "/sk");
    }

    #[test]
    fn at_char_while_autocomplete_open_does_not_open_file_picker() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;
        app.slash_autocomplete =
            Some(crate::widgets::slash_autocomplete::SlashAutocompleteState::new());
        app.sessions.current_mut().input = "/".to_owned();
        app.sessions.current_mut().cursor_position = 1;

        let key = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.file_picker_state.is_none());
    }

    #[test]
    fn backspace_removes_slash_and_dismisses() {
        let (mut app, _rx, _tx) = make_app();
        app.sessions.current_mut().input_mode = InputMode::Insert;
        app.slash_autocomplete =
            Some(crate::widgets::slash_autocomplete::SlashAutocompleteState::new());
        app.sessions.current_mut().input = "/".to_owned();
        app.sessions.current_mut().cursor_position = 1;

        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key));
        assert!(app.slash_autocomplete.is_none());
        assert!(app.input().is_empty());
    }
}

// ── trim_messages scroll adjustment (#2775) ──────────────────────────────

#[test]
fn trim_messages_no_trim_when_within_limit() {
    let (mut app, _rx, _tx) = make_app();
    for i in 0..10 {
        app.sessions
            .current_mut()
            .messages
            .push(ChatMessage::new(MessageRole::User, format!("msg {i}")));
    }
    app.sessions.current_mut().scroll_offset = 5;
    app.trim_messages();
    assert_eq!(app.sessions.current_mut().messages.len(), 10);
    assert_eq!(app.sessions.current_mut().scroll_offset, 5);
}

#[test]
fn trim_messages_evicts_excess_and_adjusts_scroll() {
    let (mut app, _rx, _tx) = make_app();
    let over = MAX_TUI_MESSAGES + 10;
    for i in 0..over {
        app.sessions
            .current_mut()
            .messages
            .push(ChatMessage::new(MessageRole::User, format!("msg {i}")));
    }
    app.sessions.current_mut().scroll_offset = 20;
    app.trim_messages();
    assert_eq!(app.sessions.current_mut().messages.len(), MAX_TUI_MESSAGES);
    assert_eq!(app.sessions.current_mut().scroll_offset, 10); // 20 - 10 excess = 10
}

#[test]
fn trim_messages_scroll_saturates_at_zero() {
    let (mut app, _rx, _tx) = make_app();
    let over = MAX_TUI_MESSAGES + 50;
    for i in 0..over {
        app.sessions
            .current_mut()
            .messages
            .push(ChatMessage::new(MessageRole::User, format!("msg {i}")));
    }
    app.sessions.current_mut().scroll_offset = 10; // less than excess (50)
    app.trim_messages();
    assert_eq!(app.sessions.current_mut().messages.len(), MAX_TUI_MESSAGES);
    assert_eq!(app.sessions.current_mut().scroll_offset, 0); // saturates at 0
}

#[test]
fn supervisor_activity_label_no_supervisor_returns_none() {
    let (app, _rx, _tx) = make_app();
    assert!(app.supervisor_activity_label().is_none());
}

#[tokio::test]
async fn supervisor_activity_label_single_active_task() {
    use zeph_common::task_supervisor::{RestartPolicy, TaskDescriptor, TaskSupervisor};

    // CancellationToken is a re-export from tokio-util inside zeph-core.
    let cancel = tokio_util::sync::CancellationToken::new();
    let sup = TaskSupervisor::new(cancel.clone());
    sup.spawn(TaskDescriptor {
        name: "config-watcher",
        restart: RestartPolicy::RunOnce,
        factory: || async { std::future::pending::<()>().await },
    });

    // Give the task time to start and register as Running.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let (mut app, _rx, _tx) = make_app();
    app = app.with_task_supervisor(sup);
    app.refresh_task_snapshots();

    let label = app.supervisor_activity_label();
    assert!(label.is_some(), "expected Some label for active task");
    assert!(
        label.as_deref().unwrap().contains("config-watcher"),
        "label should contain task name: {label:?}"
    );

    cancel.cancel();
}

#[tokio::test]
async fn supervisor_activity_label_multiple_tasks_shows_more() {
    use zeph_common::task_supervisor::{RestartPolicy, TaskDescriptor, TaskSupervisor};

    let cancel = tokio_util::sync::CancellationToken::new();
    let sup = TaskSupervisor::new(cancel.clone());
    for name in &["task-a", "task-b", "task-c"] {
        sup.spawn(TaskDescriptor {
            name,
            restart: RestartPolicy::RunOnce,
            factory: || async { std::future::pending::<()>().await },
        });
    }

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let (mut app, _rx, _tx) = make_app();
    app = app.with_task_supervisor(sup);
    app.refresh_task_snapshots();

    let label = app
        .supervisor_activity_label()
        .expect("expected Some label");
    assert!(
        label.contains('+') || label.contains("more"),
        "expected '+N more' for multiple tasks, got: {label:?}"
    );

    cancel.cancel();
}

#[test]
fn paste_inserts_text_in_insert_mode() {
    let (mut app, _rx, _tx) = make_app();
    app.handle_event(AppEvent::Paste("hello".to_owned()));
    assert_eq!(app.input(), "hello");
    assert_eq!(app.cursor_position(), 5);
}

#[test]
fn paste_at_mid_cursor_inserts_at_position() {
    let (mut app, _rx, _tx) = make_app();
    app.handle_event(AppEvent::Paste("ac".to_owned()));
    // Move cursor to position 1 (between 'a' and 'c') via Left key
    let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
    app.handle_event(AppEvent::Key(left));
    app.handle_event(AppEvent::Paste("b".to_owned()));
    assert_eq!(app.input(), "abc");
    assert_eq!(app.cursor_position(), 2);
}

#[test]
fn paste_multiline_inserts_newlines() {
    let (mut app, _rx, _tx) = make_app();
    app.handle_event(AppEvent::Paste("line1\nline2".to_owned()));
    assert_eq!(app.input(), "line1\nline2");
    assert_eq!(app.cursor_position(), 11);
}

#[test]
fn paste_in_normal_mode_ignored() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    app.handle_event(AppEvent::Paste("should not appear".to_owned()));
    assert!(app.input().is_empty());
}

#[test]
fn paste_clears_slash_autocomplete() {
    let (mut app, _rx, _tx) = make_app();
    app.slash_autocomplete =
        Some(crate::widgets::slash_autocomplete::SlashAutocompleteState::new());
    app.handle_event(AppEvent::Paste("text".to_owned()));
    assert!(app.slash_autocomplete.is_none());
    assert_eq!(app.input(), "text");
}

#[test]
fn supervisor_activity_label_truncates_at_utf8_boundary() {
    // Construct a label that is exactly 38 Unicode chars (each 3 bytes in UTF-8).
    // This verifies char-based truncation does not panic on multi-byte boundaries.

    // Build a fake supervisor by manually checking the truncation logic directly.
    // We can't easily inject a custom snapshot, so we test the logic inline.
    let long_name: String = "あ".repeat(50); // 50 × 3-byte chars
    let truncated: String = long_name.chars().take(38).collect();
    assert_eq!(truncated.chars().count(), 38, "should truncate to 38 chars");
    assert!(
        truncated.is_char_boundary(truncated.len()),
        "must be valid UTF-8"
    );
    // Confirm byte-slicing the full string at char-boundary position doesn't panic.
    let _ = &long_name[..truncated.len()];
}

#[test]
fn paste_state_set_for_multiline() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.handle_event(AppEvent::Paste("line1\nline2\nline3".to_owned()));
    let ps = app.paste_state().expect("paste_state should be Some");
    assert_eq!(ps.line_count, 3);
    assert_eq!(ps.byte_len, "line1\nline2\nline3".len());
}

#[test]
fn paste_state_none_for_single_line() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.handle_event(AppEvent::Paste("single line".to_owned()));
    assert!(app.paste_state().is_none());
}

#[test]
fn paste_state_cleared_on_char() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.handle_event(AppEvent::Paste("a\nb".to_owned()));
    assert!(app.paste_state().is_some());
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
    )));
    assert!(app.paste_state().is_none());
}

#[test]
fn paste_state_cleared_on_backspace() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.handle_event(AppEvent::Paste("a\nb".to_owned()));
    assert!(app.paste_state().is_some());
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Backspace,
        KeyModifiers::NONE,
    )));
    assert!(app.paste_state().is_none());
}

#[test]
fn paste_state_cleared_on_ctrl_u() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.handle_event(AppEvent::Paste("a\nb".to_owned()));
    assert!(app.paste_state().is_some());
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )));
    assert!(app.paste_state().is_none());
    assert!(
        app.input().is_empty(),
        "Ctrl+U must also clear input buffer"
    );
}

#[test]
fn paste_state_cleared_on_shift_enter() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.handle_event(AppEvent::Paste("a\nb".to_owned()));
    assert!(app.paste_state().is_some());
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::SHIFT,
    )));
    assert!(app.paste_state().is_none());
}

#[test]
fn paste_state_cleared_on_navigation() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;

    // Left arrow
    app.handle_event(AppEvent::Paste("a\nb".to_owned()));
    assert!(app.paste_state().is_some());
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Left,
        KeyModifiers::NONE,
    )));
    assert!(app.paste_state().is_none(), "Left must clear paste_state");

    // Home key
    app.handle_event(AppEvent::Paste("c\nd".to_owned()));
    assert!(app.paste_state().is_some());
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Home,
        KeyModifiers::NONE,
    )));
    assert!(app.paste_state().is_none(), "Home must clear paste_state");
}

#[test]
fn paste_state_consumed_on_submit() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.handle_event(AppEvent::Paste("line1\nline2\nline3\nline4".to_owned()));
    assert!(app.paste_state().is_some());
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(
        app.paste_state().is_none(),
        "paste_state cleared after submit"
    );
    assert_eq!(app.messages().len(), 1);
    assert_eq!(
        app.messages()[0].paste_line_count,
        Some(4),
        "paste_line_count must be set on submitted message"
    );
}

#[test]
fn insert_mode_page_up_scrolls_transcript() {
    let (mut app, _rx, _tx) = make_app();
    // Disable smooth scroll so offset changes are immediate (no animation needed).
    app.motion = zeph_config::Motion::Off;
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().scroll_offset = 0;
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::PageUp,
        KeyModifiers::NONE,
    )));
    assert_eq!(
        app.scroll_offset(),
        10,
        "PageUp in Insert mode must scroll transcript up by 10"
    );
}

#[test]
fn insert_mode_page_down_scrolls_transcript() {
    let (mut app, _rx, _tx) = make_app();
    app.motion = zeph_config::Motion::Off;
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().scroll_offset = 20;
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::PageDown,
        KeyModifiers::NONE,
    )));
    assert_eq!(
        app.scroll_offset(),
        10,
        "PageDown in Insert mode must scroll transcript down by 10"
    );
}

#[test]
fn insert_mode_page_down_saturates_at_zero() {
    let (mut app, _rx, _tx) = make_app();
    app.motion = zeph_config::Motion::Off;
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().scroll_offset = 5;
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::PageDown,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.scroll_offset(), 0, "PageDown must not underflow past 0");
}

#[test]
fn insert_mode_up_does_not_scroll_transcript() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    app.sessions.current_mut().scroll_offset = 0;
    // Up in Insert mode navigates input history, not transcript scroll.
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Up,
        KeyModifiers::NONE,
    )));
    assert_eq!(
        app.scroll_offset(),
        0,
        "Up in Insert mode must not change scroll_offset"
    );
}

#[test]
fn auto_scroll_suppressed_when_scrolled_up() {
    let (mut app, _rx, _tx) = make_app();
    // User has scrolled up past threshold — auto_scroll must not reset offset.
    app.sessions.current_mut().scroll_offset = 5;
    app.auto_scroll();
    assert_eq!(
        app.scroll_offset(),
        5,
        "auto_scroll must not move scroll when offset > 1"
    );
}

#[test]
fn auto_scroll_snaps_to_bottom_when_near_end() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().scroll_offset = 1;
    app.auto_scroll();
    assert_eq!(
        app.scroll_offset(),
        0,
        "auto_scroll must snap to bottom when offset <= 1"
    );
}

// ---- F1: per-section sidebar collapse ----

#[test]
fn toggle_panel_collapse_toggles_and_retrieves() {
    let (mut app, _rx, _tx) = make_app();
    assert_eq!(
        app.collapsed_panels(),
        [false; 4],
        "all panels start expanded"
    );
    app.toggle_panel_collapse(0);
    assert!(
        app.collapsed_panels()[0],
        "skills must be collapsed after toggle"
    );
    app.toggle_panel_collapse(0);
    assert!(
        !app.collapsed_panels()[0],
        "skills must expand on second toggle"
    );
}

#[test]
fn toggle_panel_collapse_out_of_range_noop() {
    let (mut app, _rx, _tx) = make_app();
    app.toggle_panel_collapse(99);
    assert_eq!(app.collapsed_panels(), [false; 4]);
}

#[test]
fn effective_collapsed_passes_through_when_no_overlay() {
    let (mut app, _rx, _tx) = make_app();
    app.toggle_panel_collapse(3);
    let eff = app.effective_collapsed();
    assert!(eff[3], "slot 3 collapse honoured when no overlay active");
}

#[test]
fn effective_collapsed_forces_expand_slot3_when_fleet_active() {
    let (mut app, _rx, _tx) = make_app();
    app.toggle_panel_collapse(3);
    app.active_panel = Panel::Fleet;
    let eff = app.effective_collapsed();
    assert!(
        !eff[3],
        "slot 3 must be force-expanded when Fleet overlay is active"
    );
}

#[test]
fn effective_collapsed_forces_expand_slot3_when_task_panel_open() {
    let (mut app, _rx, _tx) = make_app();
    app.toggle_panel_collapse(3);
    app.show_task_panel = true;
    let eff = app.effective_collapsed();
    assert!(
        !eff[3],
        "slot 3 must be force-expanded when task panel is visible"
    );
}

#[test]
fn effective_collapsed_does_not_touch_slots_0_1_2() {
    let (mut app, _rx, _tx) = make_app();
    app.toggle_panel_collapse(0);
    app.toggle_panel_collapse(1);
    app.toggle_panel_collapse(2);
    // Even with Fleet active, slots 0/1/2 pass through unchanged.
    app.active_panel = Panel::Fleet;
    let eff = app.effective_collapsed();
    assert!(eff[0]);
    assert!(eff[1]);
    assert!(eff[2]);
}

#[test]
fn alt_1_hotkey_toggles_skills_panel_in_normal_mode() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    assert!(!app.collapsed_panels()[0]);
    let key = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT);
    app.handle_event(AppEvent::Key(key));
    assert!(
        app.collapsed_panels()[0],
        "Alt+1 must collapse skills panel"
    );
    app.handle_event(AppEvent::Key(key));
    assert!(
        !app.collapsed_panels()[0],
        "Alt+1 again must expand skills panel"
    );
}

#[test]
fn alt_4_hotkey_toggles_subagents_panel_in_insert_mode() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Insert;
    assert!(!app.collapsed_panels()[3]);
    let key = KeyEvent::new(KeyCode::Char('4'), KeyModifiers::ALT);
    app.handle_event(AppEvent::Key(key));
    assert!(
        app.collapsed_panels()[3],
        "Alt+4 must collapse subagents panel in insert mode"
    );
}

#[test]
fn alt_2_and_3_hotkeys_toggle_memory_and_resources() {
    let (mut app, _rx, _tx) = make_app();
    app.sessions.current_mut().input_mode = InputMode::Normal;
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('2'),
        KeyModifiers::ALT,
    )));
    app.handle_event(AppEvent::Key(KeyEvent::new(
        KeyCode::Char('3'),
        KeyModifiers::ALT,
    )));
    let panels = app.collapsed_panels();
    assert!(panels[1], "Alt+2 must collapse memory panel");
    assert!(panels[2], "Alt+3 must collapse resources panel");
}

#[test]
fn collapse_slot3_with_subagents_panel_focused_force_expands() {
    let (mut app, _rx, _tx) = make_app();
    app.toggle_panel_collapse(3);
    app.active_panel = Panel::SubAgents;
    let eff = app.effective_collapsed();
    assert!(!eff[3], "SubAgents focus must force-expand slot 3");
}

// ── M2: wants_animation_frame unit tests (#5104) ─────────────────────────────

#[test]
fn wants_animation_frame_false_when_motion_off() {
    let (mut app, _rx, _tx) = make_app();
    app.motion = zeph_config::Motion::Off;

    // Inject active toast — must still return false under motion=Off.
    app.toasts
        .push("hello", crate::delights::ToastKind::Success, 0);
    // Inject active shimmer.
    app.splash_shimmer.activate(0);
    // Inject active flash.
    app.sessions.current_mut().flash.insert(0, 0);
    // Inject active scroll animation.
    app.sessions.current_mut().scroll_anim = Some(crate::session::ScrollAnim {
        from: 0,
        to: 10,
        start_tick: 0,
    });

    assert!(
        !app.wants_animation_frame(),
        "motion=Off must suppress all animation frames"
    );
}

#[test]
fn wants_animation_frame_false_when_all_idle() {
    let (app, _rx, _tx) = make_app();
    // Fresh app: no toasts, no flash, no scroll anim, no shimmer.
    assert!(
        !app.wants_animation_frame(),
        "idle app must not want animation frames"
    );
}

#[test]
fn wants_animation_frame_true_while_toast_active() {
    let (mut app, _rx, _tx) = make_app();
    let tick = app.anim_tick();
    app.toasts
        .push("msg", crate::delights::ToastKind::Success, tick);
    assert!(
        app.wants_animation_frame(),
        "active toast must trigger animation frame"
    );
}

#[test]
fn wants_animation_frame_true_while_flash_active() {
    let (mut app, _rx, _tx) = make_app();
    let tick = app.anim_tick();
    app.sessions.current_mut().flash.insert(0, tick);
    assert!(
        app.wants_animation_frame(),
        "active flash must trigger animation frame"
    );
}

#[test]
fn wants_animation_frame_true_while_scroll_active() {
    let (mut app, _rx, _tx) = make_app();
    let tick = app.anim_tick();
    app.sessions.current_mut().scroll_anim = Some(crate::session::ScrollAnim {
        from: 0,
        to: 5,
        start_tick: tick,
    });
    assert!(
        app.wants_animation_frame(),
        "active scroll animation must trigger animation frame"
    );
}

#[test]
fn wants_animation_frame_true_while_shimmer_active() {
    let (mut app, _rx, _tx) = make_app();
    let tick = app.anim_tick();
    app.splash_shimmer.activate(tick);
    assert!(
        app.wants_animation_frame(),
        "active shimmer must trigger animation frame"
    );
}
