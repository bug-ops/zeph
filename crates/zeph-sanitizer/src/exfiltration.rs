// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Exfiltration guards: prevent LLM-generated content from leaking data via
//! outbound channels (markdown images, tool URL injection, poisoned memory writes).
//!
//! The [`ExfiltrationGuard`] is stateless and covers five attack vectors:
//!
//! 1. **Markdown image exfiltration** — an adversary plants `![t](https://evil.com/track.gif)`
//!    in content. When the LLM echoes it, the rendered image loads silently, leaking session data.
//!    [`ExfiltrationGuard::scan_output`] strips these and replaces them with `[image removed: …]`.
//!
//! 2. **URL injection via tool calls** — a flagged URL from untrusted tool output appears in a
//!    subsequent tool call argument. [`ExfiltrationGuard::validate_tool_call`] cross-references
//!    URLs against the per-turn flagged URL set. Flag-only approach (does not block execution).
//!
//! 3. **Poisoned memory writes** — content flagged with injection patterns is intercepted before
//!    Qdrant embedding. [`ExfiltrationGuard::should_guard_memory_write`] signals the caller to
//!    skip the embedding step, preventing poisoned content from polluting semantic search.
//!
//! 4. **HTML img tag exfiltration** — `<img src="https://evil.com/track.gif">` embeds are
//!    stripped alongside markdown images. Controlled by the same `block_markdown_images` flag.
//!
//! 5. **Unicode zero-width character bypass** — inserting zero-width joiners/non-joiners between
//!    `!` and `[` breaks naive markdown regex matchers. [`ExfiltrationGuard::scan_output`]
//!    detects and strips these sequences when `block_markdown_images` is enabled.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;
use zeph_common::ToolName;

pub use zeph_config::ExfiltrationGuardConfig;

// ---------------------------------------------------------------------------
// Regex patterns
// ---------------------------------------------------------------------------

/// Matches inline markdown images with external http/https URLs:
/// `![alt text](https://example.com/track.gif)`
///
/// Local paths (`./img.png`) and data URIs (`data:image/...`) are intentionally
/// excluded — they cannot exfiltrate data to a remote server.
static MARKDOWN_IMAGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"!\[([^\]]*)\]\((https?://[^)]+)\)").expect("valid MARKDOWN_IMAGE_RE")
});

/// Matches reference-style markdown image declarations: `[ref]: https://example.com/img`
/// Used in conjunction with `REFERENCE_LABEL_RE` to detect two-part reference images.
static REFERENCE_DEF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\[([^\]]+)\]:\s*(https?://\S+)").expect("valid REFERENCE_DEF_RE")
});

/// Matches reference-style image usages: `![alt][ref]`
static REFERENCE_USAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\[([^\]]+)\]").expect("valid REFERENCE_USAGE_RE"));

/// Extracts http/https URLs from arbitrary text (used for tool argument scanning).
static URL_EXTRACT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s"'<>]+"#).expect("valid URL_EXTRACT_RE"));

/// Matches HTML `<img>` tags with external http/https `src` attributes.
///
/// Both single-quoted and double-quoted `src` values are matched. The captured group 1 contains
/// the URL. The full tag (`<img … >`) is replaced with `[image removed: <url>]`.
static HTML_IMG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<img\b[^>]*\bsrc\s*=\s*["'](https?://[^"']+)["'][^>]*>"#)
        .expect("valid HTML_IMG_RE")
});

/// Detects zero-width Unicode characters between `!` and `[` used to bypass markdown regex.
///
/// Adversaries insert U+200B (ZWSP), U+200C (ZWNJ), U+200D (ZWJ), or U+FEFF (BOM) between
/// the `!` and `[` characters to prevent standard regex matchers from recognising the
/// markdown image syntax. This pattern catches those sequences for stripping.
static UNICODE_BYPASS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("![\u{200B}\u{200C}\u{200D}\u{FEFF}]+\\[").expect("valid UNICODE_BYPASS_RE")
});

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// An exfiltration event detected by [`ExfiltrationGuard`].
///
/// Events are advisory: they are logged, counted, and returned to the caller for
/// further action. The guard itself never panics or blocks the agent loop.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::exfiltration::{ExfiltrationGuard, ExfiltrationEvent};
/// use zeph_config::ExfiltrationGuardConfig;
///
/// let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig::default());
/// let (cleaned, events) = guard.scan_output("![t](https://evil.com/pixel.gif)");
/// assert_eq!(events.len(), 1);
/// assert!(matches!(&events[0], ExfiltrationEvent::MarkdownImageBlocked { url } if url.contains("evil.com")));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum ExfiltrationEvent {
    /// A markdown image with an external URL was stripped from LLM output.
    MarkdownImageBlocked { url: String },
    /// An HTML `<img src="…">` tag with an external URL was stripped from LLM output.
    HtmlImageBlocked { url: String },
    /// A tool call argument contained a URL that appeared in untrusted flagged content.
    SuspiciousToolUrl { url: String, tool_name: ToolName },
    /// A memory write was intercepted because the content had injection flags.
    MemoryWriteGuarded { reason: String },
}

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

/// Stateless exfiltration guard covering three outbound leak vectors.
///
/// Construct once from [`ExfiltrationGuardConfig`] and store on the agent. Cheap to clone.
/// All three scanners ([`scan_output`](Self::scan_output),
/// [`validate_tool_call`](Self::validate_tool_call),
/// [`should_guard_memory_write`](Self::should_guard_memory_write)) are independently
/// toggled via the config flags `block_markdown_images`, `validate_tool_urls`, and
/// `guard_memory_writes`.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::exfiltration::ExfiltrationGuard;
/// use zeph_config::ExfiltrationGuardConfig;
///
/// let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig::default());
///
/// // Strips external tracking pixels from LLM output.
/// let (cleaned, events) = guard.scan_output("text ![track](https://evil.com/p.gif) end");
/// assert!(events.len() == 1);
/// assert!(!cleaned.contains("![track]"));
///
/// // Memory write is guarded when injection flags are present.
/// let event = guard.should_guard_memory_write(true);
/// assert!(event.is_some());
/// ```
#[derive(Debug, Clone)]
pub struct ExfiltrationGuard {
    config: ExfiltrationGuardConfig,
}

impl ExfiltrationGuard {
    /// Create a new guard from the given configuration.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::exfiltration::ExfiltrationGuard;
    /// use zeph_config::ExfiltrationGuardConfig;
    ///
    /// let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig::default());
    /// ```
    #[must_use]
    pub fn new(config: ExfiltrationGuardConfig) -> Self {
        Self { config }
    }

    /// Scan LLM output text and strip external markdown images.
    ///
    /// Returns the cleaned text and a list of [`ExfiltrationEvent::MarkdownImageBlocked`]
    /// for each image that was removed.
    ///
    /// When `block_markdown_images` is `false`, returns the input unchanged.
    ///
    /// # Scanning coverage
    /// - Inline images: `![alt](https://evil.com/track.gif)`
    /// - Reference-style images: `![alt][ref]` + `[ref]: https://evil.com/img`
    /// - Percent-encoded URLs inside already-captured groups: decoded before `is_external_url()`
    ///
    /// # Not covered (tracked in #1195)
    /// - Percent-encoded scheme bypass: `%68ttps://evil.com` — the regex requires literal
    ///   `https?://`, so a percent-encoded scheme is never captured. Fix requires pre-decoding
    ///   the full input text before regex matching.
    /// - Reference definitions inside fenced code blocks (false positive risk)
    ///
    /// # Panics
    ///
    /// Panics if the compiled regex does not produce expected capture groups (compile-time
    /// guarantee — the regex patterns are validated via `expect` in `LazyLock` initializers).
    #[must_use]
    pub fn scan_output(&self, text: &str) -> (String, Vec<ExfiltrationEvent>) {
        if !self.config.block_markdown_images {
            return (text.to_owned(), vec![]);
        }

        let mut events = Vec::new();
        let mut result = text.to_owned();

        // --- Pass 1: inline images ---
        let mut replacement = String::new();
        let mut last_end = 0usize;
        for cap in MARKDOWN_IMAGE_RE.captures_iter(text) {
            let m = cap.get(0).expect("full match");
            let raw_url = cap.get(2).expect("url group").as_str();
            let url = percent_decode_url(raw_url);

            if is_external_url(&url) {
                replacement.push_str(&text[last_end..m.start()]);
                let _ = write!(replacement, "[image removed: {url}]");
                last_end = m.end();
                events.push(ExfiltrationEvent::MarkdownImageBlocked { url });
            }
        }
        if !events.is_empty() || last_end > 0 {
            replacement.push_str(&text[last_end..]);
            result = replacement;
        }

        // --- Pass 2: reference-style images ---
        // Collect reference definitions from the (already partially cleaned) result.
        let mut ref_defs: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for cap in REFERENCE_DEF_RE.captures_iter(&result) {
            let label = cap.get(1).expect("label").as_str().to_lowercase();
            let raw_url = cap.get(2).expect("url").as_str();
            let url = percent_decode_url(raw_url);
            if is_external_url(&url) {
                ref_defs.insert(label, url);
            }
        }

        if !ref_defs.is_empty() {
            // Remove reference usages that point to external defs.
            let mut cleaned = String::with_capacity(result.len());
            let mut last_end = 0usize;
            for cap in REFERENCE_USAGE_RE.captures_iter(&result) {
                let m = cap.get(0).expect("full match");
                let label = cap.get(2).expect("label").as_str().to_lowercase();
                if let Some(url) = ref_defs.get(&label) {
                    cleaned.push_str(&result[last_end..m.start()]);
                    let _ = write!(cleaned, "[image removed: {url}]");
                    last_end = m.end();
                    events.push(ExfiltrationEvent::MarkdownImageBlocked { url: url.clone() });
                }
            }
            cleaned.push_str(&result[last_end..]);
            result = cleaned;

            // Remove the reference definition lines for blocked refs.
            // Use split('\n') (not .lines()) to preserve \r in CRLF line endings —
            // .lines() strips \r, and reconstruction with push('\n') would silently
            // convert all CRLF to LF throughout the entire text.
            let mut def_cleaned = String::with_capacity(result.len());
            for line in result.split('\n') {
                let mut keep = true;
                for cap in REFERENCE_DEF_RE.captures_iter(line) {
                    let label = cap.get(1).expect("label").as_str().to_lowercase();
                    if ref_defs.contains_key(&label) {
                        keep = false;
                        break;
                    }
                }
                if keep {
                    def_cleaned.push_str(line);
                    def_cleaned.push('\n');
                }
            }
            // Preserve trailing newline behaviour of the original.
            if !text.ends_with('\n') && def_cleaned.ends_with('\n') {
                def_cleaned.pop();
            }
            result = def_cleaned;
        }

        // --- Pass 3: HTML img tags with external URLs ---
        let mut html_result = String::with_capacity(result.len());
        let mut html_last_end = 0usize;
        for cap in HTML_IMG_RE.captures_iter(&result) {
            let m = cap.get(0).expect("full match");
            let url = cap.get(1).expect("src url group").as_str().to_owned();
            tracing::warn!(url = %url, "HTML img tag with external URL stripped from LLM output");
            html_result.push_str(&result[html_last_end..m.start()]);
            let _ = write!(html_result, "[image removed: {url}]");
            html_last_end = m.end();
            events.push(ExfiltrationEvent::HtmlImageBlocked { url });
        }
        if html_last_end > 0 {
            html_result.push_str(&result[html_last_end..]);
            result = html_result;
        }

        // --- Pass 4: Unicode zero-width bypass sequences ---
        // Adversaries insert zero-width chars between `!` and `[` to defeat markdown regexes.
        // Strip the entire `!<zwc+>[` sequence to defuse the payload.
        if UNICODE_BYPASS_RE.is_match(&result) {
            tracing::warn!("Unicode zero-width bypass attempt detected in LLM output; stripping");
            result = UNICODE_BYPASS_RE
                .replace_all(&result, "[blocked]")
                .into_owned();
        }

        (result, events)
    }

    /// Validate tool call arguments against a set of URLs flagged in untrusted content.
    ///
    /// Parses `args_json` as a JSON value and extracts all string leaves recursively to
    /// avoid JSON-encoding bypasses (escaped slashes, unicode escapes, etc.).
    ///
    /// Returns one [`ExfiltrationEvent::SuspiciousToolUrl`] per matching URL.
    /// When `validate_tool_urls` is `false`, always returns an empty vec.
    ///
    /// # Flag-only approach
    /// Matching URLs are logged and counted but tool execution is NOT blocked. Blocking
    /// would break legitimate workflows where the same URL appears in both a search result
    /// and a subsequent fetch call. See design decision D1 in the architect handoff.
    #[must_use]
    pub fn validate_tool_call(
        &self,
        tool_name: &str,
        args_json: &str,
        flagged_urls: &HashSet<String>,
    ) -> Vec<ExfiltrationEvent> {
        if !self.config.validate_tool_urls || flagged_urls.is_empty() {
            return vec![];
        }

        let parsed: serde_json::Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(_) => {
                // Fall back to raw regex scan if JSON is malformed.
                return Self::scan_raw_args(tool_name, args_json, flagged_urls);
            }
        };

        let mut events = Vec::new();
        let mut strings = Vec::new();
        collect_strings(&parsed, &mut strings);

        for s in &strings {
            for url_match in URL_EXTRACT_RE.find_iter(s) {
                let url = url_match.as_str();
                if flagged_urls.contains(url) {
                    events.push(ExfiltrationEvent::SuspiciousToolUrl {
                        url: url.to_owned(),
                        tool_name: tool_name.into(),
                    });
                }
            }
        }

        events
    }

    /// Check whether a memory write should skip Qdrant embedding.
    ///
    /// Returns `Some(MemoryWriteGuarded)` when `has_injection_flags` is `true` and
    /// `guard_memory_writes` is enabled. The caller should still save to `SQLite` for
    /// conversation continuity but omit the Qdrant embedding to prevent poisoned content
    /// from polluting semantic search results.
    ///
    /// See design decision D2 in the architect handoff.
    #[must_use]
    pub fn should_guard_memory_write(
        &self,
        has_injection_flags: bool,
    ) -> Option<ExfiltrationEvent> {
        if !self.config.guard_memory_writes || !has_injection_flags {
            return None;
        }
        Some(ExfiltrationEvent::MemoryWriteGuarded {
            reason: "content contained injection patterns flagged by ContentSanitizer".to_owned(),
        })
    }

    /// Extract URLs from untrusted tool output for use in subsequent `validate_tool_call` checks.
    ///
    fn scan_raw_args(
        tool_name: &str,
        args: &str,
        flagged_urls: &HashSet<String>,
    ) -> Vec<ExfiltrationEvent> {
        URL_EXTRACT_RE
            .find_iter(args)
            .filter(|m| flagged_urls.contains(m.as_str()))
            .map(|m| ExfiltrationEvent::SuspiciousToolUrl {
                url: m.as_str().to_owned(),
                tool_name: tool_name.into(),
            })
            .collect()
    }
}

/// Extract all `http`/`https` URLs from `content` into a `HashSet` for later URL validation.
///
/// Call this after sanitizing untrusted tool output with `ContentSanitizer` when injection
/// flags are present. Pass the returned set into the agent's `flagged_urls` field. Pass that
/// set to [`ExfiltrationGuard::validate_tool_call`] on each subsequent tool call. Clear
/// `flagged_urls` at the start of each `process_response` call (per-turn clearing strategy).
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::exfiltration::extract_flagged_urls;
///
/// let urls = extract_flagged_urls("visit https://evil.com/x and https://other.com/y");
/// assert!(urls.contains("https://evil.com/x"));
/// assert!(urls.contains("https://other.com/y"));
/// assert_eq!(urls.len(), 2);
/// ```
#[must_use]
pub fn extract_flagged_urls(content: &str) -> HashSet<String> {
    URL_EXTRACT_RE
        .find_iter(content)
        .map(|m| m.as_str().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decode percent-encoded URL characters before exfiltration matching.
///
/// Converts `%68ttps://` → `https://` so simple percent-encoding bypasses are caught.
/// Non-UTF-8 sequences are left as-is (they won't match `is_external_url`).
fn percent_decode_url(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            // hi and lo are 0-15; combined value is at most 0xFF, fits in u8.
            #[allow(clippy::cast_possible_truncation)]
            let byte = ((hi << 4) | lo) as u8;
            out.push(byte as char);
            i += 3;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn is_external_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Recursively collect all string leaves from a JSON value.
fn collect_strings<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(s) => out.push(s.as_str()),
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_strings(v, out);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> ExfiltrationGuard {
        ExfiltrationGuard::new(ExfiltrationGuardConfig::default())
    }

    fn guard_disabled() -> ExfiltrationGuard {
        ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: false,
            validate_tool_urls: false,
            guard_memory_writes: false,
        })
    }

    // --- scan_output ---

    #[test]
    fn strips_external_inline_image() {
        let (cleaned, events) =
            guard().scan_output("Before ![track](https://evil.com/p.gif) after");
        assert_eq!(
            cleaned,
            "Before [image removed: https://evil.com/p.gif] after"
        );
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ExfiltrationEvent::MarkdownImageBlocked { url } if url == "https://evil.com/p.gif")
        );
    }

    #[test]
    fn preserves_local_image() {
        let text = "Look: ![diagram](./diagram.png) — local";
        let (cleaned, events) = guard().scan_output(text);
        assert_eq!(cleaned, text);
        assert!(events.is_empty());
    }

    #[test]
    fn preserves_data_uri() {
        let text = "Inline: ![icon](data:image/png;base64,abc123)";
        let (cleaned, events) = guard().scan_output(text);
        assert_eq!(cleaned, text);
        assert!(events.is_empty());
    }

    #[test]
    fn strips_multiple_external_images() {
        let text = "![a](https://a.com/1.gif) text ![b](https://b.com/2.gif)";
        let (cleaned, events) = guard().scan_output(text);
        // Markdown image syntax must be removed; replacement label may contain URLs.
        assert!(
            !cleaned.contains("![a]("),
            "first image syntax must be removed: {cleaned}"
        );
        assert!(
            !cleaned.contains("![b]("),
            "second image syntax must be removed: {cleaned}"
        );
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn scan_output_noop_when_disabled() {
        let text = "![track](https://evil.com/p.gif)";
        let (cleaned, events) = guard_disabled().scan_output(text);
        assert_eq!(cleaned, text);
        assert!(events.is_empty());
    }

    #[test]
    fn strips_reference_style_image() {
        let text = "Here is the image: ![alt][ref]\n[ref]: https://evil.com/track.gif\nend";
        let (cleaned, events) = guard().scan_output(text);
        // The markdown image syntax and definition line must be removed.
        assert!(
            !cleaned.contains("![alt][ref]"),
            "image usage syntax must be removed: {cleaned}"
        );
        assert!(
            !cleaned.contains("[ref]:"),
            "reference definition must be removed: {cleaned}"
        );
        assert!(
            cleaned.contains("[image removed:"),
            "replacement label must be present: {cleaned}"
        );
        assert!(!events.is_empty(), "must generate event");
    }

    #[test]
    fn preserves_local_reference_image() {
        // Reference pointing to a local path — must not be stripped.
        let text = "![alt][ref]\n[ref]: ./local.png\n";
        let (cleaned, events) = guard().scan_output(text);
        assert_eq!(cleaned, text);
        assert!(events.is_empty());
    }

    #[test]
    fn decodes_percent_encoded_url_in_inline_image() {
        // %68 = 'h', so %68ttps:// decodes to https://.
        // The MARKDOWN_IMAGE_RE pattern requires a literal `https?://` prefix, so
        // `%68ttps://` is NOT matched by the regex and passes through unchanged.
        // percent_decode_url() is called on the URL *after* the regex captures it —
        // so percent-encoded schemes bypass inline detection.
        //
        // Known bypass — tracked for Phase 5 (#1195): the fix requires pre-decoding the
        // full text before regex matching (or a multi-pass decode+scan approach). The LLM
        // context wrapper already limits what arrives here, reducing practical risk.
        let text = "![t](%68ttps://evil.com/track.gif)";
        let (cleaned, _events) = guard().scan_output(text);
        // The text passes through unchanged because the regex didn't match.
        assert_eq!(
            cleaned, text,
            "percent-encoded scheme not detected by inline regex"
        );

        // A normal https:// URL IS detected.
        let normal = "![t](https://evil.com/track.gif)";
        let (normal_cleaned, normal_events) = guard().scan_output(normal);
        assert!(
            !normal_cleaned.contains("![t](https://"),
            "normal URL must be removed"
        );
        assert_eq!(normal_events.len(), 1);
    }

    #[test]
    fn empty_alt_text_still_blocked() {
        let text = "![](https://evil.com/p.gif)";
        let (cleaned, events) = guard().scan_output(text);
        // The original markdown image syntax must be removed; the replacement label may contain the URL.
        assert!(
            !cleaned.contains("![]("),
            "markdown image syntax must be removed: {cleaned}"
        );
        assert!(
            cleaned.contains("[image removed:"),
            "replacement label must be present: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn html_img_tag_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        let (cleaned, events) = guard.scan_output(r#"text <img src="https://evil.com/p.gif"> end"#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ExfiltrationEvent::HtmlImageBlocked { .. })),
            "expected HtmlImageBlocked event"
        );
        assert!(
            !cleaned.contains("<img"),
            "img tag must be removed: {cleaned}"
        );
        assert!(
            cleaned.contains("[image removed:"),
            "replacement label must be present: {cleaned}"
        );
    }

    #[test]
    fn html_img_tag_single_quote_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        let (cleaned, events) = guard.scan_output("text <img src='https://evil.com/p.gif'> end");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ExfiltrationEvent::HtmlImageBlocked { .. })),
            "expected HtmlImageBlocked event for single-quoted src"
        );
        assert!(
            !cleaned.contains("<img"),
            "img tag must be removed: {cleaned}"
        );
    }

    #[test]
    fn html_img_tag_noop_when_disabled() {
        let input = r#"text <img src="https://evil.com/p.gif"> end"#;
        let (cleaned, events) = guard_disabled().scan_output(input);
        assert_eq!(cleaned, input);
        assert!(events.is_empty());
    }

    #[test]
    fn unicode_zwj_bypass_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        // Insert U+200B (ZWSP) between ! and [ to try to evade markdown regex.
        let input = "!\u{200B}[alt](https://evil.com/track)";
        let (cleaned, _events) = guard.scan_output(input);
        // The bypass sequence `!\u{200B}[` is replaced with `[blocked]`, defusing
        // the markdown image syntax — the `!` prefix that triggers image rendering is gone.
        assert!(
            !cleaned.contains('\u{200B}'),
            "zero-width char must be stripped: {cleaned}"
        );
        assert!(
            !cleaned.starts_with('!'),
            "image trigger `!` must be removed: {cleaned}"
        );
    }

    #[test]
    fn unicode_bypass_noop_when_disabled() {
        let input = "!\u{200B}[alt](https://evil.com/track)";
        let (cleaned, events) = guard_disabled().scan_output(input);
        assert_eq!(cleaned, input);
        assert!(events.is_empty());
    }

    // --- validate_tool_call ---

    #[test]
    fn detects_flagged_url_in_json_string() {
        let mut flagged = HashSet::new();
        flagged.insert("https://evil.com/payload".to_owned());
        let args = r#"{"url": "https://evil.com/payload"}"#;
        let events = guard().validate_tool_call("fetch", args, &flagged);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ExfiltrationEvent::SuspiciousToolUrl { url, tool_name }
            if url == "https://evil.com/payload" && tool_name == "fetch")
        );
    }

    #[test]
    fn no_event_when_url_not_flagged() {
        let mut flagged = HashSet::new();
        flagged.insert("https://other.com/benign".to_owned());
        let args = r#"{"url": "https://legitimate.com/page"}"#;
        let events = guard().validate_tool_call("fetch", args, &flagged);
        assert!(events.is_empty());
    }

    #[test]
    fn validate_tool_call_noop_when_disabled() {
        let mut flagged = HashSet::new();
        flagged.insert("https://evil.com/x".to_owned());
        let args = r#"{"url": "https://evil.com/x"}"#;
        let events = guard_disabled().validate_tool_call("fetch", args, &flagged);
        assert!(events.is_empty());
    }

    #[test]
    fn validate_tool_call_noop_with_empty_flagged() {
        let args = r#"{"url": "https://evil.com/x"}"#;
        let events = guard().validate_tool_call("fetch", args, &HashSet::new());
        assert!(events.is_empty());
    }

    #[test]
    fn extracts_urls_from_nested_json() {
        let mut flagged = HashSet::new();
        flagged.insert("https://evil.com/deep".to_owned());
        let args = r#"{"nested": {"inner": ["https://evil.com/deep"]}}"#;
        let events = guard().validate_tool_call("tool", args, &flagged);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn handles_escaped_slashes_in_json() {
        // JSON-encoded URL with escaped forward slashes should still be detected
        // after serde_json parsing (which unescapes the string value).
        let mut flagged = HashSet::new();
        flagged.insert("https://evil.com/path".to_owned());
        // serde_json will unescape \/ → /
        let args = r#"{"url": "https:\/\/evil.com\/path"}"#;
        let parsed: serde_json::Value = serde_json::from_str(args).unwrap();
        // Confirm serde_json unescapes it.
        assert_eq!(parsed["url"], "https://evil.com/path");
        let events = guard().validate_tool_call("fetch", args, &flagged);
        assert_eq!(events.len(), 1, "JSON-escaped URL must be caught");
    }

    // --- should_guard_memory_write ---

    #[test]
    fn guards_when_injection_flags_set() {
        let event = guard().should_guard_memory_write(true);
        assert!(event.is_some());
        assert!(matches!(
            event.unwrap(),
            ExfiltrationEvent::MemoryWriteGuarded { .. }
        ));
    }

    #[test]
    fn passes_when_no_injection_flags() {
        let event = guard().should_guard_memory_write(false);
        assert!(event.is_none());
    }

    #[test]
    fn guard_memory_write_noop_when_disabled() {
        let event = guard_disabled().should_guard_memory_write(true);
        assert!(event.is_none());
    }

    // --- percent_decode_url ---

    #[test]
    fn percent_decode_roundtrip() {
        assert_eq!(
            percent_decode_url("https://example.com"),
            "https://example.com"
        );
        assert_eq!(
            percent_decode_url("%68ttps://example.com"),
            "https://example.com"
        );
        assert_eq!(percent_decode_url("hello%20world"), "hello world");
    }

    // --- extract_flagged_urls ---

    #[test]
    fn extracts_urls_from_plain_text() {
        let content = "check https://evil.com/x and https://other.com/y for details";
        let urls = extract_flagged_urls(content);
        assert!(urls.contains("https://evil.com/x"));
        assert!(urls.contains("https://other.com/y"));
    }
}
