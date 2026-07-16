// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`MediaSanitizer`] — validation pipeline for MCP-sourced images (spec-072).
//!
//! Mirrors [`crate::ContentSanitizer`]'s policy-object shape: constructed once from
//! [`zeph_config::McpMediaConfig`] and shared across tool calls. Every check in
//! [`MediaSanitizer::sanitize_image`] runs before the image is ever attached to an LLM
//! request, closing the decompression-bomb and MIME-spoofing surface a raw
//! `ContentBlock::Image` passthrough would otherwise expose.

use image::GenericImageView;

/// Reason an MCP-sourced image was rejected by [`MediaSanitizer::sanitize_image`].
///
/// Every variant is logged via the tool audit path (server, tool, mime, bytes, outcome) by
/// the caller — the text placeholder always remains as a fallback (spec-072 FR-005).
#[derive(Debug, Clone, thiserror::Error)]
pub enum MediaRejected {
    /// Encoded byte size exceeds [`zeph_config::McpMediaConfig::max_image_bytes`].
    #[error("image exceeds max size of {max} bytes (got {actual})")]
    SizeExceeded {
        /// Configured cap.
        max: usize,
        /// Actual encoded size.
        actual: usize,
    },
    /// Decoded width/height or total pixel count exceeds the configured caps —
    /// decompression-bomb defense.
    #[error(
        "image dimensions {width}x{height} exceed max_dimension_px={max_dimension} or max_pixels={max_pixels}"
    )]
    DimensionExceeded {
        /// Decoded width in pixels.
        width: u32,
        /// Decoded height in pixels.
        height: u32,
        /// Configured per-axis cap.
        max_dimension: u32,
        /// Configured total-pixel cap.
        max_pixels: u64,
    },
    /// Detected format is not in [`zeph_config::McpMediaConfig::allowed_formats`].
    #[error("image format {detected:?} is not in the allowed format list")]
    FormatNotAllowed {
        /// Format detected from the magic bytes.
        detected: String,
    },
    /// Declared MIME type does not match the format sniffed from the magic bytes.
    #[error("declared MIME type {declared:?} does not match detected format {detected:?}")]
    MimeMismatch {
        /// MIME type the MCP server declared.
        declared: String,
        /// MIME type inferred from the magic bytes.
        detected: String,
    },
    /// Magic-byte sniff or full decode failed.
    #[error("failed to decode image: {0}")]
    DecodeFailed(String),
}

/// Validates and decodes MCP-sourced images before they are attached to an LLM request.
///
/// Constructed once from [`zeph_config::McpMediaConfig`] and cheaply
/// cloneable. Checks run in order: magic-byte sniff vs. declared MIME, format allowlist,
/// byte-size cap, a header-only dimension pre-check (no pixel buffer allocated), then a
/// `spawn_blocking` full decode with the same dimension/pixel caps re-enforced on the
/// decoded image — a byte cap alone cannot bound the decoded pixel count, and checking
/// dimensions from the header first rejects an oversized image before paying for the full
/// decode's memory allocation (decompression-bomb defense-in-depth).
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::MediaSanitizer;
/// use zeph_config::McpMediaConfig;
///
/// let sanitizer = MediaSanitizer::new(&McpMediaConfig::default());
/// // A 1x1 PNG (magic bytes only, minimal fixture).
/// let png_1x1: &[u8] = &[
///     0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
///     0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
///     0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8,
///     0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xC9, 0xFE, 0x92, 0xEF, 0x00, 0x00, 0x00,
///     0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
/// ];
/// let result = tokio::runtime::Builder::new_current_thread()
///     .enable_time()
///     .build()
///     .unwrap()
///     .block_on(async {
///         sanitizer.sanitize_image(png_1x1, "image/png", "test-server").await
///     });
/// assert!(result.is_ok());
/// ```
#[derive(Debug, Clone)]
pub struct MediaSanitizer {
    max_image_bytes: usize,
    max_dimension_px: u32,
    max_pixels: u64,
    allowed_formats: Vec<String>,
}

impl MediaSanitizer {
    /// Build a sanitizer from the given configuration.
    #[must_use]
    pub fn new(config: &zeph_config::McpMediaConfig) -> Self {
        Self {
            max_image_bytes: config.max_image_bytes,
            max_dimension_px: config.max_dimension_px,
            max_pixels: config.max_pixels,
            allowed_formats: config.allowed_formats.clone(),
        }
    }

    /// Validate and decode a single MCP-sourced image.
    ///
    /// Returns [`zeph_llm::provider::ImageData`] on success — the original validated bytes
    /// are carried through unchanged (no re-encode required for v1 correctness). `server_id`
    /// is used only for tracing attribution.
    ///
    /// # Errors
    ///
    /// Returns [`MediaRejected`] when the magic bytes don't match `declared_mime`, the
    /// format isn't allowlisted, the byte size or decoded dimensions/pixel count exceed the
    /// configured caps, or the image fails to decode.
    pub async fn sanitize_image(
        &self,
        bytes: &[u8],
        declared_mime: &str,
        server_id: &str,
    ) -> Result<zeph_llm::provider::ImageData, MediaRejected> {
        let format =
            image::guess_format(bytes).map_err(|e| MediaRejected::DecodeFailed(e.to_string()))?;

        let detected_mime = format_to_mime(format);
        if !declared_mime.eq_ignore_ascii_case(detected_mime) {
            return Err(MediaRejected::MimeMismatch {
                declared: declared_mime.to_owned(),
                detected: detected_mime.to_owned(),
            });
        }

        let short_name = format_short_name(format);
        if !self
            .allowed_formats
            .iter()
            .any(|f| f.eq_ignore_ascii_case(short_name))
        {
            return Err(MediaRejected::FormatNotAllowed {
                detected: short_name.to_owned(),
            });
        }

        if bytes.len() > self.max_image_bytes {
            return Err(MediaRejected::SizeExceeded {
                max: self.max_image_bytes,
                actual: bytes.len(),
            });
        }

        // Decompression-bomb defense: read the dimensions from the header only (no pixel
        // buffer allocated) before ever attempting a full decode, so an image that declares
        // an oversized width/height is rejected without the caller paying for the decode's
        // memory allocation.
        let (header_width, header_height) =
            image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
                .into_dimensions()
                .map_err(|e| MediaRejected::DecodeFailed(e.to_string()))?;
        self.check_dimensions(header_width, header_height)?;

        let owned = bytes.to_vec();
        let (width, height) = tokio::task::spawn_blocking(move || {
            image::load_from_memory_with_format(&owned, format).map(|img| img.dimensions())
        })
        .await
        .map_err(|e| MediaRejected::DecodeFailed(e.to_string()))?
        .map_err(|e| MediaRejected::DecodeFailed(e.to_string()))?;

        self.check_dimensions(width, height)?;

        tracing::debug!(
            server_id,
            mime = declared_mime,
            bytes = bytes.len(),
            width,
            height,
            "MCP media sanitizer: image accepted"
        );

        Ok(zeph_llm::provider::ImageData {
            data: bytes.to_vec(),
            mime_type: declared_mime.to_owned(),
        })
    }

    /// Reject if `width`/`height` exceed the per-axis cap or their product exceeds the total
    /// pixel cap. Shared by the cheap header-only pre-check and the post-full-decode check.
    fn check_dimensions(&self, width: u32, height: u32) -> Result<(), MediaRejected> {
        let pixel_count = u64::from(width) * u64::from(height);
        if width > self.max_dimension_px
            || height > self.max_dimension_px
            || pixel_count > self.max_pixels
        {
            return Err(MediaRejected::DimensionExceeded {
                width,
                height,
                max_dimension: self.max_dimension_px,
                max_pixels: self.max_pixels,
            });
        }
        Ok(())
    }
}

fn format_to_mime(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => "application/octet-stream",
    }
}

fn format_short_name(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpeg",
        image::ImageFormat::Gif => "gif",
        image::ImageFormat::WebP => "webp",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::McpMediaConfig;

    // 1x1 valid PNG fixture (magic bytes + minimal IHDR/IDAT/IEND chunks).
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0xC9, 0xFE, 0x92, 0xEF, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[tokio::test]
    async fn accepts_valid_png() {
        let sanitizer = MediaSanitizer::new(&McpMediaConfig::default());
        let result = sanitizer
            .sanitize_image(PNG_1X1, "image/png", "test-server")
            .await
            .unwrap();
        assert_eq!(result.mime_type, "image/png");
        assert_eq!(result.data, PNG_1X1);
    }

    #[tokio::test]
    async fn rejects_size_exceeded() {
        let config = McpMediaConfig {
            max_image_bytes: 10,
            ..McpMediaConfig::default()
        };
        let sanitizer = MediaSanitizer::new(&config);
        let err = sanitizer
            .sanitize_image(PNG_1X1, "image/png", "test-server")
            .await
            .unwrap_err();
        assert!(matches!(err, MediaRejected::SizeExceeded { .. }));
    }

    #[tokio::test]
    async fn rejects_dimension_exceeded() {
        let config = McpMediaConfig {
            max_dimension_px: 0,
            ..McpMediaConfig::default()
        };
        let sanitizer = MediaSanitizer::new(&config);
        let err = sanitizer
            .sanitize_image(PNG_1X1, "image/png", "test-server")
            .await
            .unwrap_err();
        assert!(matches!(err, MediaRejected::DimensionExceeded { .. }));
    }

    #[tokio::test]
    async fn rejects_pixel_budget_exceeded() {
        let config = McpMediaConfig {
            max_pixels: 0,
            ..McpMediaConfig::default()
        };
        let sanitizer = MediaSanitizer::new(&config);
        let err = sanitizer
            .sanitize_image(PNG_1X1, "image/png", "test-server")
            .await
            .unwrap_err();
        assert!(matches!(err, MediaRejected::DimensionExceeded { .. }));
    }

    #[tokio::test]
    async fn rejects_mime_mismatch() {
        let sanitizer = MediaSanitizer::new(&McpMediaConfig::default());
        let err = sanitizer
            .sanitize_image(PNG_1X1, "image/jpeg", "test-server")
            .await
            .unwrap_err();
        assert!(matches!(err, MediaRejected::MimeMismatch { .. }));
    }

    #[tokio::test]
    async fn rejects_disallowed_format() {
        let config = McpMediaConfig {
            allowed_formats: vec!["jpeg".to_owned()],
            ..McpMediaConfig::default()
        };
        let sanitizer = MediaSanitizer::new(&config);
        let err = sanitizer
            .sanitize_image(PNG_1X1, "image/png", "test-server")
            .await
            .unwrap_err();
        assert!(matches!(err, MediaRejected::FormatNotAllowed { .. }));
    }

    #[tokio::test]
    async fn rejects_malformed_bytes() {
        let sanitizer = MediaSanitizer::new(&McpMediaConfig::default());
        let err = sanitizer
            .sanitize_image(b"not an image", "image/png", "test-server")
            .await
            .unwrap_err();
        assert!(matches!(err, MediaRejected::DecodeFailed(_)));
    }
}
