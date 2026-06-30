// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Deterministic wave animation and equalizer widget for the TUI dashboard (#5096).
//!
//! All renderers are pure functions of `(state, width, t)` where `t` is a monotonic
//! `u64` tick counter owned by `App`. No wall-clock reads happen here — all
//! time-dependence flows through the explicit `t` argument so snapshot tests
//! stay bit-identical.
//!
//! # Architecture
//!
//! - [`WaveState`] — discriminates the 6 visual modes; derived in `App::wave_state()`.
//! - [`band_value`] — pure math: maps `(state, band, t)` to a normalised `[0.0, 1.0]` amplitude.
//! - [`sample`] — maps `(state, x, t)` to a glyph bucket `0..=7` (delegates to [`band_value`]).
//! - [`glyphs`] — single-row span builder used in compact-motion paths.
//! - [`EqualizerWidget`] — full ratatui [`Widget`] for the busy separator; writes `▄` blocks
//!   directly into the [`Buffer`] with a per-row teal gradient.

use std::f32::consts::TAU;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols;
use ratatui::text::Span;
use ratatui::widgets::Widget;

use crate::theme::{EffectiveColorMode, Theme};

// ---------------------------------------------------------------------------
// Glyph ramps
// ---------------------------------------------------------------------------

/// Equalizer bar ramp from silent (▁) to full (█).
///
/// Each column's bar height is determined by [`sample`] (sine math per `WaveState`).
/// Different states produce visually distinct patterns:
/// Swell → slow tall columns; Streaming → medium ripple; Tool → choppy spikes;
/// Parallel → complex superposed pattern. Color is a vertical gradient from dim
/// accent at the base to full `#1FB9A8` at the peak (see [`bucket_to_rgb`]).
const WAVE_GLYPHS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// ASCII fallback for `TERM=dumb` terminals.
const ASCII_GLYPHS: [&str; 8] = [".", ".", "-", "-", "~", "~", "=", "="];

// ---------------------------------------------------------------------------
// WaveState
// ---------------------------------------------------------------------------

/// Discriminates the wave animation mode for the input separator row.
///
/// Derived once per render frame by [`crate::app::App::wave_state`] from live
/// agent state. The renderer receives a `WaveState` value — it never inspects
/// wall-clock time directly so snapshot tests remain deterministic.
///
/// # Variants
///
/// | Variant | When shown |
/// |---------|-----------|
/// | `Idle` | Agent is not busy — flat `▁` baseline |
/// | `Swell` | Busy, awaiting first token — high amplitude, slow roll |
/// | `Streaming` | Token stream active — medium amplitude, medium ω |
/// | `Tool` | Tool execution in progress — choppy short-λ wave |
/// | `Parallel` | ≥2 background tasks inflight — superposed sines |
/// | `Stalled` | No progress for >`stall_threshold` — flat + error tint |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveState {
    /// Agent idle — renders a static thin baseline.
    Idle,
    /// Busy, waiting for the first token.
    Swell,
    /// Token stream active (fixed ω; tps modulation deferred to v2).
    ///
    /// `// TODO(#5096-tps): modulate omega by live tok/s once a per-turn rate metric exists`
    Streaming,
    /// Tool execution in progress.
    Tool,
    /// ≥2 background tasks inflight; `sines` is clamped to `2..=3`.
    Parallel {
        /// Number of superposed sine waves; clamped to `2..=3`.
        sines: u8,
    },
    /// No progress for longer than the stall threshold.
    Stalled,
}

// ---------------------------------------------------------------------------
// Wave parameters
// ---------------------------------------------------------------------------

/// Per-state equalizer parameters passed to [`sample`].
///
/// Tuned for the 250 ms tick rate (4 fps). `omega` is the phase advance per tick
/// in radians; one full vertical cycle takes `2π / omega` ticks.
#[derive(Debug, Clone, Copy)]
struct WaveParams {
    /// Peak displacement in `[0, 1]`.
    amplitude: f32,
    /// Phase advance per tick (radians). Higher → faster vertical oscillation.
    omega: f32,
    /// Whether to superpose a secondary component for erratic spikes (Tool state).
    choppy: bool,
    /// Number of superposed sines (only for `Parallel`).
    sines: u8,
}

impl WaveState {
    /// Return the render parameters for this state.
    fn params(self) -> WaveParams {
        match self {
            // Idle and Stalled both render a flat baseline (amplitude=0).
            WaveState::Idle | WaveState::Stalled => WaveParams {
                amplitude: 0.0,
                omega: 0.0,
                choppy: false,
                sines: 1,
            },
            // Slow breathing: bars rise and fall lazily.
            WaveState::Swell => WaveParams {
                amplitude: 0.9,
                omega: 0.35,
                choppy: false,
                sines: 1,
            },
            // Medium pace: energetic activity during token streaming.
            WaveState::Streaming => WaveParams {
                amplitude: 0.85,
                omega: 1.1,
                choppy: false,
                sines: 1,
            },
            // Fast erratic: short spikes during tool execution.
            WaveState::Tool => WaveParams {
                amplitude: 0.7,
                omega: 2.3,
                choppy: true,
                sines: 1,
            },
            // Mixed pace: superposed sines create complex pattern.
            WaveState::Parallel { sines } => WaveParams {
                amplitude: 0.7,
                omega: 0.85,
                choppy: false,
                sines: sines.clamp(2, 3),
            },
        }
    }
}

/// Number of terminal columns per equalizer bar (band width).
///
/// Columns within one band share the same phase so they act as a single
/// bar — visually distinct bars oscillate independently.
const BAND_W: u32 = 3;

// ---------------------------------------------------------------------------
// Core math
// ---------------------------------------------------------------------------

/// Return the normalised amplitude `[0.0, 1.0]` for equalizer band `band_idx` at tick `t`.
///
/// This function is the shared oscillation core used by both [`sample`] and [`EqualizerWidget`].
/// Given identical `(state, band_idx, t)` it always returns the same value.
///
/// # Band-oscillation model
///
/// Each band receives a unique phase offset via the golden ratio (`0.618034`), so
/// adjacent bands oscillate independently — producing the classic audio equalizer
/// aesthetic where every bar moves up and down on its own schedule.
///
/// A squaring step (`y_norm²`) concentrates energy near the trough: bars spend
/// most time low and spike briefly to full height ("резко поднимаются").
///
/// # `u64 → f32` note
///
/// f32 mantissa is 24 bits; the cast is exact below t ≈ 16.7 M (≈48 days at
/// 4 fps). Beyond that the phase drifts slowly — visually imperceptible.
#[must_use]
pub fn band_value(state: WaveState, band_idx: u32, t: u64) -> f32 {
    let p = state.params();

    if p.amplitude < f32::EPSILON {
        return 0.0; // Idle / Stalled → flat baseline
    }

    #[allow(clippy::cast_precision_loss)]
    let tf = (t % 65536) as f32; // harmless wrap after ≈4.5 h
    #[allow(clippy::cast_precision_loss)]
    let bar_phase = (band_idx as f32 * 0.618_034).fract() * TAU;

    let y = if p.sines <= 1 {
        let mut v = p.amplitude * (p.omega * tf + bar_phase).sin();
        if p.choppy {
            // Secondary component (tribonacci ratio) for erratic Tool spikes.
            #[allow(clippy::cast_precision_loss)]
            let bar_phase2 = (band_idx as f32 * 1.324_718).fract() * TAU;
            v = (v + 0.4 * p.amplitude * (p.omega * 1.7 * tf + bar_phase2).sin())
                .clamp(-p.amplitude, p.amplitude);
        }
        v
    } else {
        // Parallel: superpose sines at golden-ratio omega multiples.
        let omegas: [f32; 3] = [1.0, 1.618_034, 2.414_214];
        let count = p.sines as usize;
        let mut sum = 0.0_f32;
        for (i, &om) in omegas[..count].iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let phase = (band_idx as f32 * (0.618_034 + i as f32 * 0.381_966)).fract() * TAU;
            sum += (p.omega * om * tf + phase).sin();
        }
        #[allow(clippy::cast_precision_loss)]
        {
            p.amplitude * (sum / count as f32).clamp(-1.0, 1.0)
        }
    };

    // Normalise to [0, 1] then square: bars mostly low, spiking sharply to peak.
    let y_norm = (y.clamp(-p.amplitude, p.amplitude) / p.amplitude + 1.0) / 2.0;
    y_norm.powi(2)
}

/// Return the glyph bucket index `0..=7` for column `x` at tick `t`.
///
/// Delegates to [`band_value`] — `x` is mapped to a band via `x / BAND_W`.
#[must_use]
pub fn sample(state: WaveState, x: u32, t: u64) -> usize {
    let band = x / BAND_W;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bucket = (band_value(state, band, t) * 7.0).round() as usize;
    bucket.clamp(0, 7)
}

// ---------------------------------------------------------------------------
// Glyph row builder
// ---------------------------------------------------------------------------

/// Render a wave row of `width` columns into `buf`, then return the spans.
///
/// The caller passes a reused `buf: &mut Vec<Span<'static>>` that is cleared
/// (not freed) each frame so capacity is amortized to zero after the first render.
///
/// Returns an empty vec when `width == 0` — never panics on narrow terminals.
///
/// # Parameters
///
/// - `state` — wave mode.
/// - `width` — number of terminal columns available for the wave.
/// - `t` — monotonic tick counter from `App::wave_tick()`.
/// - `color_mode` — resolved terminal colour capability.
/// - `ascii_only` — when `true`, uses ASCII glyph ramp regardless of state.
/// - `buf` — reused span buffer; cleared on entry.
/// - `theme` — used for colour styling.
#[allow(clippy::too_many_arguments)]
pub fn glyphs<'a>(
    state: WaveState,
    width: u32,
    t: u64,
    color_mode: EffectiveColorMode,
    ascii_only: bool,
    buf: &'a mut Vec<Span<'static>>,
    theme: &Theme,
) -> &'a [Span<'static>] {
    buf.clear();
    if width == 0 {
        return buf.as_slice();
    }

    let ramp: &[&'static str; 8] = if ascii_only {
        &ASCII_GLYPHS
    } else {
        &WAVE_GLYPHS
    };

    match color_mode {
        EffectiveColorMode::Truecolor => {
            // Per-column gradient: colour derived from bucket height.
            for x in 0..width {
                let b = sample(state, x, t);
                let glyph = ramp[b];
                let color = bucket_to_rgb(state, b);
                buf.push(Span::styled(glyph, Style::default().fg(color)));
            }
        }
        EffectiveColorMode::Ansi256 | EffectiveColorMode::Ansi16 => {
            // Flat accent colour — single span for the whole row (no per-cell alloc).
            let style = if matches!(state, WaveState::Stalled) {
                theme.error
            } else {
                theme.highlight
            };
            let mut row = String::with_capacity(width as usize * 3); // 3 bytes per block glyph
            for x in 0..width {
                let b = sample(state, x, t);
                row.push_str(ramp[b]);
            }
            // Owned String satisfies the 'static bound — dropped with the Span next frame.
            buf.push(Span::styled(std::borrow::Cow::Owned(row), style));
        }
        EffectiveColorMode::Never => {
            // Modifiers only — no colour. Single span.
            let mut row = String::with_capacity(width as usize * 3);
            for x in 0..width {
                let b = sample(state, x, t);
                row.push_str(ramp[b]);
            }
            buf.push(Span::raw(std::borrow::Cow::Owned(row)));
        }
    }

    buf.as_slice()
}

/// VU-meter equalizer widget rendered in the dashboard side panel during active inference.
///
/// Renders animated frequency bands directly into a ratatui [`Buffer`] using `▄`
/// (U+2584, lower half block) characters with a teal gradient that runs from near-black
/// at the bottom row to the full Zeph accent colour (`#1FB9A8`) at the top.
///
/// Band phases are distributed via the golden ratio so adjacent bars oscillate
/// independently, producing the classic audio equalizer aesthetic where each bar
/// moves up and down on its own schedule. The number of lit rows per band is
/// proportional to `area.height`, so the widget scales naturally to any allocated rect.
///
/// Inspired by the [`tui-equalizer`](https://github.com/ratatui/tui-widgets/tree/main/tui-equalizer)
/// reference widget, adapted for the Zeph teal design language.
///
/// # Examples
///
/// ```no_run
/// use ratatui::layout::Rect;
/// use zeph_tui::widgets::wave::{EqualizerWidget, WaveState};
/// use zeph_tui::theme::{EffectiveColorMode, Theme};
///
/// let widget = EqualizerWidget {
///     state: WaveState::Streaming,
///     tick: 42,
///     theme: &Theme::default(),
///     color_mode: EffectiveColorMode::Truecolor,
///     ascii_only: false,
/// };
/// // frame.render_widget(widget, area);
/// ```
pub struct EqualizerWidget<'a> {
    /// Current wave animation state.
    pub state: WaveState,
    /// Monotonic tick counter from `App::wave_tick()`.
    pub tick: u64,
    /// Theme reference for ANSI colour fallback.
    pub theme: &'a Theme,
    /// Resolved terminal colour capability.
    pub color_mode: EffectiveColorMode,
    /// When `true`, uses ASCII-safe `|` instead of `▄`.
    pub ascii_only: bool,
}

impl Widget for EqualizerWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let n_bands = area.width / BAND_W as u16;
        if n_bands == 0 {
            return;
        }
        let effective_w = n_bands * BAND_W as u16;
        let eq_area = Rect {
            width: effective_w,
            ..area
        };

        let band_areas =
            Layout::horizontal(vec![Constraint::Length(BAND_W as u16); n_bands as usize])
                .split(eq_area);

        for (idx, &band_area) in band_areas.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let value = band_value(self.state, idx as u32, self.tick);
            render_eq_band(
                band_area,
                value,
                self.state,
                self.color_mode,
                self.theme,
                self.ascii_only,
                buf,
            );
        }
    }
}

/// Render one equalizer band into the buffer.
///
/// Fills from the bottom up: lit rows receive `▄` in the gradient colour;
/// unlit rows are left untouched (transparent background).
fn render_eq_band(
    area: Rect,
    value: f32,
    state: WaveState,
    color_mode: EffectiveColorMode,
    theme: &Theme,
    ascii_only: bool,
    buf: &mut Buffer,
) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lit_rows = (value.clamp(0.0, 1.0) * area.height as f32) as u16;
    let symbol = if ascii_only { "|" } else { symbols::bar::HALF };

    for row in 0..lit_rows {
        let y = area.bottom().saturating_sub(row + 1);
        let color = eq_row_color(row, area.height, state, color_mode, theme);
        for x in area.left()..area.right() {
            buf[(x, y)].set_fg(color).set_symbol(symbol);
        }
    }
}

/// Compute the foreground colour for a single lit row of an equalizer band.
///
/// `row_from_bottom = 0` is the lowest (darkest) lit row; increasing values move
/// toward the top (brightest). In Truecolor mode a smooth teal gradient is applied;
/// ANSI modes fall back to the theme highlight or error colour.
fn eq_row_color(
    row_from_bottom: u16,
    total_height: u16,
    state: WaveState,
    color_mode: EffectiveColorMode,
    theme: &Theme,
) -> Color {
    match color_mode {
        EffectiveColorMode::Truecolor => {
            let v = if total_height <= 1 {
                1.0_f32
            } else {
                row_from_bottom as f32 / (total_height - 1) as f32
            };
            if matches!(state, WaveState::Stalled) {
                // Error tint: dark red at bottom → bright red at top.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                return Color::Rgb((80.0 + v * 175.0) as u8, 10, 10);
            }
            // Teal gradient: #0A191E (bottom) → #1FB9A8 (top / accent).
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Color::Rgb(
                (10.0 + v * 21.0) as u8,  // 10 → 31
                (25.0 + v * 160.0) as u8, // 25 → 185
                (30.0 + v * 138.0) as u8, // 30 → 168
            )
        }
        EffectiveColorMode::Ansi256 | EffectiveColorMode::Ansi16 => {
            if matches!(state, WaveState::Stalled) {
                theme.error.fg.unwrap_or(Color::Red)
            } else {
                theme.highlight.fg.unwrap_or(Color::Yellow)
            }
        }
        EffectiveColorMode::Never => Color::Reset,
    }
}

/// Map a bucket index `0..=7` to an RGB colour for the Truecolor thin-line wave.
///
/// Trough (bucket 0) → near-invisible on dark bg. Crest (bucket 7) → full
/// accent `#1FB9A8`, matching the CSS gradient in the design mock.
/// Quadratic curve keeps low buckets dark so the peak stands out.
fn bucket_to_rgb(state: WaveState, bucket: usize) -> Color {
    if matches!(state, WaveState::Stalled) {
        // Error tint: low-to-mid red gradient along the flat line.
        #[allow(clippy::cast_possible_truncation)]
        let v = (80 + bucket * 22) as u8;
        return Color::Rgb(v, 15, 15);
    }
    // Quadratic fade: 0 → dark (#0A191E), 7 → accent (#1FB9A8).
    let t = (bucket as f32 / 7.0).powi(2);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let r = (10.0_f32 + t * 21.0) as u8; // 10..=31
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let g = (25.0_f32 + t * 160.0) as u8; // 25..=185
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let b = (30.0_f32 + t * 138.0) as u8; // 30..=168
    Color::Rgb(r, g, b)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// All bucket values must stay in 0..=7 for any input combination.
    #[test]
    fn sample_bucket_always_in_range() {
        for t in [0u64, 1, 63, 127, 255, 65535, 65536, 1_000_000] {
            for x in [0u32, 1, 5, 10, 40, 80, 160] {
                for state in [
                    WaveState::Idle,
                    WaveState::Swell,
                    WaveState::Streaming,
                    WaveState::Tool,
                    WaveState::Parallel { sines: 2 },
                    WaveState::Parallel { sines: 3 },
                    WaveState::Stalled,
                ] {
                    let b = sample(state, x, t);
                    assert!(b <= 7, "bucket {b} out of range for {state:?} x={x} t={t}");
                }
            }
        }
    }

    /// Idle and Stalled always return bucket 0 (flat baseline).
    #[test]
    fn idle_and_stalled_are_flat() {
        for t in [0u64, 100, 999] {
            for x in 0u32..40 {
                assert_eq!(sample(WaveState::Idle, x, t), 0, "Idle must be flat");
                assert_eq!(sample(WaveState::Stalled, x, t), 0, "Stalled must be flat");
            }
        }
    }

    /// Determinism: identical (state, x, t) → identical output.
    #[test]
    fn sample_is_deterministic() {
        let states = [
            WaveState::Swell,
            WaveState::Streaming,
            WaveState::Tool,
            WaveState::Parallel { sines: 2 },
        ];
        for state in states {
            for x in [0u32, 7, 13, 40] {
                for t in [0u64, 42, 1024] {
                    let a = sample(state, x, t);
                    let b = sample(state, x, t);
                    assert_eq!(
                        a, b,
                        "sample must be deterministic for {state:?} x={x} t={t}"
                    );
                }
            }
        }
    }

    /// `glyphs(width=0)` is a hard no-op — never panics, returns empty slice.
    #[test]
    fn glyphs_width_zero_returns_empty() {
        let theme = Theme::default();
        let mut buf = Vec::new();
        let spans = glyphs(
            WaveState::Streaming,
            0,
            42,
            EffectiveColorMode::Truecolor,
            false,
            &mut buf,
            &theme,
        );
        assert!(spans.is_empty(), "width=0 must return empty spans");
    }

    /// Buffer reuse: calling `glyphs` twice reuses the allocation (capacity ≥ prev len).
    #[test]
    fn glyphs_buffer_reuse() {
        let theme = Theme::default();
        let mut buf: Vec<Span<'static>> = Vec::new();
        glyphs(
            WaveState::Streaming,
            40,
            0,
            EffectiveColorMode::Truecolor,
            false,
            &mut buf,
            &theme,
        );
        let cap_after_first = buf.capacity();
        assert!(
            cap_after_first >= 40,
            "buffer should have capacity for 40 spans"
        );
        glyphs(
            WaveState::Streaming,
            40,
            1,
            EffectiveColorMode::Truecolor,
            false,
            &mut buf,
            &theme,
        );
        assert_eq!(
            buf.capacity(),
            cap_after_first,
            "second call must not reallocate"
        );
    }

    /// Truecolor output has one span per column (gradient).
    #[test]
    fn glyphs_truecolor_one_span_per_column() {
        let theme = Theme::default();
        let mut buf = Vec::new();
        let spans = glyphs(
            WaveState::Streaming,
            20,
            5,
            EffectiveColorMode::Truecolor,
            false,
            &mut buf,
            &theme,
        );
        assert_eq!(
            spans.len(),
            20,
            "Truecolor must produce one span per column"
        );
    }

    /// Ansi256 output is a single span for the whole row.
    #[test]
    fn glyphs_ansi256_single_span() {
        let theme = Theme::default();
        let mut buf = Vec::new();
        let spans = glyphs(
            WaveState::Streaming,
            20,
            5,
            EffectiveColorMode::Ansi256,
            false,
            &mut buf,
            &theme,
        );
        assert_eq!(spans.len(), 1, "Ansi256 must produce a single flat span");
    }

    /// motion=Off: holding state and motion fixed, varying t produces identical output.
    /// (The actual Off gate is in `input::render`, but we verify the pure layer here
    /// by checking that Idle is always byte-identical regardless of t.)
    #[test]
    fn idle_output_invariant_across_ticks() {
        let theme = Theme::default();
        let mut buf_a = Vec::new();
        let mut buf_b = Vec::new();
        let spans_a = glyphs(
            WaveState::Idle,
            40,
            0,
            EffectiveColorMode::Truecolor,
            false,
            &mut buf_a,
            &theme,
        );
        let spans_b = glyphs(
            WaveState::Idle,
            40,
            999,
            EffectiveColorMode::Truecolor,
            false,
            &mut buf_b,
            &theme,
        );
        let text_a: String = spans_a.iter().map(|s| s.content.as_ref()).collect();
        let text_b: String = spans_b.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text_a, text_b, "Idle output must be tick-invariant");
    }

    /// ASCII fallback uses the ASCII ramp, not block glyphs.
    #[test]
    fn ascii_fallback_uses_ascii_ramp() {
        let theme = Theme::default();
        let mut buf = Vec::new();
        let spans = glyphs(
            WaveState::Streaming,
            20,
            5,
            EffectiveColorMode::Truecolor,
            true, // ascii_only
            &mut buf,
            &theme,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        // ASCII ramp characters — no block glyphs should appear.
        assert!(
            !text.contains('▁') && !text.contains('█'),
            "ASCII mode must not contain block glyphs: {text:?}"
        );
    }

    /// Stalled state has error tint (Rgb with high R) in Truecolor.
    #[test]
    fn stalled_uses_error_tint_in_truecolor() {
        let theme = Theme::default();
        let mut buf = Vec::new();
        let spans = glyphs(
            WaveState::Stalled,
            10,
            0,
            EffectiveColorMode::Truecolor,
            false,
            &mut buf,
            &theme,
        );
        for span in spans {
            if let Some(Color::Rgb(r, _g, _b)) = span.style.fg {
                assert!(
                    r >= 80,
                    "Stalled must have elevated R channel for error tint, got r={r}"
                );
            }
        }
    }
}
