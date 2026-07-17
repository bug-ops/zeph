// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Minimal terminal line editor used by [`CliChannel`].
//!
//! This module provides reading functions that cover the CLI's stdin modes:
//!
//! * [`read_line`] — for interactive TTY sessions.  Uses crossterm raw mode to
//!   implement cursor movement, history navigation, and `Ctrl-C`/`Ctrl-D`
//!   handling without relying on any external readline library.
//! * [`read_line_yieldable`] — same as `read_line`, but polls for events
//!   instead of blocking, so it can voluntarily relinquish exclusive terminal
//!   access to a concurrent elicitation/confirmation prompt (see
//!   [`ReadLineResult::Yielded`]).
//! * [`read_line_piped`] — for non-TTY (piped) stdin.  Reads one line at a
//!   time from a [`BufRead`] source with a 1 MiB safety limit.
//!
//! All functions return [`ReadLineResult`] so the caller can handle EOF,
//! interruption, and normal input in a single `match`.
//!
//! [`CliChannel`]: crate::CliChannel
//! [`BufRead`]: std::io::BufRead

use std::io::{self, BufRead, Read, Write, stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{self, ClearType},
};

/// The outcome of a single readline call.
///
/// Both [`read_line`] (TTY) and [`read_line_piped`] (non-TTY) return this type
/// so the caller can handle all three cases uniformly.
#[non_exhaustive]
#[derive(Debug)]
pub enum ReadLineResult {
    /// A complete line was read.  The trailing newline is stripped.
    Line(String),
    /// The user pressed `Ctrl-C` (TTY only).
    Interrupted,
    /// End-of-file was reached (`Ctrl-D` on empty input, or the pipe closed).
    Eof,
    /// The read was voluntarily abandoned because another caller (an active
    /// elicitation/confirmation prompt) needs exclusive terminal access.
    ///
    /// Only ever returned by [`read_line_yieldable`].
    Yielded,
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Maximum number of bytes read per line in piped mode to prevent OOM on unterminated input.
const MAX_LINE_LEN: u64 = 1 << 20; // 1 MiB

/// Read a single line from `reader` without raw mode (for piped/non-TTY stdin).
///
/// Reads at most [`MAX_LINE_LEN`] bytes. Input that exceeds this limit is silently truncated.
/// Returns `ReadLineResult::Eof` when the reader is exhausted (0 bytes read).
///
/// # Errors
///
/// Returns `io::Error` on underlying I/O failure.
pub fn read_line_piped<R: BufRead>(reader: &mut R) -> io::Result<ReadLineResult> {
    let mut buf = String::new();
    let n = reader.take(MAX_LINE_LEN).read_line(&mut buf)?;
    if n == 0 {
        return Ok(ReadLineResult::Eof);
    }
    // Strip trailing newline characters
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
    }
    Ok(ReadLineResult::Line(buf))
}

/// Read a single line from the terminal with readline-style editing.
///
/// Enables crossterm raw mode for the duration of the call (restored on return
/// or panic via [`RawModeGuard`]).  Supports:
///
/// * Left / Right arrow — character-level cursor movement
/// * Home / End, `Ctrl-A` / `Ctrl-E` — jump to line start / end
/// * Backspace / Delete — character deletion
/// * `Alt-Backspace` — delete the previous word
/// * `Ctrl-U` — clear the entire line
/// * Up / Down arrow — history navigation (prefix-aware)
/// * `Ctrl-C` — return [`ReadLineResult::Interrupted`]
/// * `Ctrl-D` on an empty line — return [`ReadLineResult::Eof`]
///
/// This function is **blocking** and must be called from
/// [`tokio::task::spawn_blocking`] when used inside an async context.
///
/// # Errors
///
/// Returns `io::Error` if enabling raw mode, reading an event, or writing to
/// stdout fails.
pub fn read_line(prompt: &str, history: &[String]) -> io::Result<ReadLineResult> {
    let _guard = RawModeGuard::enter()?;

    let mut input = String::new();
    let mut cursor_pos: usize = 0;
    let mut history_index: Option<usize> = None;
    let mut draft = String::new();

    render(prompt, &input, cursor_pos)?;

    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        // Ignore release/repeat events on platforms that send them
        if key.kind != event::KeyEventKind::Press {
            continue;
        }

        if let Some(result) = handle_key_event(
            key,
            &mut input,
            &mut cursor_pos,
            history,
            &mut history_index,
            &mut draft,
        )? {
            return Ok(result);
        }

        render(prompt, &input, cursor_pos)?;
    }
}

/// Read a single line from the terminal, yielding back to the caller when
/// `yield_requested` becomes `true` instead of blocking indefinitely inside
/// `crossterm::event::read()`.
///
/// Behaves exactly like [`read_line`] except that it checks `yield_requested`
/// at the top of every loop iteration and polls for the next event with a
/// short timeout (50ms) instead of blocking on it directly. Checking the flag
/// before `event::poll` (rather than only inside the poll-timeout branch) is
/// required so a set flag preempts even under continuous sub-50ms-gap
/// keystroke arrivals, where `poll` would otherwise always return `true` and
/// the timeout branch would never run. When the flag is set, the call returns
/// [`ReadLineResult::Yielded`] instead of continuing to wait — this lets a
/// concurrent caller (e.g. an active elicitation/confirmation prompt) take
/// over exclusive terminal access without racing this reader for keystrokes.
///
/// This closes the *starvation* window (an in-progress keystroke stream)
/// completely, but there remains a bounded ~50ms *acquisition* window between
/// the concurrent caller setting `yield_requested` and this loop's current
/// `event::poll` call returning — accepted as an intentional MVP tradeoff
/// (the common case is a user reacting to a freshly-printed prompt, not
/// already mid-keystroke) rather than adding a full ack/notify handshake.
///
/// Any input typed so far is discarded when yielding.
///
/// # Errors
///
/// Returns `io::Error` if enabling raw mode, polling/reading an event, or
/// writing to stdout fails.
pub fn read_line_yieldable(
    prompt: &str,
    history: &[String],
    yield_requested: &AtomicBool,
) -> io::Result<ReadLineResult> {
    let _guard = RawModeGuard::enter()?;

    let mut input = String::new();
    let mut cursor_pos: usize = 0;
    let mut history_index: Option<usize> = None;
    let mut draft = String::new();

    render(prompt, &input, cursor_pos)?;

    loop {
        // Checked before polling (not just on timeout) so a continuous
        // sub-50ms keystroke stream cannot starve this check indefinitely.
        if yield_requested.load(Ordering::Acquire) {
            return Ok(ReadLineResult::Yielded);
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != event::KeyEventKind::Press {
            continue;
        }

        if let Some(result) = handle_key_event(
            key,
            &mut input,
            &mut cursor_pos,
            history,
            &mut history_index,
            &mut draft,
        )? {
            return Ok(result);
        }

        render(prompt, &input, cursor_pos)?;
    }
}

/// Apply a single key event to the in-progress input line.
///
/// Returns `Ok(Some(result))` when the line is complete (Enter, `Ctrl-C`, or
/// `Ctrl-D` on empty input) and the caller should stop reading; `Ok(None)` to
/// keep looping. Shared between [`read_line`] and [`read_line_yieldable`] so
/// the two entry points stay behaviorally identical.
fn handle_key_event(
    key: KeyEvent,
    input: &mut String,
    cursor_pos: &mut usize,
    history: &[String],
    history_index: &mut Option<usize>,
    draft: &mut String,
) -> io::Result<Option<ReadLineResult>> {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            write!(stdout(), "\r\n")?;
            stdout().flush()?;
            return Ok(Some(ReadLineResult::Interrupted));
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) if input.is_empty() => {
            write!(stdout(), "\r\n")?;
            stdout().flush()?;
            return Ok(Some(ReadLineResult::Eof));
        }
        (_, KeyCode::Enter) => {
            write!(stdout(), "\r\n")?;
            stdout().flush()?;
            return Ok(Some(ReadLineResult::Line(std::mem::take(input))));
        }
        (KeyModifiers::CONTROL, KeyCode::Char('a')) | (_, KeyCode::Home) => {
            *cursor_pos = 0;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('e')) | (_, KeyCode::End) => {
            *cursor_pos = char_count(input);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            input.clear();
            *cursor_pos = 0;
        }
        (KeyModifiers::ALT, KeyCode::Backspace) => {
            let boundary = prev_word_boundary(input, *cursor_pos);
            let start = byte_offset(input, boundary);
            let end = byte_offset(input, *cursor_pos);
            input.drain(start..end);
            *cursor_pos = boundary;
        }
        (_, KeyCode::Backspace) if *cursor_pos > 0 => {
            let off = byte_offset(input, *cursor_pos - 1);
            input.remove(off);
            *cursor_pos -= 1;
        }
        (_, KeyCode::Delete) if *cursor_pos < char_count(input) => {
            let off = byte_offset(input, *cursor_pos);
            input.remove(off);
        }
        (_, KeyCode::Left) => {
            *cursor_pos = cursor_pos.saturating_sub(1);
        }
        (_, KeyCode::Right) if *cursor_pos < char_count(input) => {
            *cursor_pos += 1;
        }
        (_, KeyCode::Up) => {
            navigate_history_up(history, input, cursor_pos, history_index, draft);
        }
        (_, KeyCode::Down) => {
            navigate_history_down(history, input, cursor_pos, history_index, draft);
        }
        (_, KeyCode::Char(c)) => {
            let off = byte_offset(input, *cursor_pos);
            input.insert(off, c);
            *cursor_pos += 1;
        }
        _ => {}
    }

    Ok(None)
}

fn navigate_history_up(
    history: &[String],
    input: &mut String,
    cursor_pos: &mut usize,
    history_index: &mut Option<usize>,
    draft: &mut String,
) {
    match *history_index {
        None => {
            if history.is_empty() {
                return;
            }
            draft.clone_from(input);
            let prefix = &*draft;
            let found = history
                .iter()
                .rposition(|e| prefix.is_empty() || e.starts_with(prefix));
            let Some(idx) = found else { return };
            *history_index = Some(idx);
            input.clone_from(&history[idx]);
        }
        Some(i) => {
            let prefix = &*draft;
            let found = history[..i]
                .iter()
                .rposition(|e| prefix.is_empty() || e.starts_with(prefix));
            let Some(idx) = found else { return };
            *history_index = Some(idx);
            input.clone_from(&history[idx]);
        }
    }
    *cursor_pos = char_count(input);
}

fn navigate_history_down(
    history: &[String],
    input: &mut String,
    cursor_pos: &mut usize,
    history_index: &mut Option<usize>,
    draft: &mut String,
) {
    let Some(i) = *history_index else { return };
    let prefix = &*draft;
    let found = history[i + 1..]
        .iter()
        .position(|e| prefix.is_empty() || e.starts_with(prefix))
        .map(|offset| i + 1 + offset);
    if let Some(idx) = found {
        *history_index = Some(idx);
        input.clone_from(&history[idx]);
    } else {
        *history_index = None;
        *input = std::mem::take(draft);
    }
    *cursor_pos = char_count(input);
}

fn render(prompt: &str, input: &str, cursor_pos: usize) -> io::Result<()> {
    let mut out = stdout();
    let prefix: String = input.chars().take(cursor_pos).collect();
    let cursor_col = prompt.len() + unicode_display_width(&prefix);
    write!(
        out,
        "\r{}{}{}{}",
        terminal::Clear(ClearType::CurrentLine),
        prompt,
        input,
        cursor::MoveToColumn(u16::try_from(cursor_col).unwrap_or(u16::MAX)),
    )?;
    out.flush()
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn byte_offset(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
}

fn prev_word_boundary(s: &str, cursor: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut i = cursor;
    while i > 0 && !chars[i - 1].is_alphanumeric() {
        i -= 1;
    }
    while i > 0 && chars[i - 1].is_alphanumeric() {
        i -= 1;
    }
    i
}

fn unicode_display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(s)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::io::Cursor;

    use super::*;

    #[test]
    fn read_line_piped_returns_line() {
        let mut reader = Cursor::new(b"hello world\n");
        let result = read_line_piped(&mut reader).unwrap();
        assert_matches!(result, ReadLineResult::Line(l) if l == "hello world");
    }

    #[test]
    fn read_line_piped_strips_crlf() {
        let mut reader = Cursor::new(b"hello\r\n");
        let result = read_line_piped(&mut reader).unwrap();
        assert_matches!(result, ReadLineResult::Line(l) if l == "hello");
    }

    #[test]
    fn read_line_piped_returns_eof_on_empty() {
        let mut reader = Cursor::new(b"");
        let result = read_line_piped(&mut reader).unwrap();
        assert_matches!(result, ReadLineResult::Eof);
    }

    #[test]
    fn read_line_piped_no_newline_at_eof() {
        let mut reader = Cursor::new(b"no newline");
        let result = read_line_piped(&mut reader).unwrap();
        assert_matches!(result, ReadLineResult::Line(l) if l == "no newline");
    }

    #[test]
    fn read_line_piped_multi_line_sequence() {
        let mut reader = Cursor::new(b"line1\nline2\n");
        let r1 = read_line_piped(&mut reader).unwrap();
        assert_matches!(r1, ReadLineResult::Line(l) if l == "line1");
        let r2 = read_line_piped(&mut reader).unwrap();
        assert_matches!(r2, ReadLineResult::Line(l) if l == "line2");
        let r3 = read_line_piped(&mut reader).unwrap();
        assert_matches!(r3, ReadLineResult::Eof);
    }

    #[test]
    fn char_count_ascii() {
        assert_eq!(char_count("hello"), 5);
        assert_eq!(char_count(""), 0);
    }

    #[test]
    fn char_count_unicode() {
        assert_eq!(char_count("héllo"), 5);
        assert_eq!(char_count("日本語"), 3);
    }

    #[test]
    fn byte_offset_start() {
        assert_eq!(byte_offset("hello", 0), 0);
    }

    #[test]
    fn byte_offset_end() {
        assert_eq!(byte_offset("hello", 5), 5);
    }

    #[test]
    fn byte_offset_beyond() {
        assert_eq!(byte_offset("hello", 100), 5);
    }

    #[test]
    fn byte_offset_unicode() {
        // "é" is 2 bytes, so char index 1 = byte offset 2
        let s = "éllo";
        assert_eq!(byte_offset(s, 1), 2);
    }

    #[test]
    fn prev_word_boundary_from_end() {
        // "hello world" cursor at 11 (end), boundary should be at start of "world"=6
        assert_eq!(prev_word_boundary("hello world", 11), 6);
    }

    #[test]
    fn prev_word_boundary_at_start() {
        assert_eq!(prev_word_boundary("hello", 0), 0);
    }

    #[test]
    fn prev_word_boundary_skips_spaces() {
        // "hello   world" cursor after spaces at 8, boundary = after "hello" at 5? no, past spaces
        // spaces are non-alphanumeric, then alphanumeric of "hello"
        assert_eq!(prev_word_boundary("hello   world", 8), 0);
    }

    #[test]
    fn navigate_history_up_empty_history_no_op() {
        let history: Vec<String> = vec![];
        let mut input = String::from("test");
        let mut cursor = 4;
        let mut idx = None;
        let mut draft = String::new();
        navigate_history_up(&history, &mut input, &mut cursor, &mut idx, &mut draft);
        assert_eq!(input, "test");
        assert!(idx.is_none());
    }

    #[test]
    fn navigate_history_up_selects_last_entry() {
        let history = vec!["cmd1".to_string(), "cmd2".to_string()];
        let mut input = String::new();
        let mut cursor = 0;
        let mut idx = None;
        let mut draft = String::new();
        navigate_history_up(&history, &mut input, &mut cursor, &mut idx, &mut draft);
        assert_eq!(input, "cmd2");
        assert_eq!(idx, Some(1));
        assert_eq!(cursor, 4);
    }

    #[test]
    fn navigate_history_up_twice_goes_further_back() {
        let history = vec!["cmd1".to_string(), "cmd2".to_string()];
        let mut input = String::new();
        let mut cursor = 0;
        let mut idx = None;
        let mut draft = String::new();
        navigate_history_up(&history, &mut input, &mut cursor, &mut idx, &mut draft);
        navigate_history_up(&history, &mut input, &mut cursor, &mut idx, &mut draft);
        assert_eq!(input, "cmd1");
        assert_eq!(idx, Some(0));
    }

    #[test]
    fn navigate_history_down_restores_draft() {
        let history = vec!["cmd1".to_string()];
        // Simulate having gone up: idx is Some(0), input is the history entry
        let mut input = String::from("cmd1");
        let mut cursor = 4;
        let mut idx = Some(0);
        // Draft preserves what the user typed before navigating up
        let mut draft = String::from("draft");
        // Now go back down — should restore draft
        navigate_history_down(&history, &mut input, &mut cursor, &mut idx, &mut draft);
        assert_eq!(input, "draft");
        assert!(idx.is_none());
    }

    #[test]
    fn navigate_history_down_no_op_when_no_index() {
        let history = vec!["cmd1".to_string()];
        let mut input = String::from("unchanged");
        let mut cursor = 9;
        let mut idx = None;
        let mut draft = String::new();
        navigate_history_down(&history, &mut input, &mut cursor, &mut idx, &mut draft);
        assert_eq!(input, "unchanged");
    }
}
