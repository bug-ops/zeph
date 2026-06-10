// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Named theme presets embedded at compile time and user-defined theme file loading.
//!
//! Resolution order for a configured theme name:
//! 1. Exact preset name match (case-sensitive).
//! 2. User theme file `~/.config/zeph/themes/<name>.toml`.
//! 3. Error with a clear message; callers should fall back to [`Preset::Zephyr`].
//!
//! Preset names take precedence over user files — a user cannot shadow a built-in
//! preset by placing a file with the same name (security: prevents built-in override).

use std::path::PathBuf;

use super::palette::SemanticPalette;

/// Error type for theme loading failures.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ThemeLoadError {
    /// The theme name contains a path traversal or separator.
    #[error("unsafe theme name '{0}': must not contain path separators, '..', or be absolute")]
    UnsafeName(String),
    /// File size exceeds the 64 KiB safety cap.
    #[error("theme file '{path}' exceeds 64 KiB limit ({size} bytes)")]
    FileTooLarge { path: PathBuf, size: u64 },
    /// The theme file could not be read.
    #[error("failed to read theme file '{path}': {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The TOML content could not be parsed as a [`SemanticPalette`].
    #[error("failed to parse theme '{name}': {source}")]
    Parse {
        name: String,
        #[source]
        source: toml::de::Error,
    },
    /// No preset or user file found for the given name.
    #[error("unknown theme '{name}': no built-in preset and no user file at '{path}'")]
    NotFound { name: String, path: PathBuf },
}

/// Built-in named theme presets embedded at compile time.
///
/// # Examples
///
/// ```rust
/// use zeph_tui::theme::presets::Preset;
///
/// let p = Preset::Zephyr.palette();
/// assert_eq!(p.accent, zeph_tui::theme::palette::Rgb(0x1F, 0xB9, 0xA8));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Default dark aqua palette.
    Zephyr,
    /// Light variant of the Zephyr palette.
    ZephyrLight,
    /// Maximum contrast for accessibility.
    HighContrast,
    /// Maps legacy `Theme::default()` hardcoded colours to palette roles.
    Classic,
    /// Catppuccin Mocha dark palette.
    CatppuccinMocha,
    /// Gruvbox dark retro palette.
    GruvboxDark,
    /// Solarized dark palette.
    SolarizedDark,
}

/// All preset variants — used in tests to ensure every variant parses successfully.
pub const ALL_PRESETS: &[Preset] = &[
    Preset::Zephyr,
    Preset::ZephyrLight,
    Preset::HighContrast,
    Preset::Classic,
    Preset::CatppuccinMocha,
    Preset::GruvboxDark,
    Preset::SolarizedDark,
];

impl Preset {
    /// Parse and return the embedded [`SemanticPalette`] for this preset.
    ///
    /// # Panics
    ///
    /// Panics if the embedded TOML is invalid — guaranteed not to happen at runtime
    /// because `#[test] all_presets_parse` covers every variant.
    #[must_use]
    pub fn palette(self) -> SemanticPalette {
        toml::from_str(self.toml_src()).expect("embedded preset TOML is always valid")
    }

    /// Return the raw embedded TOML source for this preset.
    #[must_use]
    pub fn toml_src(self) -> &'static str {
        match self {
            Self::Zephyr => include_str!("presets/zephyr.toml"),
            Self::ZephyrLight => include_str!("presets/zephyr-light.toml"),
            Self::HighContrast => include_str!("presets/high-contrast.toml"),
            Self::Classic => include_str!("presets/classic.toml"),
            Self::CatppuccinMocha => include_str!("presets/catppuccin-mocha.toml"),
            Self::GruvboxDark => include_str!("presets/gruvbox-dark.toml"),
            Self::SolarizedDark => include_str!("presets/solarized-dark.toml"),
        }
    }

    /// Resolve a theme name to a [`Preset`], or return `None` if unrecognised.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "zephyr" | "" => Some(Self::Zephyr),
            "zephyr-light" => Some(Self::ZephyrLight),
            "high-contrast" => Some(Self::HighContrast),
            "classic" => Some(Self::Classic),
            "catppuccin-mocha" => Some(Self::CatppuccinMocha),
            "gruvbox-dark" => Some(Self::GruvboxDark),
            "solarized-dark" => Some(Self::SolarizedDark),
            _ => None,
        }
    }
}

/// Resolve a palette by name: preset match → user file → error.
///
/// # Security (M3)
///
/// - Rejects names containing path separators (`/`, `\`), `..`, or absolute paths.
/// - Preset names take precedence — user files cannot shadow built-ins.
/// - User files are capped at 64 KiB before parsing.
///
/// # Errors
///
/// Returns [`ThemeLoadError`] when the name is unsafe, the file cannot be read,
/// is too large, cannot be parsed, or no match is found.
pub fn resolve_palette(name: &str) -> Result<SemanticPalette, ThemeLoadError> {
    // M3: validate name before joining into a path.
    validate_theme_name(name)?;

    // Preset names take precedence (prevents user shadowing built-ins).
    if let Some(preset) = Preset::from_name(name) {
        return Ok(preset.palette());
    }

    // Fall back to user theme file.
    load_user_theme(name)
}

/// Validate a theme name for path safety (M3).
fn validate_theme_name(name: &str) -> Result<(), ThemeLoadError> {
    if name.is_empty() {
        return Ok(()); // empty → zephyr default
    }
    // Reject absolute paths.
    if name.starts_with('/') || name.starts_with('\\') {
        return Err(ThemeLoadError::UnsafeName(name.to_owned()));
    }
    // Reject path separators and traversal.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ThemeLoadError::UnsafeName(name.to_owned()));
    }
    // Reject Windows absolute paths like `C:\`.
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        return Err(ThemeLoadError::UnsafeName(name.to_owned()));
    }
    Ok(())
}

/// Load a user-defined theme from `~/.config/zeph/themes/<name>.toml`.
///
/// # Errors
///
/// Returns [`ThemeLoadError`] if the name is not found, the file is too large, or
/// the TOML cannot be parsed.
fn load_user_theme(name: &str) -> Result<SemanticPalette, ThemeLoadError> {
    use std::io::Read;

    const MAX_SIZE: u64 = 64 * 1024; // 64 KiB

    let themes_dir = user_themes_dir();
    let path = themes_dir.join(format!("{name}.toml"));

    // Reject symlinks before opening to prevent following links out of the themes dir.
    let meta = std::fs::symlink_metadata(&path).map_err(|_| ThemeLoadError::NotFound {
        name: name.to_owned(),
        path: path.clone(),
    })?;
    if meta.file_type().is_symlink() {
        return Err(ThemeLoadError::NotFound {
            name: name.to_owned(),
            path,
        });
    }

    // Read with an explicit byte cap — authoritative regardless of metadata/TOCTOU race.
    let f = std::fs::File::open(&path).map_err(|e| ThemeLoadError::Io {
        path: path.clone(),
        source: e,
    })?;
    let mut buf = String::new();
    f.take(MAX_SIZE + 1)
        .read_to_string(&mut buf)
        .map_err(|e| ThemeLoadError::Io {
            path: path.clone(),
            source: e,
        })?;
    if buf.len() as u64 > MAX_SIZE {
        return Err(ThemeLoadError::FileTooLarge {
            path,
            size: buf.len() as u64,
        });
    }

    toml::from_str(&buf).map_err(|e| ThemeLoadError::Parse {
        name: name.to_owned(),
        source: e,
    })
}

fn user_themes_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("zeph")
        .join("themes")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S5: every Preset variant must parse without panicking.
    #[test]
    fn all_presets_parse() {
        for &preset in ALL_PRESETS {
            let _ = preset.palette(); // panics if TOML is invalid
        }
    }

    #[test]
    fn preset_from_name_roundtrip() {
        assert_eq!(Preset::from_name("zephyr"), Some(Preset::Zephyr));
        assert_eq!(Preset::from_name(""), Some(Preset::Zephyr));
        assert_eq!(Preset::from_name("gruvbox-dark"), Some(Preset::GruvboxDark));
        assert_eq!(Preset::from_name("unknown"), None);
    }

    #[test]
    fn validate_name_rejects_traversal() {
        assert!(validate_theme_name("../etc/passwd").is_err());
        assert!(validate_theme_name("/absolute").is_err());
        assert!(validate_theme_name("path/sep").is_err());
        assert!(validate_theme_name("path\\sep").is_err());
    }

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_theme_name("").is_ok());
        assert!(validate_theme_name("zephyr").is_ok());
        assert!(validate_theme_name("my-custom-theme").is_ok());
        assert!(validate_theme_name("theme123").is_ok());
    }

    #[test]
    fn resolve_palette_zephyr_default() {
        let p = resolve_palette("").unwrap();
        assert_eq!(p.accent, crate::theme::palette::Rgb(0x1F, 0xB9, 0xA8));
    }

    #[test]
    fn resolve_palette_unsafe_name_error() {
        assert!(resolve_palette("../evil").is_err());
        assert!(resolve_palette("/etc/passwd").is_err());
    }
}
