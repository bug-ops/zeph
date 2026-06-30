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
use crate::widgets::wave::{EQ_ROWS, EQ_W_MAX, EqualizerWidget, WaveState};
use zeph_common::text::format_tokens;

/// Prompt glyph shown at the beginning of the separator line.
const PROMPT_GLYPH: &str = "you ▸";

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
        Span::styled(format!("{PROMPT_GLYPH} "), theme.system_message),
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
        Span::styled(format!("{PROMPT_GLYPH} "), theme.system_message),
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
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let theme = &app.theme;

    if busy && matches!(motion, Motion::Full) && app.show_equalizer {
        // Compact 2-row equalizer: prompt glyph on row 0, then EqualizerWidget.
        const PROMPT_W: u16 = 6; // "you ▸ " width
        let sep_height = EQ_ROWS.min(area.height.saturating_sub(1));
        if sep_height > 0 {
            // Prompt glyph on the first row only.
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("{PROMPT_GLYPH} "),
                    theme.system_message,
                ))),
                Rect {
                    x: area.x,
                    y: area.y,
                    width: PROMPT_W,
                    height: 1,
                },
            );
            // Bounded equalizer to the right of the prompt.
            let eq_x = area.x.saturating_add(PROMPT_W);
            let eq_avail = area.width.saturating_sub(PROMPT_W);
            #[allow(clippy::cast_possible_truncation)]
            let eq_w = eq_avail.min(EQ_W_MAX as u16);
            if eq_w > 0 {
                frame.render_widget(
                    EqualizerWidget {
                        state: wave_state,
                        tick: wave_tick,
                        theme,
                        color_mode: app.effective_color_mode(),
                        ascii_only: app.is_ascii_only(),
                    },
                    Rect {
                        x: eq_x,
                        y: area.y,
                        width: eq_w,
                        height: sep_height,
                    },
                );
            }
        }
        render_text_area(
            app,
            frame,
            Rect {
                y: area.y.saturating_add(sep_height),
                height: area.height.saturating_sub(sep_height),
                ..area
            },
            busy,
        );
    } else {
        let sep_spans: Vec<Span<'_>> = if busy {
            match motion {
                Motion::Minimal => {
                    build_spinner_busy_sep(spinner_idx, activity_label, app, theme, true)
                }
                _ => build_spinner_busy_sep(spinner_idx, activity_label, app, theme, false),
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
        // In Full mode the wave row shows prompt glyph + wave animation only.
        // The busy verb is in the status bar (§6), not duplicated here.
        assert!(
            output.contains("you ▸"),
            "prompt glyph must appear in Full+busy; got: {output:?}"
        );
        // "esc to interrupt" must NOT appear in Full mode — only in Minimal/Off.
        assert!(
            !output.contains("esc to interrupt"),
            "Full mode must not show interrupt hint in wave row; got: {output:?}"
        );
        assert!(
            !output.is_empty(),
            "render must not produce empty output; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_full_narrow_wave_appears() {
        // width=20: wave_w = 20 - 6 = 14, wave appears right after the prompt glyph.
        // No label text competes for space — the whole remaining width is wave.
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
        assert!(
            output.contains("you ▸"),
            "prompt glyph must appear on narrow terminal; got: {output:?}"
        );
        // Text area is blank when busy (no placeholder).
        assert!(
            !output.contains("Type a message"),
            "placeholder must be hidden when busy; got: {output:?}"
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
        // No lit dot-matrix bullet — Off mode uses the spinner/label path only.
        // Note: `·` (middle-dot) is the static spinner symbol and may appear legitimately.
        assert!(
            !output.contains('•'),
            "no dot-matrix bullet should appear under Motion::Off; got: {output:?}"
        );
    }
}
