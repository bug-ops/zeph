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
