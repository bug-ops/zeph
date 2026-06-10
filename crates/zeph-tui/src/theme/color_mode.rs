// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Terminal colour capability detection and colour-space downgrade pipeline.
//!
//! [`EffectiveColorMode`] is the resolved, non-`Auto` result of [`resolve_color_mode`].
//! [`map_color`] and [`apply_mode`] are the single seams through which all palette colours
//! pass during [`super::Theme`] derivation — never at render time.

use ratatui::style::{Color, Style};
use zeph_config::ColorMode;

/// Resolved terminal colour capability — the result of [`resolve_color_mode`].
///
/// Unlike [`ColorMode`], this enum has no `Auto` variant. It is only produced
/// after detection has run, so `from_palette_with_mode` cannot accidentally receive
/// an unresolved value.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::theme::color_mode::{EffectiveColorMode, resolve_color_mode};
/// use zeph_config::ColorMode;
///
/// let mode = resolve_color_mode(ColorMode::Truecolor);
/// assert_eq!(mode, EffectiveColorMode::Truecolor);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveColorMode {
    /// 24-bit RGB — no downgrade performed.
    Truecolor,
    /// RGB colours are mapped to the nearest xterm-256 index.
    Ansi256,
    /// RGB colours are mapped to the nearest ANSI-16 named colour.
    Ansi16,
    /// All colours stripped; text modifiers (BOLD, DIM, UNDERLINED) are preserved.
    Never,
}

/// Resolve a [`ColorMode`] to an [`EffectiveColorMode`], running terminal detection when `Auto`.
///
/// Detection order (per <https://no-color.org> and common terminal conventions):
/// 1. `NO_COLOR` present in environment (any value, including empty) → `Never`.
/// 2. `COLORTERM` ∈ `{truecolor, 24bit}` → `Truecolor`.
/// 3. `TERM` contains `256color` → `Ansi256`.
/// 4. `TERM` matches known 16-colour terminals → `Ansi16`.
/// 5. `TERM=dumb` or `TERM` unset → `Never`.
/// 6. Fallback (ambiguous) → `Ansi256` (safe modern default).
///
/// # Examples
///
/// ```rust
/// use zeph_tui::theme::color_mode::{EffectiveColorMode, resolve_color_mode};
/// use zeph_config::ColorMode;
///
/// // Non-Auto modes pass through unchanged.
/// assert_eq!(resolve_color_mode(ColorMode::Ansi16), EffectiveColorMode::Ansi16);
/// assert_eq!(resolve_color_mode(ColorMode::Never), EffectiveColorMode::Never);
/// ```
#[must_use]
pub fn resolve_color_mode(mode: ColorMode) -> EffectiveColorMode {
    match mode {
        ColorMode::Truecolor => EffectiveColorMode::Truecolor,
        ColorMode::Ansi256 => EffectiveColorMode::Ansi256,
        ColorMode::Ansi16 => EffectiveColorMode::Ansi16,
        ColorMode::Never => EffectiveColorMode::Never,
        _ => detect(),
    }
}

/// Detect the terminal's colour capability from the environment.
///
/// Per <https://no-color.org>: `NO_COLOR` disables colour when *present*, regardless of value.
fn detect() -> EffectiveColorMode {
    // M2: `NO_COLOR` — presence alone (even empty string) means disable colour.
    if std::env::var_os("NO_COLOR").is_some() {
        return EffectiveColorMode::Never;
    }

    if let Ok(colorterm) = std::env::var("COLORTERM")
        && (colorterm == "truecolor" || colorterm == "24bit")
    {
        return EffectiveColorMode::Truecolor;
    }

    if let Ok(term) = std::env::var("TERM") {
        if term == "dumb" {
            return EffectiveColorMode::Never;
        }
        if term.contains("256color") {
            return EffectiveColorMode::Ansi256;
        }
        // Known 16-colour terminal families.
        let base = term.split('-').next().unwrap_or("");
        if matches!(
            base,
            "xterm" | "screen" | "vt100" | "linux" | "rxvt" | "konsole"
        ) {
            return EffectiveColorMode::Ansi16;
        }
    } else {
        // TERM unset — cannot determine capability.
        return EffectiveColorMode::Never;
    }

    // Ambiguous — default to Ansi256 (safe, supported by most terminals since ~2017).
    EffectiveColorMode::Ansi256
}

/// Map a single [`Color`] through the downgrade pipeline for the given mode.
///
/// `Rgb` values are converted to the nearest indexed colour for `Ansi256`/`Ansi16`,
/// or stripped to `Color::Reset` for `Never`. Non-Rgb colours pass through unchanged.
///
/// # Examples
///
/// ```rust
/// use ratatui::style::Color;
/// use zeph_tui::theme::color_mode::{EffectiveColorMode, map_color};
///
/// let c = map_color(Color::Rgb(0, 0, 0), EffectiveColorMode::Truecolor);
/// assert_eq!(c, Color::Rgb(0, 0, 0));
///
/// let stripped = map_color(Color::Rgb(255, 0, 0), EffectiveColorMode::Never);
/// assert_eq!(stripped, Color::Reset);
/// ```
#[must_use]
pub fn map_color(color: Color, mode: EffectiveColorMode) -> Color {
    match mode {
        EffectiveColorMode::Truecolor => color,
        EffectiveColorMode::Never => Color::Reset,
        EffectiveColorMode::Ansi256 => {
            if let Color::Rgb(r, g, b) = color {
                Color::Indexed(rgb_to_ansi256(r, g, b))
            } else {
                color
            }
        }
        EffectiveColorMode::Ansi16 => {
            if let Color::Rgb(r, g, b) = color {
                Color::Indexed(rgb_to_ansi16(r, g, b))
            } else {
                color
            }
        }
    }
}

/// Apply the colour mode to a [`Style`], downgrading all `Rgb` colours in fg/bg.
///
/// For [`EffectiveColorMode::Never`]: removes all fg/bg colours but preserves modifiers.
///
/// # Examples
///
/// ```rust
/// use ratatui::style::{Color, Modifier, Style};
/// use zeph_tui::theme::color_mode::{EffectiveColorMode, apply_mode};
///
/// let bold_red = Style::default().fg(Color::Rgb(255, 0, 0)).add_modifier(Modifier::BOLD);
/// let stripped = apply_mode(bold_red, EffectiveColorMode::Never);
/// assert_eq!(stripped.fg, None);
/// assert_eq!(stripped.bg, None);
/// assert!(stripped.add_modifier.contains(Modifier::BOLD));
/// ```
#[must_use]
pub fn apply_mode(style: Style, mode: EffectiveColorMode) -> Style {
    match mode {
        EffectiveColorMode::Truecolor => style,
        EffectiveColorMode::Never => Style {
            fg: None,
            bg: None,
            underline_color: None,
            add_modifier: style.add_modifier,
            sub_modifier: style.sub_modifier,
        },
        EffectiveColorMode::Ansi256 | EffectiveColorMode::Ansi16 => {
            let mut s = style;
            if let Some(fg) = s.fg {
                s.fg = Some(map_color(fg, mode));
            }
            if let Some(bg) = s.bg {
                s.bg = Some(map_color(bg, mode));
            }
            if let Some(ul) = s.underline_color {
                s.underline_color = Some(map_color(ul, mode));
            }
            s
        }
    }
}

// ── ANSI-256 mapping (M4: compute both cube and gray-ramp, pick nearest) ─────────────────────────

/// Map an RGB triplet to the nearest xterm-256 palette index.
///
/// Per the critic's M4 requirement: always compute both the 6×6×6 cube candidate
/// and the 24-step gray-ramp candidate and return whichever has smaller Euclidean distance.
fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let (cube_idx, cube_dist) = nearest_cube(r, g, b);
    let (gray_idx, gray_dist) = nearest_gray_ramp(r, g, b);
    if gray_dist <= cube_dist {
        gray_idx
    } else {
        cube_idx
    }
}

/// Quantise one 8-bit channel to the nearest of the 6 cube levels {0,95,135,175,215,255}.
fn quantize_cube_level(v: u8) -> (u8, u8) {
    // Cube levels and their 8-bit values.
    const LEVELS: [(u8, u8); 6] = [(0, 0), (1, 95), (2, 135), (3, 175), (4, 215), (5, 255)];
    const LEVEL_VALS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let vi = i16::from(v);
    let mut best_idx = 0u8;
    let mut best_dist = i32::MAX;
    for (idx, level) in LEVELS {
        let d = i32::from((vi - i16::from(level)).abs());
        if d < best_dist {
            best_dist = d;
            best_idx = idx;
        }
    }
    (best_idx, LEVEL_VALS[best_idx as usize])
}

fn nearest_cube(r: u8, g: u8, b: u8) -> (u8, u32) {
    let (ri, rv) = quantize_cube_level(r);
    let (gi, gv) = quantize_cube_level(g);
    let (bi, bv) = quantize_cube_level(b);
    let idx = 16 + 36 * ri + 6 * gi + bi;
    let dist = dist_sq(r, g, b, rv, gv, bv);
    (idx, dist)
}

fn nearest_gray_ramp(r: u8, g: u8, b: u8) -> (u8, u32) {
    // Gray ramp: indices 232–255, values 8,18,28,…,238 (step 10, 24 entries).
    let luma = (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000;
    // Ramp values: 8 + 10 * n for n in 0..24.
    let n = if luma < 8 {
        0u8
    } else if luma >= 238 {
        23u8
    } else {
        u8::try_from((luma - 8 + 5) / 10).unwrap_or(23)
    };
    let n = n.min(23);
    let gray_val = 8 + 10 * n;
    let idx = 232 + n;
    let dist = dist_sq(r, g, b, gray_val, gray_val, gray_val);
    (idx, dist)
}

fn dist_sq(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let dr = u32::from(r1.abs_diff(r2)).pow(2);
    let dg = u32::from(g1.abs_diff(g2)).pow(2);
    let db = u32::from(b1.abs_diff(b2)).pow(2);
    dr + dg + db
}

// ── ANSI-16 mapping ───────────────────────────────────────────────────────────────────────────────

/// Standard xterm ANSI-16 palette values (indices 0–15).
const ANSI16_PALETTE: [(u8, u8, u8); 16] = [
    (0, 0, 0),       // 0  Black
    (128, 0, 0),     // 1  Red
    (0, 128, 0),     // 2  Green
    (128, 128, 0),   // 3  Yellow
    (0, 0, 128),     // 4  Blue
    (128, 0, 128),   // 5  Magenta
    (0, 128, 128),   // 6  Cyan
    (192, 192, 192), // 7  White
    (128, 128, 128), // 8  BrightBlack (Dark Gray)
    (255, 0, 0),     // 9  BrightRed
    (0, 255, 0),     // 10 BrightGreen
    (255, 255, 0),   // 11 BrightYellow
    (0, 0, 255),     // 12 BrightBlue
    (255, 0, 255),   // 13 BrightMagenta
    (0, 255, 255),   // 14 BrightCyan
    (255, 255, 255), // 15 BrightWhite
];

fn rgb_to_ansi16(r: u8, g: u8, b: u8) -> u8 {
    let mut best_idx = 0u8;
    let mut best_dist = u32::MAX;
    for (i, &(pr, pg, pb)) in ANSI16_PALETTE.iter().enumerate() {
        let d = dist_sq(r, g, b, pr, pg, pb);
        if d < best_dist {
            best_dist = d;
            #[allow(clippy::cast_possible_truncation)]
            {
                best_idx = i as u8;
            } // 16 entries — always fits u8
        }
    }
    best_idx
}

#[cfg(test)]
mod tests {
    use ratatui::style::Modifier;

    use super::*;

    #[test]
    fn no_color_strips_all_colors() {
        // NO_COLOR semantics: fg/bg removed, modifiers kept.
        let style = Style::default()
            .fg(Color::Rgb(255, 0, 0))
            .bg(Color::Rgb(0, 0, 0))
            .add_modifier(Modifier::BOLD);
        let out = apply_mode(style, EffectiveColorMode::Never);
        assert_eq!(out.fg, None);
        assert_eq!(out.bg, None);
        assert!(out.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn truecolor_identity() {
        let style = Style::default().fg(Color::Rgb(31, 185, 168));
        assert_eq!(apply_mode(style, EffectiveColorMode::Truecolor), style);
    }

    #[test]
    fn ansi256_black_maps_to_index_16() {
        // Pure black (0,0,0) — cube index 16 (0+0+0 = 16), gray ramp index 232 (value 8).
        // Distance to cube: 0; distance to gray ramp: 8^2*3 = 192. Cube wins.
        let idx = rgb_to_ansi256(0, 0, 0);
        assert_eq!(idx, 16, "black should map to cube index 16");
    }

    #[test]
    fn ansi256_near_gray_picks_ramp() {
        // (128, 128, 128) — equidistant among cube and ramp.
        // Gray ramp: luma≈128, n=(128-8+5)/10=12, val=128, dist=0.
        // Cube: quantize(128) → level 2 (135), dist=(128-135)^2 * 3 = 147.
        let idx = rgb_to_ansi256(128, 128, 128);
        assert!(
            idx >= 232,
            "near-gray (128,128,128) should prefer gray ramp, got {idx}"
        );
    }

    #[test]
    fn ansi256_color_downgrade() {
        // #1FB9A8 (31, 185, 168) should produce a valid 0–255 index.
        let idx = rgb_to_ansi256(31, 185, 168);
        // idx is u8 (0–255 by type); just verify it doesn't panic.
        let _ = idx;
    }

    #[test]
    fn ansi16_pure_red() {
        // (255, 0, 0) should map to index 9 (BrightRed) or 1 (Red).
        let idx = rgb_to_ansi16(255, 0, 0);
        assert!(
            idx == 1 || idx == 9,
            "pure red should map to red or bright red, got {idx}"
        );
    }

    #[test]
    fn map_color_never_resets() {
        assert_eq!(
            map_color(Color::Rgb(255, 128, 0), EffectiveColorMode::Never),
            Color::Reset
        );
    }

    #[test]
    fn resolve_color_mode_passthrough() {
        assert_eq!(
            resolve_color_mode(ColorMode::Truecolor),
            EffectiveColorMode::Truecolor
        );
        assert_eq!(
            resolve_color_mode(ColorMode::Ansi256),
            EffectiveColorMode::Ansi256
        );
        assert_eq!(
            resolve_color_mode(ColorMode::Ansi16),
            EffectiveColorMode::Ansi16
        );
        assert_eq!(
            resolve_color_mode(ColorMode::Never),
            EffectiveColorMode::Never
        );
    }

    #[test]
    #[serial_test::serial]
    #[allow(unsafe_code)]
    fn auto_with_no_color_env_resolves_to_never() {
        // Temporarily set NO_COLOR. Per no-color.org, presence alone (even empty) disables colour.
        // serial guards against parallel tests mutating the same env var.
        // SAFETY: single-threaded via #[serial]; no other test reads this env var concurrently.
        unsafe { std::env::set_var("NO_COLOR", "1") };
        let result = resolve_color_mode(ColorMode::Auto);
        unsafe { std::env::remove_var("NO_COLOR") };
        assert_eq!(result, EffectiveColorMode::Never);
    }
}
