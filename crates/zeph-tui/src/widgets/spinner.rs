// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Breeze spinner frames for the TUI.
//!
//! Provides a 6-frame fill-then-empty animation in two variants: Unicode (▹/▸)
//! and ASCII (./>) for terminals that cannot display the Unicode glyphs.
//!
//! All spinner sites in the crate should call [`breeze_frame`] instead of
//! maintaining their own frame tables.

/// Unicode breeze frames — 6 frames, each exactly 3 cells wide.
///
/// Animation cadence: fill left-to-right, then empty left-to-right.
///
/// ```
/// use zeph_tui::widgets::spinner::BREEZE_FRAMES;
/// assert_eq!(BREEZE_FRAMES[0], "▹▹▹");
/// assert_eq!(BREEZE_FRAMES.len(), 6);
/// ```
pub const BREEZE_FRAMES: [&str; 6] = ["▹▹▹", "▸▹▹", "▸▸▹", "▸▸▸", "▹▸▸", "▹▹▸"];

/// ASCII breeze frames — 6 frames mirroring the Unicode fill-then-empty cadence.
///
/// Used when the terminal is not capable of rendering Unicode glyphs (e.g. `TERM=dumb`).
///
/// ```
/// use zeph_tui::widgets::spinner::BREEZE_ASCII;
/// assert_eq!(BREEZE_ASCII[0], "...");
/// assert_eq!(BREEZE_ASCII.len(), 6);
/// ```
pub const BREEZE_ASCII: [&str; 6] = ["...", ">..", ">>.", ">>>", ".>>", "..>"];

/// Return the current breeze frame for the given tick counter.
///
/// `tick` is taken modulo 6 so the animation wraps cleanly. `ascii` selects the
/// ASCII fallback when the terminal cannot render the Unicode glyphs.
///
/// # Examples
///
/// ```
/// use zeph_tui::widgets::spinner::breeze_frame;
///
/// assert_eq!(breeze_frame(0, false), "▹▹▹");
/// assert_eq!(breeze_frame(3, false), "▸▸▸");
/// assert_eq!(breeze_frame(6, false), "▹▹▹"); // wraps
/// assert_eq!(breeze_frame(0, true),  "...");
/// ```
///
/// TODO(#5095): pause animation when a breathing separator is active (coordination deferred).
#[must_use]
pub fn breeze_frame(tick: u64, ascii: bool) -> &'static str {
    let idx = (tick % 6) as usize;
    if ascii {
        BREEZE_ASCII[idx]
    } else {
        BREEZE_FRAMES[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_frame_zero() {
        assert_eq!(breeze_frame(0, false), "▹▹▹");
    }

    #[test]
    fn unicode_all_six_frames() {
        let expected = ["▹▹▹", "▸▹▹", "▸▸▹", "▸▸▸", "▹▸▸", "▹▹▸"];
        for (tick, &frame) in expected.iter().enumerate() {
            assert_eq!(breeze_frame(tick as u64, false), frame, "tick={tick}");
        }
    }

    #[test]
    fn ascii_all_six_frames() {
        let expected = ["...", ">..", ">>.", ">>>", ".>>", "..>"];
        for (tick, &frame) in expected.iter().enumerate() {
            assert_eq!(breeze_frame(tick as u64, true), frame, "tick={tick}");
        }
    }

    #[test]
    fn wraps_at_six() {
        assert_eq!(breeze_frame(6, false), breeze_frame(0, false));
        assert_eq!(breeze_frame(7, false), breeze_frame(1, false));
        assert_eq!(breeze_frame(12, false), breeze_frame(0, false));
    }

    #[test]
    fn ascii_wraps_at_six() {
        assert_eq!(breeze_frame(6, true), breeze_frame(0, true));
    }
}
