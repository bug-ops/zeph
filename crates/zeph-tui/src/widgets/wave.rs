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
//! - [`EqualizerWidget`] — full ratatui [`Widget`] for the side-panel slot; draws a braille
//!   waveform (mirrored about the centre axis) that jerks in time to a sharp beat envelope.

use std::f32::consts::TAU;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
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
/// Network → complex superposed pattern. Colour is a vertical gradient (see
/// [`bucket_to_rgb`]): teal for foreground work, violet for `Network`.
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
/// | Variant | When shown | Colour |
/// |---------|-----------|--------|
/// | `Idle` | Agent is not busy — flat `▁` baseline | teal |
/// | `Swell` | Busy, awaiting first token — high amplitude, slow roll | teal |
/// | `Streaming` | Token stream active — medium amplitude, medium ω | teal |
/// | `Tool` | Tool execution in progress — choppy short-λ wave | teal |
/// | `Network` | External/background requests inflight — superposed sines | violet |
/// | `Stalled` | No progress for >`stall_threshold` — flat + error tint | red |
///
/// Foreground agent work (`Swell`/`Streaming`/`Tool`) renders in the teal accent;
/// [`WaveState::Network`] — background/external requests run by the task supervisor
/// (memory enrichment, telemetry, MCP, egress, background shell) — renders in a distinct
/// **violet** gradient so concurrent background activity is visually separable from the
/// agent's own turn.
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
    /// External/background requests inflight (task-supervisor work: enrichment,
    /// telemetry, MCP, egress, background shell). Rendered in violet to set it
    /// apart from foreground agent work. `sines` is clamped to `1..=3`.
    Network {
        /// Number of superposed sine waves; clamped to `1..=3` by concurrency.
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
    /// Number of superposed sines (only for `Network`).
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
            // Background/external requests: superposed sines create a complex pattern,
            // distinct violet colour applied in `wave_color` / `bucket_to_rgb`.
            WaveState::Network { sines } => WaveParams {
                amplitude: 0.75,
                omega: 0.95,
                choppy: false,
                sines: sines.clamp(1, 3),
            },
        }
    }
}

/// Terminal columns per equalizer band. One column = one independent bar.
const BAND_W: u32 = 1;

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
        // Network: superpose sines at golden-ratio omega multiples.
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
    let y_norm = f32::midpoint(y.clamp(-p.amplitude, p.amplitude) / p.amplitude, 1.0);
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

/// Animated braille waveform rendered in the dashboard side panel during active inference.
///
/// Instead of discrete bars, the widget draws a single continuous waveform mirrored about
/// the horizontal centre axis — like an audio waveform display. It is rendered with braille
/// characters (U+2800 range), giving 2× horizontal and 4× vertical sub-pixel resolution.
///
/// The outline is a travelling superposition of sines (so it ripples across the width),
/// multiplied by a sharp beat envelope (instant attack, cubic decay) so the whole wave
/// jerks up and down "in time to the music". A teal gradient brightens toward the wave
/// peaks (`#1FB9A8`), staying dim near the quiet centre axis.
///
/// `Idle` and `Stalled` collapse the wave to a flat centre line (`Stalled` tinted red).
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
    /// When `true`, renders the wave with ASCII density characters instead of braille.
    pub ascii_only: bool,
}

/// Braille dot bit for each `(sub_col, sub_row)`, where `sub_row = 0` is the top.
///
/// Unicode braille (`U+2800` base) dot numbering:
///
/// ```text
///   (1)(4)
///   (2)(5)
///   (3)(6)
///   (7)(8)
/// ```
const BRAILLE_DOT: [[u8; 4]; 2] = [
    [0x01, 0x02, 0x04, 0x40], // left column  → dots 1, 2, 3, 7
    [0x08, 0x10, 0x20, 0x80], // right column → dots 4, 5, 6, 8
];

impl Widget for EqualizerWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let w = usize::from(area.width);
        let sub_w = area.width * 2; // 2 braille dot columns per terminal cell
        let sub_h = area.height * 4; // 4 braille dot rows per terminal cell
        let center = f32::from(sub_h) / 2.0;
        let max_half = (center - 1.0).max(0.0);

        // Accumulate braille dot bits per terminal cell, then write once.
        let mut cells = vec![0u8; w * usize::from(area.height)];
        for sx in 0..sub_w {
            let amp = wave_profile(self.state, sx, sub_w, self.tick);
            let half = amp * max_half;
            // `center ± half` is bounded to `[0, sub_h]`, which fits u16.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let top = (center - half).round().clamp(0.0, f32::from(sub_h - 1)) as u16;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let bot = (center + half).round().clamp(0.0, f32::from(sub_h - 1)) as u16;
            let col = usize::from(sx / 2);
            let sub_col = usize::from(sx % 2);
            for sy in top..=bot {
                let row = usize::from(sy / 4);
                let sub_row = usize::from(sy % 4);
                cells[row * w + col] |= BRAILLE_DOT[sub_col][sub_row];
            }
        }

        // Brightness rises toward the wave extremes (peaks = brightest accent).
        let mid_row = (f32::from(area.height) - 1.0) / 2.0;
        let mut utf8 = [0u8; 4];
        for row in 0..area.height {
            for col in 0..area.width {
                let bits = cells[usize::from(row) * w + usize::from(col)];
                if bits == 0 {
                    continue;
                }
                let intensity = if mid_row <= 0.0 {
                    1.0
                } else {
                    ((f32::from(row) - mid_row).abs() / mid_row).clamp(0.0, 1.0)
                };
                let color = wave_color(intensity, self.state, self.color_mode, self.theme);
                let symbol = if self.ascii_only {
                    ascii_density(bits)
                } else {
                    // 0x2800..=0x28FF are all valid braille code points.
                    char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ')
                };
                buf[(area.left() + col, area.top() + row)]
                    .set_fg(color)
                    .set_symbol(symbol.encode_utf8(&mut utf8));
            }
        }
    }
}

/// Vertical half-amplitude (`0.0..=1.0` of the half-height) of the braille
/// waveform at sub-column `sx` and tick `t`.
///
/// The outline is a travelling superposition of sines (so it ripples across the
/// width), multiplied by a sharp beat envelope (instant attack, cubic decay,
/// floored so it pulses without fully dying). `Idle` / `Stalled` return `0.0`,
/// collapsing the wave to a flat centre line.
fn wave_profile(state: WaveState, sx: u16, sub_w: u16, t: u64) -> f32 {
    let p = state.params();
    if p.amplitude < f32::EPSILON {
        return 0.0;
    }

    #[allow(clippy::cast_precision_loss)]
    let tf = (t % 65536) as f32; // harmless wrap after ≈4.5 h
    #[allow(clippy::cast_precision_loss)]
    let u = if sub_w <= 1 {
        0.0
    } else {
        f32::from(sx) / f32::from(sub_w - 1)
    };

    // Travelling waveform: superposed sines drifting across the width.
    let mut s = (u * TAU * 1.5 + p.omega * tf).sin();
    let mut denom = 1.0_f32;
    s += 0.6 * (u * TAU * 3.0 - p.omega * 1.6 * tf).sin();
    denom += 0.6;
    if p.choppy {
        s += 0.4 * (u * TAU * 5.0 + p.omega * 2.3 * tf).sin();
        denom += 0.4;
    }
    if p.sines > 1 {
        s += 0.5 * (u * TAU * 2.3 + p.omega * 1.27 * tf).sin();
        denom += 0.5;
    }
    let shape = (s / denom).abs();

    // Sharp beat envelope: instant attack, cubic decay, floored at 0.3.
    let beat_phase = (p.omega * 0.18 * tf).fract();
    let energy = 0.3 + 0.7 * (1.0 - beat_phase).powi(3);

    (p.amplitude * energy * shape).clamp(0.0, 1.0)
}

/// ASCII density glyph for a braille cell, chosen by how many dots are lit.
///
/// Used when the terminal cannot render braille (`ascii_only`).
fn ascii_density(bits: u8) -> char {
    match bits.count_ones() {
        0 => ' ',
        1..=2 => '.',
        3..=4 => ':',
        5..=6 => '+',
        _ => '#',
    }
}

/// Foreground colour for a braille wave cell.
///
/// `intensity` (`0.0..=1.0`) is the cell's distance from the centre axis: peaks
/// (`1.0`) get the full accent, the quiet centre (`0.0`) stays dim. In Truecolor
/// mode a smooth gradient is applied — teal for foreground agent work, **violet**
/// for [`WaveState::Network`] (background/external requests), red for `Stalled`.
/// ANSI modes fall back to theme colours (magenta for `Network`).
fn wave_color(
    intensity: f32,
    state: WaveState,
    color_mode: EffectiveColorMode,
    theme: &Theme,
) -> Color {
    match color_mode {
        EffectiveColorMode::Truecolor => {
            let v = intensity.clamp(0.0, 1.0);
            if matches!(state, WaveState::Stalled) {
                // Error tint: dark red at centre → bright red at the peaks.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                return Color::Rgb((80.0 + v * 175.0) as u8, 10, 10);
            }
            if matches!(state, WaveState::Network { .. }) {
                // Violet gradient: #14102C (centre) → #8B5CF6 (peaks).
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                return Color::Rgb(
                    (20.0 + v * 119.0) as u8, // 20 → 139
                    (16.0 + v * 76.0) as u8,  // 16 → 92
                    (44.0 + v * 202.0) as u8, // 44 → 246
                );
            }
            // Teal gradient: #0A191E (centre) → #1FB9A8 (peaks / accent).
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Color::Rgb(
                (10.0 + v * 21.0) as u8,  // 10 → 31
                (25.0 + v * 160.0) as u8, // 25 → 185
                (30.0 + v * 138.0) as u8, // 30 → 168
            )
        }
        EffectiveColorMode::Ansi256 | EffectiveColorMode::Ansi16 => match state {
            WaveState::Stalled => theme.error.fg.unwrap_or(Color::Red),
            WaveState::Network { .. } => Color::Magenta,
            _ => theme.highlight.fg.unwrap_or(Color::Yellow),
        },
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
    // Quadratic fade keeps low buckets dark so the peak stands out.
    #[allow(clippy::cast_precision_loss)]
    let t = (bucket as f32 / 7.0).powi(2);
    if matches!(state, WaveState::Network { .. }) {
        // Violet fade: 0 → dark (#14102C), 7 → #8B5CF6.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        return Color::Rgb(
            (20.0_f32 + t * 119.0) as u8,
            (16.0_f32 + t * 76.0) as u8,
            (44.0_f32 + t * 202.0) as u8,
        );
    }
    // Teal fade: 0 → dark (#0A191E), 7 → accent (#1FB9A8).
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
                    WaveState::Network { sines: 2 },
                    WaveState::Network { sines: 3 },
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
            WaveState::Network { sines: 2 },
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

    // --- EqualizerWidget (braille waveform) ---------------------------------

    /// Count terminal rows that contain at least one non-blank cell.
    fn non_blank_rows(buf: &Buffer, area: Rect) -> usize {
        (0..area.height)
            .filter(|&row| {
                (0..area.width).any(|col| {
                    let s = buf[(area.left() + col, area.top() + row)].symbol();
                    !s.trim().is_empty()
                })
            })
            .count()
    }

    /// Idle collapses the wave to a single flat centre line — exactly one row lit.
    #[test]
    fn wave_widget_idle_is_flat_line() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 12, 4);
        let mut buf = Buffer::empty(area);
        EqualizerWidget {
            state: WaveState::Idle,
            tick: 123,
            theme: &theme,
            color_mode: EffectiveColorMode::Truecolor,
            ascii_only: false,
        }
        .render(area, &mut buf);
        assert_eq!(
            non_blank_rows(&buf, area),
            1,
            "Idle must render a single flat centre line"
        );
    }

    /// A busy state spreads the wave across more than one terminal row for at
    /// least one tick (mirrored amplitude above/below the centre axis).
    #[test]
    fn wave_widget_busy_spreads_vertically() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 16, 4);
        let spread = (0u64..40).any(|tick| {
            let mut buf = Buffer::empty(area);
            EqualizerWidget {
                state: WaveState::Streaming,
                tick,
                theme: &theme,
                color_mode: EffectiveColorMode::Truecolor,
                ascii_only: false,
            }
            .render(area, &mut buf);
            non_blank_rows(&buf, area) > 1
        });
        assert!(spread, "busy wave must span >1 row for some tick");
    }

    /// Rendering into a degenerate 1×1 area must never panic.
    #[test]
    fn wave_widget_tiny_area_no_panic() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        EqualizerWidget {
            state: WaveState::Tool,
            tick: 7,
            theme: &theme,
            color_mode: EffectiveColorMode::Truecolor,
            ascii_only: false,
        }
        .render(area, &mut buf);
    }

    /// ASCII mode emits only density characters, never braille code points.
    #[test]
    fn wave_widget_ascii_has_no_braille() {
        let theme = Theme::default();
        let area = Rect::new(0, 0, 16, 4);
        let mut buf = Buffer::empty(area);
        EqualizerWidget {
            state: WaveState::Swell,
            tick: 11,
            theme: &theme,
            color_mode: EffectiveColorMode::Ansi256,
            ascii_only: true,
        }
        .render(area, &mut buf);
        for row in 0..area.height {
            for col in 0..area.width {
                let s = buf[(area.left() + col, area.top() + row)].symbol();
                assert!(
                    s.chars().all(|c| !('\u{2800}'..='\u{28FF}').contains(&c)),
                    "ASCII mode must not emit braille: {s:?}"
                );
            }
        }
    }

    /// `ascii_density` maps dot-count buckets to increasing ink density.
    #[test]
    fn ascii_density_buckets() {
        assert_eq!(ascii_density(0x00), ' ');
        assert_eq!(ascii_density(0x01), '.'); // 1 dot
        assert_eq!(ascii_density(0x0F), ':'); // 4 dots
        assert_eq!(ascii_density(0x3F), '+'); // 6 dots
        assert_eq!(ascii_density(0xFF), '#'); // 8 dots
    }

    /// Network peaks are violet (blue-dominant) while foreground work is teal
    /// (green-dominant) — the two activity classes must be colour-separable.
    #[test]
    fn network_wave_color_is_violet_distinct_from_teal() {
        let theme = Theme::default();
        let net = wave_color(
            1.0,
            WaveState::Network { sines: 2 },
            EffectiveColorMode::Truecolor,
            &theme,
        );
        let teal = wave_color(
            1.0,
            WaveState::Streaming,
            EffectiveColorMode::Truecolor,
            &theme,
        );
        let Color::Rgb(nr, ng, nb) = net else {
            panic!("expected Rgb for Network peak, got {net:?}");
        };
        let Color::Rgb(_tr, tg, tb) = teal else {
            panic!("expected Rgb for Streaming peak, got {teal:?}");
        };
        assert!(
            nb > ng && nb > nr,
            "Network peak must be blue-dominant (violet); got r={nr} g={ng} b={nb}"
        );
        assert!(tg > tb, "Streaming (teal) peak must be green-dominant");
        assert_ne!(net, teal, "Network and foreground colours must differ");
    }

    /// ANSI mode maps `Network` to magenta, distinct from the foreground highlight.
    #[test]
    fn network_wave_color_ansi_is_magenta() {
        let theme = Theme::default();
        let net = wave_color(
            1.0,
            WaveState::Network { sines: 1 },
            EffectiveColorMode::Ansi16,
            &theme,
        );
        assert_eq!(net, Color::Magenta);
    }
}
