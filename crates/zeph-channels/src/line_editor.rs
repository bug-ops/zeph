use std::io::{self, Write, stdout};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{self, ClearType},
};

pub enum ReadLineResult {
    Line(String),
    Interrupted,
    Eof,
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

        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                write!(stdout(), "\r\n")?;
                stdout().flush()?;
                return Ok(ReadLineResult::Interrupted);
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                if input.is_empty() {
                    write!(stdout(), "\r\n")?;
                    stdout().flush()?;
                    return Ok(ReadLineResult::Eof);
                }
            }
            (_, KeyCode::Enter) => {
                write!(stdout(), "\r\n")?;
                stdout().flush()?;
                return Ok(ReadLineResult::Line(input));
            }
            (KeyModifiers::CONTROL, KeyCode::Char('a')) | (_, KeyCode::Home) => {
                cursor_pos = 0;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('e')) | (_, KeyCode::End) => {
                cursor_pos = char_count(&input);
            }
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                input.clear();
                cursor_pos = 0;
            }
            (KeyModifiers::ALT, KeyCode::Backspace) => {
                let boundary = prev_word_boundary(&input, cursor_pos);
                let start = byte_offset(&input, boundary);
                let end = byte_offset(&input, cursor_pos);
                input.drain(start..end);
                cursor_pos = boundary;
            }
            (_, KeyCode::Backspace) => {
                if cursor_pos > 0 {
                    let off = byte_offset(&input, cursor_pos - 1);
                    input.remove(off);
                    cursor_pos -= 1;
                }
            }
            (_, KeyCode::Delete) => {
                if cursor_pos < char_count(&input) {
                    let off = byte_offset(&input, cursor_pos);
                    input.remove(off);
                }
            }
            (_, KeyCode::Left) => {
                cursor_pos = cursor_pos.saturating_sub(1);
            }
            (_, KeyCode::Right) => {
                if cursor_pos < char_count(&input) {
                    cursor_pos += 1;
                }
            }
            (_, KeyCode::Up) => {
                navigate_history_up(
                    history,
                    &mut input,
                    &mut cursor_pos,
                    &mut history_index,
                    &mut draft,
                );
            }
            (_, KeyCode::Down) => {
                navigate_history_down(
                    history,
                    &mut input,
                    &mut cursor_pos,
                    &mut history_index,
                    &mut draft,
                );
            }
            (_, KeyCode::Char(c)) => {
                let off = byte_offset(&input, cursor_pos);
                input.insert(off, c);
                cursor_pos += 1;
            }
            _ => {}
        }

        render(prompt, &input, cursor_pos)?;
    }
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
    use super::*;

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
