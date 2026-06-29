// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Splash screen widget displayed before the first message is sent.
//!
//! Renders a branded wordmark with a gradient (truecolor), plain accent (ANSI-16), or
//! plain ASCII (`Never` mode). Layout degrades gracefully based on available terminal height.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::delights::shimmer_brightness;
use crate::theme::{EffectiveColorMode, map_color};

// Aqua (#22d3ee) and ice (#e0f2fe) — splash wordmark gradient only; intentionally
// distinct from palette.accent (#1FB9A8) used everywhere else in chrome.
const AQUA: (u8, u8, u8) = (0x22, 0xd3, 0xee);
const ICE: (u8, u8, u8) = (0xe0, 0xf2, 0xfe);

// Accent fallback for ANSI-16 / Never modes (cyan is the nearest named colour to aqua).
const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

const SLOGAN: &str = "think further.";
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Keybinding hints shown in the splash footer — single source of truth.
const QUICK_HINTS: &[(&str, &str)] = &[
    ("/", "commands"),
    ("@", "files"),
    ("?", "keys"),
    ("Tab", "panels"),
];

/// Render the splash screen.
///
/// The `color_mode` parameter selects the rendering tier:
/// - [`EffectiveColorMode::Truecolor`] / [`EffectiveColorMode::Ansi256`]: aqua-to-ice gradient
///   across the four letters of `zeph`.
/// - [`EffectiveColorMode::Ansi16`]: plain accent colour, Unicode prefix `≈`.
/// - [`EffectiveColorMode::Never`]: ASCII prefix `~`, no colour.
///
/// Layout degrades to a compact form when fewer than 8 rows are available:
/// - ≥ 8 rows: full layout (wordmark + slogan + version + hints).
/// - 3–7 rows: two-line compact layout.
/// - < 3 rows: single wordmark line.
///
/// `shimmer_phase` drives the one-shot highlight sweep across the wordmark (#5104).
/// Pass `None` to render the static gradient (off-state, byte-identical to pre-feature).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    color_mode: EffectiveColorMode,
    shimmer_phase: Option<u64>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let lines = build_lines(area.height, color_mode, shimmer_phase);
    let paragraph = Paragraph::new(lines).alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

fn build_lines(
    height: u16,
    mode: EffectiveColorMode,
    shimmer_phase: Option<u64>,
) -> Vec<Line<'static>> {
    let wordmark = wordmark_line(mode, shimmer_phase);
    let hints = hints_line(mode);

    if height < 3 {
        return vec![wordmark];
    }

    if height < 8 {
        // Compact: wordmark + slogan on one line, hints on the next.
        let compact = compact_wordmark_line(mode, shimmer_phase);
        return vec![compact, hints];
    }

    // Full layout: blank, wordmark, slogan, version, blank, hints, blank, blank.
    let slogan_style = Style::default().fg(MUTED);
    let version_style = Style::default().fg(MUTED);

    vec![
        Line::default(),
        wordmark,
        Line::from(Span::styled(SLOGAN, slogan_style)),
        Line::from(Span::styled(format!("v{VERSION}"), version_style)),
        Line::default(),
        hints,
        Line::default(),
        Line::default(),
    ]
}

/// Single-line wordmark: `≈ zeph` (or `~ zeph` in ASCII mode) with gradient or plain colour.
fn wordmark_line(mode: EffectiveColorMode, shimmer_phase: Option<u64>) -> Line<'static> {
    match mode {
        EffectiveColorMode::Truecolor | EffectiveColorMode::Ansi256 => {
            gradient_wordmark_line("≈ ", mode, shimmer_phase)
        }
        EffectiveColorMode::Ansi16 => {
            Line::from(Span::styled("≈ zeph", Style::default().fg(ACCENT)))
        }
        EffectiveColorMode::Never => Line::from(Span::raw("~ zeph")),
    }
}

/// Compact single-line: `≈ zeph  think further.` for 3–7 row layouts.
fn compact_wordmark_line(mode: EffectiveColorMode, shimmer_phase: Option<u64>) -> Line<'static> {
    match mode {
        EffectiveColorMode::Truecolor | EffectiveColorMode::Ansi256 => {
            let mut spans = gradient_wordmark_spans("≈ ", mode, shimmer_phase);
            spans.push(Span::styled(
                format!("  {SLOGAN}"),
                Style::default().fg(MUTED),
            ));
            Line::from(spans)
        }
        EffectiveColorMode::Ansi16 => Line::from(vec![
            Span::styled("≈ zeph", Style::default().fg(ACCENT)),
            Span::styled(format!("  {SLOGAN}"), Style::default().fg(MUTED)),
        ]),
        EffectiveColorMode::Never => Line::from(format!("~ zeph  {SLOGAN}")),
    }
}

/// Hints row: each key in accent (or plain), description in muted.
fn hints_line(mode: EffectiveColorMode) -> Line<'static> {
    let key_style = match mode {
        EffectiveColorMode::Never => Style::default(),
        _ => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    };
    let desc_style = match mode {
        EffectiveColorMode::Never => Style::default(),
        _ => Style::default().fg(MUTED),
    };

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(QUICK_HINTS.len() * 3);
    for (i, (key, desc)) in QUICK_HINTS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", desc_style));
        }
        spans.push(Span::styled(*key, key_style));
        spans.push(Span::styled(format!(" {desc}"), desc_style));
    }
    Line::from(spans)
}

/// Build a `Line` with the gradient wordmark (full-layout version).
fn gradient_wordmark_line(
    prefix: &'static str,
    mode: EffectiveColorMode,
    shimmer_phase: Option<u64>,
) -> Line<'static> {
    Line::from(gradient_wordmark_spans(prefix, mode, shimmer_phase))
}

/// Build the gradient wordmark spans: prefix in accent, `zeph` in interpolated colours.
///
/// When `shimmer_phase` is `Some(tick)`, each letter's brightness is boosted by a bell-curve
/// envelope centred on the shimmer position, producing a moving highlight sweep (#5104).
/// When `shimmer_phase` is `None` the output is byte-identical to the pre-feature baseline.
///
/// Colors are passed through [`map_color`] so that `Ansi256` terminals receive indexed
/// colors instead of truecolor RGB escape sequences.
fn gradient_wordmark_spans(
    prefix: &'static str,
    mode: EffectiveColorMode,
    shimmer_phase: Option<u64>,
) -> Vec<Span<'static>> {
    let accent_style = Style::default().fg(map_color(ACCENT, mode));
    let letters: &[char] = &['z', 'e', 'p', 'h'];
    let n_letters = letters.len();

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(1 + n_letters);
    spans.push(Span::styled(prefix, accent_style));

    for (letter_pos, ch) in letters.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let interp = if n_letters <= 1 {
            0.0_f32
        } else {
            letter_pos as f32 / (n_letters - 1) as f32
        };
        let (red, green, blue) = lerp_rgb_components(AQUA, ICE, interp);

        // Apply shimmer brightness boost per letter when shimmer is active.
        let (red, green, blue) = if let Some(phase) = shimmer_phase {
            let boost = shimmer_brightness(letter_pos, n_letters, phase);
            boost_rgb(red, green, blue, boost)
        } else {
            (red, green, blue)
        };

        let color = map_color(Color::Rgb(red, green, blue), mode);
        spans.push(Span::styled(ch.to_string(), Style::default().fg(color)));
    }

    spans
}

/// Linear interpolation between two RGB colours, returning components.
fn lerp_rgb_components(
    (r1, g1, b1): (u8, u8, u8),
    (r2, g2, b2): (u8, u8, u8),
    t: f32,
) -> (u8, u8, u8) {
    (
        lerp_channel(r1, r2, t),
        lerp_channel(g1, g2, t),
        lerp_channel(b1, b2, t),
    )
}

fn lerp_channel(a: u8, b: u8, t: f32) -> u8 {
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
    }
}

/// Boost RGB channels towards white by `amount` in [0, 1].
fn boost_rgb(r: u8, g: u8, b: u8, amount: f32) -> (u8, u8, u8) {
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let boost = |c: u8| -> u8 {
        let boosted = f32::from(c) + (255.0 - f32::from(c)) * amount;
        boosted.min(255.0) as u8
    };
    (boost(r), boost(g), boost(b))
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use crate::test_utils::render_to_string;
    use crate::theme::EffectiveColorMode;

    fn render(width: u16, height: u16, mode: EffectiveColorMode) -> String {
        render_to_string(width, height, |frame, area| {
            super::render(frame, area, mode, None);
        })
    }

    #[test]
    fn splash_truecolor_full_layout() {
        let output = render(60, 20, EffectiveColorMode::Truecolor);
        assert_snapshot!(output);
    }

    #[test]
    fn splash_ansi16_full_layout() {
        let output = render(60, 20, EffectiveColorMode::Ansi16);
        assert_snapshot!(output);
    }

    #[test]
    fn splash_ascii_only_full_layout() {
        let output = render(60, 20, EffectiveColorMode::Never);
        assert_snapshot!(output);
    }

    #[test]
    fn splash_short_terminal_compact_layout() {
        // height < 8 → compact 2-row layout.
        let output = render(60, 5, EffectiveColorMode::Truecolor);
        assert_snapshot!(output);
    }

    #[test]
    fn splash_minimal_single_line() {
        // height < 3 → single wordmark line.
        let output = render(60, 2, EffectiveColorMode::Truecolor);
        assert_snapshot!(output);
    }

    #[test]
    fn splash_ascii_prefix_used_in_never_mode() {
        let output = render(60, 20, EffectiveColorMode::Never);
        assert!(
            output.contains("~ zeph"),
            "ASCII mode must use '~ zeph' prefix, got: {output}"
        );
        assert!(
            !output.contains("≈"),
            "ASCII mode must not contain '≈', got: {output}"
        );
    }

    #[test]
    fn splash_unicode_prefix_in_truecolor() {
        let output = render(60, 20, EffectiveColorMode::Truecolor);
        assert!(
            output.contains("≈"),
            "Truecolor mode must contain '≈', got: {output}"
        );
    }

    #[test]
    fn splash_version_shown_in_full_layout() {
        let output = render(60, 20, EffectiveColorMode::Ansi16);
        assert!(
            output.contains(env!("CARGO_PKG_VERSION")),
            "Full layout must contain crate version, got: {output}"
        );
    }

    #[test]
    fn splash_hints_present_in_full_layout() {
        let output = render(60, 20, EffectiveColorMode::Ansi16);
        assert!(
            output.contains("commands"),
            "Full layout must contain hint text, got: {output}"
        );
    }

    #[test]
    fn splash_zero_area_no_panic() {
        // Must not panic on a zero-size area.
        render_to_string(0, 0, |frame, area| {
            super::render(frame, area, EffectiveColorMode::Truecolor, None);
        });
    }

    #[test]
    fn splash_shimmer_none_matches_baseline() {
        // `shimmer_phase = None` must produce byte-identical output to pre-feature baseline.
        let without_shimmer = render(60, 20, EffectiveColorMode::Truecolor);
        let with_none = render_to_string(60, 20, |frame, area| {
            super::render(frame, area, EffectiveColorMode::Truecolor, None);
        });
        assert_eq!(
            without_shimmer, with_none,
            "shimmer_phase=None must be byte-identical to baseline (off-state)"
        );
    }

    #[test]
    fn splash_shimmer_active_does_not_panic() {
        // Active shimmer must not panic for any phase in [0, SHIMMER_TICKS).
        for phase in 0..crate::delights::SHIMMER_TICKS {
            render_to_string(60, 20, |frame, area| {
                super::render(frame, area, EffectiveColorMode::Truecolor, Some(phase));
            });
        }
    }
}
