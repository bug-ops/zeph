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
use crate::widgets::status_verbs::humanize;
use crate::widgets::wave::{WaveState, glyphs};
use zeph_common::text::format_tokens;

/// Prompt glyph shown at the beginning of the separator line.
const PROMPT_GLYPH: &str = "›";

/// Append human-voice label spans to the separator line (no animated glyph).
///
/// Used by `Motion::Minimal` (breeze spinner) and `Motion::Off` (static).
/// Callers pass `animated_glyph = false` for `Off`.
fn push_label_spans<'a>(
    spans: &mut Vec<Span<'a>>,
    spinner_idx: u8,
    activity_label: Option<&'a str>,
    app: &App,
    theme: &'a Theme,
    animated: bool,
) {
    let symbol = if animated {
        breeze_frame(u64::from(spinner_idx), app.is_ascii_only())
    } else {
        // Static glyph — no animated symbol under Motion::Off; other separator content (token estimate, label) still reflects live state.
        "·"
    };
    if let Some(label) = activity_label {
        let phrase = humanize(label);
        if phrase.verb.is_empty() {
            spans.push(Span::styled(format!("  {symbol}"), theme.highlight));
        } else {
            spans.push(Span::styled(
                format!("  {symbol} {}", phrase.verb),
                theme.highlight,
            ));
            if !phrase.detail.is_empty() {
                spans.push(Span::styled(
                    format!(" · {}", phrase.detail),
                    theme.system_message,
                ));
            }
        }
    } else {
        spans.push(Span::styled(format!("  {symbol}"), theme.highlight));
    }
    spans.push(Span::styled("  esc to interrupt", theme.system_message));
}

/// Build the busy separator row for `Motion::Full`.
///
/// Layout (left → right):
/// 1. `› ` prompt glyph (2 columns, always present).
/// 2. Activity label: `verb · detail  esc to interrupt` — measured via unicode width.
/// 3. Wave right-fills the remaining columns (`wave_w = width - prompt_w - label_w - 1`).
///    The `+1` is a space gutter between label and wave. `wave_w = 0` → wave skipped.
///
/// The `~N tokens` estimate and mode hint are deliberately omitted while busy+Full to
/// free width for the wave.
fn build_full_busy_sep<'a>(
    area_width: u16,
    activity_label: Option<&'a str>,
    app: &'a App,
    wave_state: WaveState,
    wave_tick: u64,
    theme: &'a Theme,
    wave_buf: &mut Vec<Span<'static>>,
) -> Vec<Span<'static>> {
    const PROMPT_W: u16 = 2; // "› " width

    // Build the human-voice label text first so we can measure it.
    let label_text: String = if let Some(label) = activity_label {
        let phrase = humanize(label);
        if phrase.verb.is_empty() {
            "  esc to interrupt".to_owned()
        } else if phrase.detail.is_empty() {
            format!("  {} · esc to interrupt", phrase.verb)
        } else {
            format!("  {} · {}  esc to interrupt", phrase.verb, phrase.detail)
        }
    } else {
        "  esc to interrupt".to_owned()
    };

    #[allow(clippy::cast_possible_truncation)]
    let label_w = label_text.width() as u16;

    let wave_w = area_width
        .saturating_sub(PROMPT_W)
        .saturating_sub(label_w)
        .saturating_sub(1); // gutter

    // Build spans — all 'static lifetime via owned Strings.
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(format!("{PROMPT_GLYPH} "), theme.highlight));
    spans.push(Span::styled(label_text, theme.system_message));

    if wave_w > 0 {
        spans.push(Span::raw(" ")); // gutter
        let color_mode = app.effective_color_mode();
        let ascii = app.is_ascii_only();
        glyphs(
            wave_state,
            u32::from(wave_w),
            wave_tick,
            color_mode,
            ascii,
            wave_buf,
            theme,
        );
        spans.extend_from_slice(wave_buf);
    }

    spans
}

/// Build the busy separator row for `Motion::Minimal` (animated) and `Motion::Off` (static).
///
/// Both modes share the same layout — prompt glyph + mode hint + token estimate + queued badge
/// + activity label. The only difference is whether the label glyph animates (`animated = true`).
fn build_spinner_busy_sep<'a>(
    spinner_idx: u8,
    activity_label: Option<&'a str>,
    app: &'a App,
    theme: &'a Theme,
    animated: bool,
) -> Vec<Span<'a>> {
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
    let mut spans: Vec<Span<'_>> = vec![
        Span::styled(format!("{PROMPT_GLYPH} "), theme.highlight),
        Span::styled(mode_hint, theme.system_message),
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
    push_label_spans(
        &mut spans,
        spinner_idx,
        activity_label,
        app,
        theme,
        animated,
    );
    spans
}

/// Build the idle separator row (agent not busy, all motion modes share this layout).
fn build_idle_sep<'a>(app: &'a App, theme: &'a Theme) -> Vec<Span<'a>> {
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
    let mut spans: Vec<Span<'_>> = vec![
        Span::styled(format!("{PROMPT_GLYPH} "), theme.highlight),
        Span::styled(mode_hint, theme.system_message),
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
fn render_text_area(app: &App, frame: &mut Frame, text_area: Rect) {
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

#[allow(clippy::too_many_arguments)]
pub fn render(
    app: &App,
    frame: &mut Frame,
    area: Rect,
    busy: bool,
    activity_label: Option<&str>,
    spinner_idx: u8,
    wave_state: WaveState,
    wave_tick: u64,
    motion: Motion,
    wave_buf: &mut Vec<Span<'static>>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let theme = &app.theme;

    let sep_spans: Vec<Span<'_>> = if busy {
        match motion {
            Motion::Full => build_full_busy_sep(
                area.width,
                activity_label,
                app,
                wave_state,
                wave_tick,
                theme,
                wave_buf,
            ),
            Motion::Minimal => {
                build_spinner_busy_sep(spinner_idx, activity_label, app, theme, true)
            }
            Motion::Off => build_spinner_busy_sep(spinner_idx, activity_label, app, theme, false),
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

    /// Thin wrapper so existing tests don't need to manage `wave_buf` themselves.
    #[allow(clippy::too_many_arguments)]
    fn render_input(
        app: &App,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        busy: bool,
        activity_label: Option<&str>,
        spinner_idx: u8,
        wave_state: crate::widgets::wave::WaveState,
        wave_tick: u64,
        motion: zeph_config::Motion,
    ) {
        let mut buf = Vec::new();
        super::render(
            app,
            frame,
            area,
            busy,
            activity_label,
            spinner_idx,
            wave_state,
            wave_tick,
            motion,
            &mut buf,
        );
    }

    #[test]
    fn input_insert_mode() {
        let app = make_app();
        let output = render_to_string(40, 5, |frame, area| {
            render_input(
                &app,
                frame,
                area,
                false,
                None,
                0,
                crate::widgets::wave::WaveState::Idle,
                0,
                zeph_config::Motion::Full,
            );
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
            render_input(
                &app,
                frame,
                area,
                false,
                None,
                0,
                crate::widgets::wave::WaveState::Idle,
                0,
                zeph_config::Motion::Full,
            );
        });
        assert_snapshot!(output);
    }

    #[test]
    fn input_busy_shows_spinner() {
        let app = make_app();
        let output = render_to_string(60, 5, |frame, area| {
            render_input(
                &app,
                frame,
                area,
                true,
                Some("Thinking..."),
                0,
                crate::widgets::wave::WaveState::Swell,
                0,
                zeph_config::Motion::Minimal,
            );
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
            render_input(
                &app,
                frame,
                area,
                false,
                None,
                0,
                crate::widgets::wave::WaveState::Idle,
                0,
                zeph_config::Motion::Full,
            );
        });
        assert_snapshot!(output);
    }

    #[test]
    fn input_busy_width_40() {
        let app = make_app();
        let output = render_to_string(40, 5, |frame, area| {
            render_input(
                &app,
                frame,
                area,
                true,
                Some("Thinking..."),
                0,
                crate::widgets::wave::WaveState::Swell,
                0,
                zeph_config::Motion::Minimal,
            );
        });
        // On a 40-column terminal the full "esc to interrupt" hint may be truncated;
        // humanize() lowercases the verb, so check for "thinking" (lowercase).
        assert!(
            output.contains("thinking"),
            "activity label must appear when busy; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_width_80() {
        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            render_input(
                &app,
                frame,
                area,
                true,
                Some("Thinking..."),
                0,
                crate::widgets::wave::WaveState::Swell,
                0,
                zeph_config::Motion::Minimal,
            );
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
            render_input(
                &app,
                frame,
                area,
                false,
                None,
                0,
                crate::widgets::wave::WaveState::Idle,
                0,
                zeph_config::Motion::Full,
            );
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
            render_input(
                &app,
                frame,
                area,
                false,
                None,
                0,
                crate::widgets::wave::WaveState::Idle,
                0,
                zeph_config::Motion::Full,
            );
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

    // C2: Motion::Full busy-path tests (wave render integration)

    #[test]
    fn input_busy_full_streaming_width_80() {
        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            render_input(
                &app,
                frame,
                area,
                true,
                Some("Streaming..."),
                0,
                crate::widgets::wave::WaveState::Streaming,
                5,
                zeph_config::Motion::Full,
            );
        });
        // Separator must have the prompt glyph and label.
        assert!(
            output.contains('›'),
            "prompt glyph must appear in Full+busy; got: {output:?}"
        );
        assert!(
            output.contains("esc to interrupt"),
            "interrupt hint must appear; got: {output:?}"
        );
        // Wave glyphs OR the label fills the row — no block-element crash check.
        assert!(
            !output.is_empty(),
            "render must not produce empty output; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_full_narrow_wave_skipped() {
        // width=20 is narrow enough that wave_w saturates to 0 after prompt (2) + label (~18).
        let app = make_app();
        let output = render_to_string(20, 5, |frame, area| {
            render_input(
                &app,
                frame,
                area,
                true,
                None,
                0,
                crate::widgets::wave::WaveState::Swell,
                0,
                zeph_config::Motion::Full,
            );
        });
        // No wave block characters when wave_w == 0.
        for glyph in &["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"] {
            assert!(
                !output.contains(glyph),
                "no wave glyphs should appear on narrow terminal; got: {output:?}"
            );
        }
        // Label must still be present.
        assert!(
            output.contains('›'),
            "prompt glyph must appear even on narrow terminal; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_motion_off() {
        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            render_input(
                &app,
                frame,
                area,
                true,
                Some("Streaming..."),
                0,
                crate::widgets::wave::WaveState::Streaming,
                5,
                zeph_config::Motion::Off,
            );
        });
        // Interrupt hint must be present.
        assert!(
            output.contains("esc to interrupt"),
            "interrupt hint must appear in Off mode; got: {output:?}"
        );
        // No block-element wave glyphs.
        for glyph in &["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"] {
            assert!(
                !output.contains(glyph),
                "no wave glyphs should appear under Motion::Off; got: {output:?}"
            );
        }
    }
}
