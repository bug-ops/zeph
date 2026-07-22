// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;
use zeph_config::Motion;

use crate::app::{App, InputMode};
use crate::theme::Theme;
use crate::widgets::spinner::breeze_frame;
use zeph_common::text::format_tokens;

/// Prompt glyph shown at the beginning of the separator line.
const PROMPT_GLYPH: &str = "you ▸";

/// Append the animated spinner glyph and interrupt hint to a busy separator row.
///
/// No activity label is shown here — the busy verb already lives in the bottom
/// status bar ([`crate::widgets::status`]) and the wave equalizer in the side
/// panel, so duplicating it on this row would be redundant.
///
/// Used by `Motion::Minimal` (breeze spinner) and `Motion::Off` (static `·`).
fn push_busy_tail<'a>(
    spans: &mut Vec<Span<'a>>,
    spinner_idx: u8,
    app: &App,
    theme: &'a Theme,
    animated: bool,
) {
    let symbol = if animated {
        breeze_frame(u64::from(spinner_idx), app.is_ascii_only())
    } else {
        "·"
    };
    spans.push(Span::styled(format!("  {symbol}"), theme.highlight));
    spans.push(Span::styled("  ctrl+c to interrupt", theme.system_message));
}

/// Mode hint shown on the separator row, accurate to what the keys actually do.
///
/// `Normal` → press `i` to start typing; `Insert` → `Esc` switches back to
/// Normal mode (it does not cancel input or interrupt the agent).
fn mode_hint(app: &App) -> &'static str {
    match app.input_mode() {
        InputMode::Normal => "press 'i' to type",
        InputMode::Insert => "esc for normal mode",
    }
}

/// Build the busy separator row for `Motion::Minimal` (animated) and `Motion::Off` (static).
///
/// Layout: prompt glyph + mode hint + token estimate + queued badge + spinner +
/// interrupt hint. The only difference between modes is whether the spinner glyph
/// animates (`animated = true`).
fn build_spinner_busy_sep<'a>(
    spinner_idx: u8,
    app: &'a App,
    theme: &'a Theme,
    animated: bool,
) -> Vec<Span<'a>> {
    let estimate = app.context_token_estimate();
    let meta = if estimate > 0 {
        format!("  ~{} tokens", format_tokens(estimate as u64))
    } else {
        String::new()
    };
    let mut spans: Vec<Span<'_>> = vec![
        Span::styled(format!("{PROMPT_GLYPH} "), theme.system_message),
        Span::styled(mode_hint(app), theme.system_message),
        Span::styled(meta, theme.system_message),
    ];
    if app.queued_count() > 0 {
        spans.push(Span::styled(
            format!("  [+{} queued]", app.queued_count()),
            theme.highlight,
        ));
    }
    if app.editing_queued() {
        spans.push(Span::styled("  [editing queued]", theme.highlight));
    }
    push_busy_tail(&mut spans, spinner_idx, app, theme, animated);
    spans
}

/// Build the idle separator row (agent not busy, all motion modes share this layout).
fn build_idle_sep<'a>(app: &'a App, theme: &'a Theme) -> Vec<Span<'a>> {
    let estimate = app.context_token_estimate();
    let meta = if estimate > 0 {
        format!("  ~{} tokens", format_tokens(estimate as u64))
    } else {
        String::new()
    };
    let mut spans: Vec<Span<'_>> = vec![
        Span::styled(format!("{PROMPT_GLYPH} "), theme.system_message),
        Span::styled(mode_hint(app), theme.system_message),
        Span::styled(meta, theme.system_message),
    ];
    if app.queued_count() > 0 {
        spans.push(Span::styled(
            format!("  [+{} queued]", app.queued_count()),
            theme.highlight,
        ));
    }
    if app.editing_queued() {
        spans.push(Span::styled("  [editing queued]", theme.highlight));
    }
    spans
}

/// Render the editable text area and update the terminal cursor position.
fn render_text_area(app: &App, frame: &mut Frame, text_area: Rect, busy: bool) {
    let theme = &app.theme;
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
    } else if app.input().is_empty() && matches!(app.input_mode(), InputMode::Insert) && !busy {
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

pub fn render(
    app: &App,
    frame: &mut Frame,
    area: Rect,
    busy: bool,
    spinner_idx: u8,
    motion: Motion,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let theme = &app.theme;

    let sep_spans: Vec<Span<'_>> = if busy {
        match motion {
            Motion::Off => build_spinner_busy_sep(spinner_idx, app, theme, false),
            _ => build_spinner_busy_sep(spinner_idx, app, theme, true),
        }
    } else {
        build_idle_sep(app, theme)
    };

    frame.render_widget(
        Paragraph::new(Line::from(sep_spans)),
        Rect { height: 1, ..area },
    );
    render_text_area(
        app,
        frame,
        Rect {
            y: area.y.saturating_add(1),
            height: area.height.saturating_sub(1),
            ..area
        },
        busy,
    );
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

    fn render_input(
        app: &App,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        busy: bool,
        spinner_idx: u8,
        motion: zeph_config::Motion,
    ) {
        super::render(app, frame, area, busy, spinner_idx, motion);
    }

    #[test]
    fn input_insert_mode() {
        let app = make_app();
        let output = render_to_string(40, 5, |frame, area| {
            render_input(&app, frame, area, false, 0, zeph_config::Motion::Full);
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
            render_input(&app, frame, area, false, 0, zeph_config::Motion::Full);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn input_busy_shows_spinner() {
        let app = make_app();
        let output = render_to_string(60, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Minimal);
        });
        assert!(
            output.contains("ctrl+c to interrupt"),
            "spinner hint must appear when busy"
        );
    }

    #[test]
    fn input_idle_width_80() {
        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            render_input(&app, frame, area, false, 0, zeph_config::Motion::Full);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn input_busy_width_40() {
        let app = make_app();
        let output = render_to_string(40, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Minimal);
        });
        // The busy verb is NOT duplicated here (it lives in the status bar); the
        // separator shows the prompt glyph + mode hint, which fit at width 40.
        assert!(
            output.contains("you ▸"),
            "prompt glyph must appear when busy; got: {output:?}"
        );
        assert!(
            !output.contains("thinking"),
            "activity label must NOT be duplicated in the input separator; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_width_80() {
        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Minimal);
        });
        assert!(
            output.contains("ctrl+c to interrupt"),
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
            render_input(&app, frame, area, false, 0, zeph_config::Motion::Full);
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
            render_input(&app, frame, area, false, 0, zeph_config::Motion::Full);
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

    #[test]
    fn input_busy_full_shows_spinner() {
        // Motion::Full busy path — shows animated spinner same as Minimal.
        // The equalizer is rendered in the side panel, not in the input area.
        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Full);
        });
        assert!(
            output.contains("you ▸"),
            "prompt glyph must appear in Full+busy; got: {output:?}"
        );
        assert!(
            output.contains("ctrl+c to interrupt"),
            "interrupt hint must appear in Full+busy mode; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_full_narrow() {
        let app = make_app();
        let output = render_to_string(20, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Full);
        });
        assert!(
            output.contains("you ▸"),
            "prompt glyph must appear on narrow terminal; got: {output:?}"
        );
        assert!(
            !output.contains("Type a message"),
            "placeholder must be hidden when busy; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_motion_off() {
        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Off);
        });
        assert!(
            output.contains("ctrl+c to interrupt"),
            "interrupt hint must appear in Off mode; got: {output:?}"
        );
        assert!(
            !output.contains('•'),
            "no dot-matrix bullet should appear under Motion::Off; got: {output:?}"
        );
    }
}
