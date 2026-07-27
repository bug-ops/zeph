// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;
use zeph_config::Motion;

use crate::app::{App, InputMode};
use crate::layout::truncate_to_width;
use crate::theme::Theme;
use crate::widgets::spinner::breeze_frame;
use crate::widgets::status_verbs::humanize;
use zeph_common::text::format_tokens;

/// Prompt glyph shown at the beginning of the separator line.
const PROMPT_GLYPH: &str = "you ▸";

/// Upper bound on the humanized verb's display width, so one abnormally long or
/// unrecognized status label (`humanize` passes unknown strings through verbatim)
/// cannot crowd out the interrupt hint.
const VERB_MAX_WIDTH: u16 = 24;

/// Below this width, a truncated verb (e.g. `"thinki…"`) is less legible than showing
/// no verb at all, so it is dropped entirely instead.
const VERB_MIN_WIDTH: u16 = 6;

/// The width always reserved for the verb (its minimum useful width, plus the leading
/// `"  "`) before the row's optional idle elements are admitted. The verb is
/// rule-mandated (Spinner Rule, spec 011-tui: "a visible spinner **with a short status
/// message**"), while the mode hint / token estimate / queue badges are convenience
/// text, so the verb must outrank them for space — otherwise, as the terminal widens,
/// an idle element can suddenly become admissible and starve the verb back down to
/// nothing, making verb visibility non-monotonic in width.
const VERB_RESERVED_WIDTH: u16 = VERB_MIN_WIDTH + 2;

/// The interrupt hint shown at the end of a busy separator row.
const INTERRUPT_HINT: &str = "  ctrl+c to interrupt";

/// Append the spinner glyph, humanized activity verb, and interrupt hint to a busy
/// separator row.
///
/// This is the canonical on-screen location for the busy spinner + activity verb (see
/// spec 011-tui); the bottom status bar no longer duplicates it. The spinner and
/// interrupt hint are mandatory (Spinner Rule, #6646) and their width is reserved by
/// the caller before `verb_budget` is computed — see [`build_spinner_busy_sep`] — so
/// only the verb is truncated (and, below [`VERB_MIN_WIDTH`], dropped) under pressure;
/// the spinner and hint themselves always render in full.
fn push_busy_tail<'a>(
    spans: &mut Vec<Span<'a>>,
    spinner_span: String,
    verb_budget: u16,
    app: &App,
    theme: &'a Theme,
) {
    spans.push(Span::styled(spinner_span, theme.highlight));

    let phrase = humanize(app.status_label().unwrap_or("thinking"));
    let verb = if phrase.detail.is_empty() {
        phrase.verb
    } else {
        format!("{} · {}", phrase.verb, phrase.detail)
    };
    if !verb.is_empty() {
        let budget = verb_budget.saturating_sub(2).min(VERB_MAX_WIDTH); // leading "  "
        if budget >= VERB_MIN_WIDTH {
            let truncated = truncate_to_width(&verb, budget as usize);
            spans.push(Span::styled(format!("  {truncated}"), theme.system_message));
        }
    }

    spans.push(Span::styled(INTERRUPT_HINT, theme.system_message));
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
/// Layout: prompt glyph + mode hint + token estimate + queued badge + spinner + verb +
/// interrupt hint. The only difference between modes is whether the spinner glyph
/// animates (`animated = true`).
///
/// The spinner, interrupt hint, and a minimum-width slice for the verb
/// ([`VERB_RESERVED_WIDTH`]) are all mandatory and reserved up front; the row's other,
/// merely convenient elements (mode hint, token estimate, queue badges) are dropped
/// first under pressure — each is only included if it still fits in what's left, so
/// neither the spinner nor the verb is ever pushed out by them. Whatever width remains
/// after that is added back on top of the verb's reserved slice (see
/// [`push_busy_tail`]), letting it grow past the floor when there's slack.
fn build_spinner_busy_sep<'a>(
    spinner_idx: u8,
    app: &'a App,
    theme: &'a Theme,
    animated: bool,
    max_width: u16,
) -> Vec<Span<'a>> {
    let symbol = if animated {
        breeze_frame(u64::from(spinner_idx), app.is_ascii_only())
    } else {
        "·"
    };
    let spinner_span = format!("  {symbol}");
    let spinner_width = u16::try_from(spinner_span.width()).unwrap_or(u16::MAX);
    let hint_width = u16::try_from(INTERRUPT_HINT.width()).unwrap_or(u16::MAX);

    let prompt_span = format!("{PROMPT_GLYPH} ");
    let prompt_width = u16::try_from(prompt_span.width()).unwrap_or(u16::MAX);

    let mut spans: Vec<Span<'_>> = vec![Span::styled(prompt_span, theme.system_message)];
    let mut budget = max_width
        .saturating_sub(prompt_width)
        .saturating_sub(spinner_width)
        .saturating_sub(hint_width)
        .saturating_sub(VERB_RESERVED_WIDTH);

    let mode = mode_hint(app);
    let mode_width = u16::try_from(mode.width()).unwrap_or(u16::MAX);
    if mode_width <= budget {
        spans.push(Span::styled(mode, theme.system_message));
        budget -= mode_width;
    }

    let estimate = app.context_token_estimate();
    if estimate > 0 {
        let meta = format!("  ~{} tokens", format_tokens(estimate as u64));
        let meta_width = u16::try_from(meta.width()).unwrap_or(u16::MAX);
        if meta_width <= budget {
            budget -= meta_width;
            spans.push(Span::styled(meta, theme.system_message));
        }
    }

    if app.queued_count() > 0 {
        let queued = format!("  [+{} queued]", app.queued_count());
        let queued_width = u16::try_from(queued.width()).unwrap_or(u16::MAX);
        if queued_width <= budget {
            budget -= queued_width;
            spans.push(Span::styled(queued, theme.highlight));
        }
    }

    if app.editing_queued() {
        const EDITING_QUEUED: &str = "  [editing queued]";
        let editing_width = u16::try_from(EDITING_QUEUED.width()).unwrap_or(u16::MAX);
        if editing_width <= budget {
            budget -= editing_width;
            spans.push(Span::styled(EDITING_QUEUED, theme.highlight));
        }
    }

    let verb_budget = budget.saturating_add(VERB_RESERVED_WIDTH);
    push_busy_tail(&mut spans, spinner_span, verb_budget, app, theme);
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
        let (cursor_x, cursor_y) = caret_xy(app, text_area, app.cursor_position());
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Computes the on-screen `(x, y)` position of `char_index` within `text_area`.
///
/// Shared by the real terminal cursor (above) and the mention-picker popup anchor
/// (`crate::widgets::mention_picker::render`) so the popup never disagrees with where
/// the cursor is actually drawn (M3). Splits the buffer on `'\n'` only — the paragraph
/// renders with `Wrap { trim: false }`, so on a visually-wrapped line the computed `x`
/// can overshoot `text_area.width`; this is a pre-existing limitation shared identically
/// by both call sites, not something this helper newly introduces.
pub(crate) fn caret_xy(app: &App, text_area: Rect, char_index: usize) -> (u16, u16) {
    let input = app.input();
    let byte_idx = input
        .char_indices()
        .nth(char_index)
        .map_or(input.len(), |(idx, _)| idx);
    let prefix = &input[..byte_idx];
    let last_line = prefix.rsplit('\n').next().unwrap_or(prefix);
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x = text_area.x + last_line.width() as u16;
    let visible_lines = text_area.height;
    let cursor_line = u16::try_from(prefix.matches('\n').count()).unwrap_or(u16::MAX);
    let scroll = cursor_line.saturating_sub(visible_lines.saturating_sub(1));
    let cursor_y = text_area.y + cursor_line.saturating_sub(scroll);
    (cursor_x, cursor_y)
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
            Motion::Off => build_spinner_busy_sep(spinner_idx, app, theme, false, area.width),
            _ => build_spinner_busy_sep(spinner_idx, app, theme, true, area.width),
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
        // Spinner + interrupt hint are mandatory (Spinner Rule) and reserved first,
        // along with a minimum-width slice for the verb; the mode hint (a merely
        // convenient idle element) is the first thing dropped under pressure, per
        // spec 011-tui ("truncated before the hint under width pressure") — never the
        // spinner or hint.
        assert!(
            output.contains("you ▸"),
            "prompt glyph must appear when busy; got: {output:?}"
        );
        assert!(
            output.contains("ctrl+c to interrupt"),
            "interrupt hint must always be fully visible, even under width pressure; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_width_45_shows_verb_without_mode_hint() {
        // M6/M8 regression: the verb has a reserved minimum-width slice that outranks
        // the mode hint, so verb visibility degrades monotonically with width instead
        // of vanishing in a middle band (e.g. 51-58 cols) only to reappear once the
        // terminal is wide enough to admit the mode hint too. At width 45 the mode hint
        // ("esc for normal mode", 19 cols) does not fit, but the verb still must.
        let app = make_app();
        let output = render_to_string(45, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Minimal);
        });
        assert!(
            !output.contains("esc for normal mode"),
            "mode hint must not fit at width 45 (test premise); got: {output:?}"
        );
        assert!(
            output.contains("thinking"),
            "verb must still render even though the mode hint was dropped; got: {output:?}"
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
    fn input_busy_narrow_with_queue_badge_keeps_spinner_and_hint_visible() {
        // Spinner Rule (NON-NEGOTIABLE): the spinner and interrupt hint must always
        // render, even when the queue badge and narrow width would otherwise crowd
        // them out — the queue badge (or mode hint, or verb) is what gets dropped.
        use crate::widgets::spinner::BREEZE_FRAMES;

        let mut app = make_app();
        app.handle_event(crate::event::AppEvent::Agent(
            crate::event::AgentEvent::QueueCount(2),
        ));
        let output = render_to_string(40, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Minimal);
        });
        assert!(
            BREEZE_FRAMES.iter().any(|f| output.contains(f)),
            "spinner must remain visible despite an active queue badge on a narrow \
             terminal; got: {output:?}"
        );
        assert!(
            output.contains("ctrl+c to interrupt"),
            "interrupt hint must remain visible too; got: {output:?}"
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
        // Assert on the spinner's own span ("  ·", two leading spaces) rather than a
        // bare '·' — `humanize` joins verb + detail with " · ", so a bare '·' check
        // would pass by coincidence of the verb text, not because the spinner rendered.
        assert!(
            output.contains("  ·"),
            "the static dot spinner must still render under Motion::Off; got: {output:?}"
        );
        assert!(
            output.contains("thinking"),
            "the activity verb must still render under Motion::Off; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_humanizes_label_with_detail() {
        let mut app = make_app();
        app.sessions.current_mut().status_label = Some("Loading skills...".to_owned());
        let output = render_to_string(80, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Minimal);
        });
        assert!(
            output.contains("loading · skills"),
            "raw label must be humanized to 'loading · skills'; got: {output:?}"
        );
        assert!(
            !output.contains("Loading skills"),
            "raw internal label must not leak into the rendered separator; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_unrecognized_label_passes_through() {
        let mut app = make_app();
        app.sessions.current_mut().status_label = Some("Some unknown operation...".to_owned());
        let output = render_to_string(80, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Minimal);
        });
        assert!(
            output.contains("Some unknown operation"),
            "humanize() fallback must pass through unrecognized labels verbatim; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_long_verb_is_capped_and_hint_still_shown() {
        let mut app = make_app();
        app.sessions.current_mut().status_label =
            Some("This is a very long unrecognized status message".to_owned());
        let output = render_to_string(200, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Minimal);
        });
        assert!(
            output.contains("ctrl+c to interrupt"),
            "interrupt hint must still appear even with a very long verb; got: {output:?}"
        );
        assert!(
            !output.contains("This is a very long unrecognized status message"),
            "an abnormally long verb must be capped, not rendered in full; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_uses_breeze_spinner_not_braille() {
        use crate::widgets::spinner::BREEZE_FRAMES;

        let app = make_app();
        let output = render_to_string(80, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Minimal);
        });
        let contains_braille = output
            .chars()
            .any(|c| ('\u{2800}'..='\u{28FF}').contains(&c));
        assert!(
            !contains_braille,
            "braille spinner must not appear; got: {output:?}"
        );
        assert!(
            BREEZE_FRAMES.iter().any(|f| output.contains(f)),
            "expected a breeze_frame glyph in the busy separator; got: {output:?}"
        );
    }

    #[test]
    fn input_busy_ascii_fallback_uses_ascii_breeze_frames() {
        use crate::widgets::spinner::{BREEZE_ASCII, BREEZE_FRAMES};

        let mut app = make_app();
        app.unicode_capable = false;
        assert!(app.is_ascii_only());

        let output = render_to_string(80, 5, |frame, area| {
            render_input(&app, frame, area, true, 0, zeph_config::Motion::Minimal);
        });
        assert!(
            BREEZE_ASCII.iter().any(|f| output.contains(f)),
            "expected an ASCII breeze_frame glyph when unicode is unavailable; got: {output:?}"
        );
        assert!(
            !BREEZE_FRAMES.iter().any(|f| output.contains(f)),
            "Unicode breeze glyphs must not appear in ASCII fallback mode; got: {output:?}"
        );
    }
}
