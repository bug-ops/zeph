// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Semantic colour palette — the source of truth from which all widget [`super::Theme`] styles
//! are derived. A [`SemanticPalette`] names colours by role rather than by hue.

use serde::{Deserialize, Serialize};

/// A 24-bit RGB colour represented as `#rrggbb` hex in TOML/JSON.
///
/// Converts to [`ratatui::style::Color::Rgb`] via [`From<Rgb> for ratatui::style::Color`].
///
/// # Examples
///
/// ```rust
/// use zeph_tui::theme::palette::Rgb;
///
/// let r: Rgb = serde_json::from_str(r##""#1FB9A8""##).unwrap();
/// assert_eq!(r, Rgb(0x1F, 0xB9, 0xA8));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Parse a `#rrggbb` hex string.
    ///
    /// # Errors
    ///
    /// Returns an error string when the input is not a valid `#rrggbb` literal.
    pub fn from_hex(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let hex = s
            .strip_prefix('#')
            .ok_or_else(|| format!("missing '#' prefix: {s}"))?;
        if hex.len() != 6 {
            return Err(format!(
                "expected 6 hex digits after '#', got {}: {s}",
                hex.len()
            ));
        }
        let r = u8::from_str_radix(&hex[0..2], 16)
            .map_err(|e| format!("invalid red component in '{s}': {e}"))?;
        let g = u8::from_str_radix(&hex[2..4], 16)
            .map_err(|e| format!("invalid green component in '{s}': {e}"))?;
        let b = u8::from_str_radix(&hex[4..6], 16)
            .map_err(|e| format!("invalid blue component in '{s}': {e}"))?;
        Ok(Self(r, g, b))
    }
}

impl From<Rgb> for ratatui::style::Color {
    fn from(Rgb(r, g, b): Rgb) -> Self {
        Self::Rgb(r, g, b)
    }
}

impl Serialize for Rgb {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("#{:02X}{:02X}{:02X}", self.0, self.1, self.2))
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Extended semantic roles for accent colours that cannot be derived from the base 10 roles.
///
/// Prevents lossy folding of colours like burnt-orange (`assistant_accent`) and amber
/// (`highlight`) into the main accent or warning roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtendedRoles {
    /// Secondary accent — burnt-orange / olive tones for `assistant_accent` / `tool_accent`.
    pub accent_alt: Rgb,
    /// Amber highlight for `highlight` widget fields.
    pub highlight: Rgb,
    /// Navy header background for `header.bg`.
    pub header_bg: Rgb,
}

/// Ten semantic role colours that define a complete visual theme.
///
/// All widget [`super::Theme`] styles are derived from these roles via
/// [`super::Theme::from_palette`] / [`super::Theme::from_palette_with_mode`].
///
/// # TOML format
///
/// ```toml
/// [tui.theme.palette]
/// bg      = "#0E1216"
/// surface = "#132022"
/// text    = "#C8D3D9"
/// # …
/// ```
///
/// # Examples
///
/// ```rust
/// use zeph_tui::theme::palette::SemanticPalette;
///
/// let p = SemanticPalette::zephyr();
/// // The zephyr palette uses an aqua accent.
/// assert_eq!(p.accent, zeph_tui::theme::palette::Rgb(0x1F, 0xB9, 0xA8));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticPalette {
    /// Base background — darkest surface (app background).
    pub bg: Rgb,
    /// Raised panel / code-block background — slightly lighter than `bg`.
    pub surface: Rgb,
    /// Primary foreground — default text colour.
    pub text: Rgb,
    /// De-emphasised text — timestamps, gutters, dim labels.
    pub muted: Rgb,
    /// Brand / interactive accent — links, active elements, user messages.
    pub accent: Rgb,
    /// Success state — completed operations, positive indicators.
    pub success: Rgb,
    /// Caution / running state — in-progress indicators.
    pub warning: Rgb,
    /// Failure / error state.
    pub error: Rgb,
    /// Neutral informational notices.
    pub info: Rgb,
    /// Panel borders and separators.
    pub border: Rgb,
    /// Extended roles for colours that cannot be derived from the base 10.
    pub extended: ExtendedRoles,
}

impl SemanticPalette {
    /// The canonical Zephyr dark aqua palette.
    ///
    /// This is the default palette used when `[tui.theme] name` is empty or `"zephyr"`.
    #[must_use]
    pub fn zephyr() -> Self {
        Self {
            bg: Rgb(0x0E, 0x12, 0x16),
            surface: Rgb(0x13, 0x20, 0x22),
            text: Rgb(0xC8, 0xD3, 0xD9),
            muted: Rgb(0x64, 0x69, 0x6F),
            accent: Rgb(0x1F, 0xB9, 0xA8),
            success: Rgb(0x7E, 0xE8, 0xA2),
            warning: Rgb(0xE8, 0xC7, 0x7E),
            error: Rgb(0xE2, 0x6D, 0x6D),
            info: Rgb(0x6F, 0xDC, 0xD2),
            border: Rgb(0x2A, 0x3A, 0x3F),
            extended: ExtendedRoles {
                accent_alt: Rgb(0xB9, 0x55, 0x19),
                highlight: Rgb(0xD7, 0x96, 0x3C),
                header_bg: Rgb(0x14, 0x28, 0x50),
            },
        }
    }

    /// The "classic" palette — maps the current hardcoded `Theme::default()` values to roles.
    ///
    /// Intended as a stable reference for users upgrading from pre-2.0 themes.
    #[must_use]
    pub fn classic() -> Self {
        Self {
            bg: Rgb(0x00, 0x00, 0x00),
            surface: Rgb(0x0F, 0x1E, 0x37),
            text: Rgb(0xC8, 0xC8, 0xD2),
            muted: Rgb(0x69, 0x69, 0x69),
            accent: Rgb(0x00, 0xFF, 0xFF),  // Color::Cyan
            success: Rgb(0x00, 0x80, 0x00), // Color::Green
            warning: Rgb(0xFF, 0xFF, 0x00), // Color::Yellow
            error: Rgb(0xFF, 0x00, 0x00),   // Color::Red
            info: Rgb(0x64, 0xB4, 0xFF),
            border: Rgb(0x80, 0x80, 0x80), // Color::Gray
            extended: ExtendedRoles {
                accent_alt: Rgb(0xB9, 0x55, 0x19),
                highlight: Rgb(0xD7, 0x96, 0x3C),
                header_bg: Rgb(0x14, 0x28, 0x50),
            },
        }
    }
}

impl Default for SemanticPalette {
    fn default() -> Self {
        Self::zephyr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_roundtrip_serde() {
        let r = Rgb(0x1F, 0xB9, 0xA8);
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r##""#1FB9A8""##);
        let back: Rgb = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn rgb_from_hex_valid() {
        assert_eq!(Rgb::from_hex("#0E1216").unwrap(), Rgb(0x0E, 0x12, 0x16));
        assert_eq!(Rgb::from_hex("#FFFFFF").unwrap(), Rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(Rgb::from_hex("#000000").unwrap(), Rgb(0, 0, 0));
    }

    #[test]
    fn rgb_from_hex_invalid() {
        assert!(Rgb::from_hex("0E1216").is_err()); // missing #
        assert!(Rgb::from_hex("#0E121").is_err()); // too short
        assert!(Rgb::from_hex("#0E12167").is_err()); // too long
        assert!(Rgb::from_hex("#GGGGGG").is_err()); // invalid hex
    }

    #[test]
    fn rgb_to_ratatui_color() {
        use ratatui::style::Color;
        let c: Color = Rgb(0x1F, 0xB9, 0xA8).into();
        assert_eq!(c, Color::Rgb(0x1F, 0xB9, 0xA8));
    }

    #[test]
    fn zephyr_palette_accent_is_aqua() {
        let p = SemanticPalette::zephyr();
        assert_eq!(p.accent, Rgb(0x1F, 0xB9, 0xA8));
    }
}
