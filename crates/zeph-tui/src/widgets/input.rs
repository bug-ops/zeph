// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use throbber_widgets_tui::BRAILLE_SIX;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, InputMode};
use zeph_common::text::format_tokens;

/// Prompt glyph shown at the beginning of the separator line.
const PROMPT_GLYPH: &str = "›";

pub fn render(
    app: &App,
    frame: &mut Frame,
    area: Rect,
    busy: bool,
    activity_label: Option<&str>,
    spinner_idx: u8,
) {
    let theme = &app.theme;

    // Build the separator line that replaces the former top border.
    // Format: "› mode_hint  [meta…]"
    let mode_hint = match app.input_mode() {
        InputMode::Normal => "press 'i' to type",
        InputMode::Insert => "esc to cancel",
    };
    let estimate = app.context_token_estimate();
    let meta = if estimate > 0 {
        format!("  ~{} tokens", format_tokens(estimate as u64))
    } else {
        String::new()
    };

    let mut sep_spans: Vec<Span<'_>> = vec![
        Span::styled(format!("{PROMPT_GLYPH} "), theme.highlight),
        Span::styled(mode_hint, theme.system_message),
        Span::styled(meta, theme.system_message),
    ];

    if app.queued_count() > 0 {
        sep_spans.push(Span::styled(
            format!("  [+{} queued]", app.queued_count()),
            theme.highlight,
        ));
    }
    if app.editing_queued() {
        sep_spans.push(Span::styled("  [editing queued]", theme.highlight));
    }
    if busy {
        let sym_idx = usize::from(spinner_idx) % BRAILLE_SIX.symbols.len();
        let symbol = BRAILLE_SIX.symbols[sym_idx];
        let spinner_span = if let Some(label) = activity_label {
            Span::styled(format!("  {symbol} {label}"), theme.highlight)
        } else {
            Span::styled(format!("  {symbol}"), theme.highlight)
        };
        sep_spans.push(spinner_span);
        sep_spans.push(Span::styled("  esc to interrupt", theme.system_message));
    }

    // Render separator line in the top row of the area.
    if area.height >= 1 {
        let sep_area = Rect { height: 1, ..area };
        frame.render_widget(Paragraph::new(Line::from(sep_spans)), sep_area);
    }

    // The text area starts one row below the separator.
    let text_area = Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    };

    let block = Block::default();

    let visible_lines = text_area.height;
    let cursor_line = u16::try_from(
        app.input()[..app
            .input()
            .char_indices()
            .nth(app.cursor_position())
            .map_or(app.input().len(), |(idx, _)| idx)]
            .matches('\n')
            .count(),
    )
    .unwrap_or(u16::MAX);
    let scroll = cursor_line.saturating_sub(visible_lines.saturating_sub(1));

    let paragraph = if let Some(ps) = app.paste_state() {
        // Show compact indicator while multiline paste is pending in the buffer.
        // Cursor is not shown — the user cannot edit within the indicator display.
        let size_label = if ps.byte_len >= 1024 {
            // Integer KB with one decimal place; precision loss at >4 PB is acceptable.
            #[allow(clippy::cast_precision_loss)]
            let kb = ps.byte_len as f64 / 1024.0;
            format!("{kb:.1} KB")
        } else {
            format!("{} B", ps.byte_len)
        };
        let indicator = format!("[Pasted: {} lines · {}]", ps.line_count, size_label);
        Paragraph::new(indicator)
            .block(block)
            .style(theme.system_message)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false })
    } else if app.input().is_empty() && matches!(app.input_mode(), InputMode::Insert) {
        Paragraph::new("Type a message, / for commands, @ to mention")
            .block(block)
            .style(theme.system_message)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false })
    } else {
        Paragraph::new(app.input())
            .block(block)
            .style(theme.input_text)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false })
    };

    frame.render_widget(paragraph, text_area);

    // Do not show cursor when paste indicator is active — the user interacts
    // with the indicator as a whole unit, not individual characters.
    if app.paste_state().is_none() && matches!(app.input_mode(), InputMode::Insert) {
        let prefix: String = app.input().chars().take(app.cursor_position()).collect();
        let last_line = prefix.rsplit('\n').next().unwrap_or(&prefix);
        #[allow(clippy::cast_possible_truncation)]
        let cursor_x = text_area.x + last_line.width() as u16;
        let line_count = u16::try_from(prefix.matches('\n').count()).unwrap_or(u16::MAX);
        #[allow(clippy::cast_possible_truncation)]
        let cursor_y = text_area.y + line_count.saturating_sub(scroll);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use tokio::sync::mpsc;

    use crate::app::App;
    use crate::test_utils::render_to_string;

    fn make_app() -> App {
        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        App::new(user_tx, agent_rx)
    }

    #[test]
    fn input_insert_mode() {
        let app = make_app();
        let output = render_to_string(40, 5, |frame, area| {
            super::render(&app, frame, area, false, None, 0);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn input_normal_mode() {
        let mut app = make_app();
        app.handle_event(crate::event::AppEvent::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
        ));
        let output = render_to_string(40, 5, |frame, area| {
            super::render(&app, frame, area, false, None, 0);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn input_busy_shows_spinner() {
        let app = make_app();
        let output = render_to_string(60, 5, |frame, area| {
            super::render(&app, frame, area, true, Some("Thinking..."), 0);
        });
        assert!(
            output.contains("esc to interrupt"),
            "spinner hint must appear when busy"
        );
    }

    #[test]
    fn input_idle_width_80() {
        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            super::render(&app, frame, area, false, None, 0);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn input_busy_width_40() {
        let app = make_app();
        let output = render_to_string(40, 5, |frame, area| {
            super::render(&app, frame, area, true, Some("Thinking..."), 0);
        });
        // On a 40-column terminal the full "esc to interrupt" hint may be truncated;
        // verify that the activity label is present, which confirms the spinner path ran.
        assert!(
            output.contains("Thinking"),
            "activity label must appear when busy; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_width_80() {
        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            super::render(&app, frame, area, true, Some("Thinking..."), 0);
        });
        assert!(
            output.contains("esc to interrupt"),
            "spinner hint must appear on wide terminal"
        );
    }

    #[test]
    fn input_shows_token_estimate_when_nonzero() {
        let mut app = make_app();
        app.handle_event(crate::event::AppEvent::Agent(
            crate::event::AgentEvent::ContextEstimate(14_200),
        ));
        let output = render_to_string(80, 5, |frame, area| {
            super::render(&app, frame, area, false, None, 0);
        });
        assert!(
            output.contains("14.2k tokens"),
            "token estimate must appear in input block title when estimate is nonzero"
        );
    }

    #[test]
    fn input_hides_token_estimate_when_zero() {
        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            super::render(&app, frame, area, false, None, 0);
        });
        assert!(
            !output.contains("tokens"),
            "token estimate must not appear when estimate is 0"
        );
    }

    #[test]
    fn format_token_count_below_1000() {
        assert_eq!(format!("~{}", zeph_common::text::format_tokens(0)), "~0");
        assert_eq!(
            format!("~{}", zeph_common::text::format_tokens(512)),
            "~512"
        );
        assert_eq!(
            format!("~{}", zeph_common::text::format_tokens(999)),
            "~999"
        );
    }

    #[test]
    fn format_token_count_1000_and_above() {
        assert_eq!(
            format!("~{}", zeph_common::text::format_tokens(1000)),
            "~1.0k"
        );
        assert_eq!(
            format!("~{}", zeph_common::text::format_tokens(1500)),
            "~1.5k"
        );
        assert_eq!(
            format!("~{}", zeph_common::text::format_tokens(14_200)),
            "~14.2k"
        );
        assert_eq!(
            format!("~{}", zeph_common::text::format_tokens(100_000)),
            "~100.0k"
        );
    }
}
