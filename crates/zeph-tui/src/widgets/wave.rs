// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Deterministic wave animation for the input separator row (#5096).
//!
//! The wave is a pure function of `(state, width, t)` where `t` is a monotonic
//! `u64` tick counter owned by `App`. No wall-clock reads happen here — all
//! time-dependence flows through the explicit `t` argument so snapshot tests
//! and property tests stay bit-identical.
//!
//! # Architecture
//!
//! - [`WaveState`] — discriminates the 6 visual modes; derived in `App::wave_state()`.
//! - [`sample`] — pure math: maps `(state, x, t)` to a glyph bucket `0..=7`.
//! - [`glyphs`] — converts a row width to a `Vec<Span<'static>>` using the bucket ramp.
//!
//! The caller owns a reused `Vec<Span<'static>>` buffer (`wave_buf`) and passes a `&mut` to
//! [`glyphs`] so the per-frame allocation is amortized to zero after the first frame.

use std::f32::consts::TAU;

use ratatui::style::{Color, Style};
use ratatui::text::Span;

use crate::theme::{EffectiveColorMode, Theme};

// ---------------------------------------------------------------------------
// Glyph ramps
// ---------------------------------------------------------------------------

/// Block-element gradient from thin (index 0) to full (index 7).
/// `[&'static str; 8]` so `Span::styled(WAVE_GLYPHS[b], style)` borrows a `&'static str`
/// — no per-glyph String allocation.
const WAVE_GLYPHS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// ASCII fallback ramp for `TERM=dumb` terminals.
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

/// Per-state wave parameters passed to [`sample`].
///
/// `amplitude`, `wavelength`, and `omega` are tuned for the 250 ms tick rate
/// (4 fps). Per-tick phase step = `omega`; per-column spatial frequency = 2π/λ.
/// Nyquist constraint: omega < π, λ ≥ ~8 columns.
#[derive(Debug, Clone, Copy)]
struct WaveParams {
    /// Peak displacement in `[0, 1]`.
    amplitude: f32,
    /// Spatial wavelength in columns.
    wavelength: f32,
    /// Phase advance per tick (radians).
    omega: f32,
    /// Whether to apply a choppy short-λ secondary component.
    choppy: bool,
    /// Number of parallel sines to superpose (only for `Parallel`).
    sines: u8,
}

impl WaveState {
    /// Return the render parameters for this state.
    fn params(self) -> WaveParams {
        match self {
            // Idle and Stalled both render a flat line (amplitude=0); colour tint is
            // applied by `glyphs()` based on the WaveState variant, not params.
            WaveState::Idle | WaveState::Stalled => WaveParams {
                amplitude: 0.0,
                wavelength: 1.0, // unused (A=0)
                omega: 0.0,
                choppy: false,
                sines: 1,
            },
            WaveState::Swell => WaveParams {
                amplitude: 0.9,
                wavelength: 40.0, // long roll
                omega: 0.25,
                choppy: false,
                sines: 1,
            },
            WaveState::Streaming => WaveParams {
                amplitude: 0.6,
                wavelength: 12.0,
                omega: 0.7,
                choppy: false,
                sines: 1,
            },
            WaveState::Tool => WaveParams {
                amplitude: 0.5,
                wavelength: 4.0, // short choppy
                omega: 1.0,
                choppy: true,
                sines: 1,
            },
            WaveState::Parallel { sines } => WaveParams {
                amplitude: 0.5,
                wavelength: 14.0, // primary λ
                omega: 0.6,
                choppy: false,
                sines: sines.clamp(2, 3),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Core math
// ---------------------------------------------------------------------------

/// Return the glyph bucket index `0..=7` for column `x` at tick `t`.
///
/// This function is pure — given identical `(state, x, t)` arguments it always
/// returns the same value. Use it directly in snapshot and property tests.
///
/// The `u64 → f32` cast for `t` is intentional: f32 mantissa is 24 bits so the
/// cast is exact below t ≈ 16.7M (≈48 days at 4 fps continuous-busy). Beyond
/// that the phase slowly drifts — visually imperceptible, not a correctness issue.
#[must_use]
pub fn sample(state: WaveState, x: u32, t: u64) -> usize {
    let p = state.params();

    if p.amplitude < f32::EPSILON {
        // Flat line — bucket 0 for Idle/Stalled.
        return 0;
    }

    #[allow(clippy::cast_precision_loss)]
    let tf = (t % 65536) as f32; // period 65536 ticks ≈ 4.5 h — harmless wrap

    #[allow(clippy::cast_precision_loss)]
    let xf = x as f32;

    let y = if p.sines <= 1 {
        let phase = TAU * xf / p.wavelength - p.omega * tf;
        let mut y = p.amplitude * phase.sin();
        if p.choppy {
            // Add a secondary short-λ component for the choppy Tool wave.
            let phase2 = TAU * xf / (p.wavelength / 2.0) - p.omega * 1.5 * tf;
            y = (y + 0.3 * phase2.sin()).clamp(-1.0, 1.0);
        }
        y
    } else {
        // Parallel: superpose `sines` unit sines with λ∈{14,9,5} and divide by count.
        let lambdas: [f32; 3] = [14.0, 9.0, 5.0];
        let count = p.sines as usize;
        let mut sum = 0.0f32;
        for (i, &lambda) in lambdas[..count].iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let phase = TAU * xf / lambda - p.omega * (1.0 + 0.2 * i as f32) * tf;
            sum += phase.sin();
        }
        #[allow(clippy::cast_precision_loss)]
        let normalized = sum / count as f32;
        p.amplitude * normalized.clamp(-1.0, 1.0)
    };

    // Map y ∈ [-1, 1] → bucket 0..=7.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bucket = ((y + 1.0) * 3.5).round() as usize;
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

/// Map a bucket index `0..=7` to an RGB colour for the Truecolor gradient.
///
/// Low buckets use a cool blue; high buckets shift to bright aqua/white.
fn bucket_to_rgb(state: WaveState, bucket: usize) -> Color {
    if matches!(state, WaveState::Stalled) {
        // Error tint: dim red gradient.
        #[allow(clippy::cast_possible_truncation)]
        let v = (80 + bucket * 22) as u8;
        return Color::Rgb(v, 20, 20);
    }
    // Aqua gradient: r stays low, g and b ramp up with height.
    #[allow(clippy::cast_possible_truncation)]
    let g = (80 + bucket * 22) as u8; // 80..=234
    #[allow(clippy::cast_possible_truncation)]
    let b = (120 + bucket * 17) as u8; // 120..=239
    Color::Rgb(20, g, b)
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
