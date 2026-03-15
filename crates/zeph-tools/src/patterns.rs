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
//! and homoglyph substitution. [`strip_format_chars`] mitigates Unicode Cf-category bypass
//! but does not handle homoglyphs. This scanner is **advisory and defense-in-depth only**,
//! not a security boundary. The trust gate (tool blocking via `TrustGateExecutor`) is the
//! primary enforcement mechanism.

/// Raw (name, regex pattern) pairs for prompt-injection detection.
///
/// Covers common English-language techniques from OWASP LLM Top 10, Unicode bypass
/// vectors (handled upstream by [`strip_format_chars`]), exfiltration channels
/// (markdown/HTML images), and delimiter-escape attempts against Zeph's own wrapper tags.
///
/// Both `zeph-mcp` and `zeph-core::sanitizer` compile their own [`regex::Regex`] instances
/// from this slice. Do not export a compiled `LazyLock` — let each consumer own its state.
pub const RAW_INJECTION_PATTERNS: &[(&str, &str)] = &[
    (
        "ignore_instructions",
        r"(?i)ignore\s+(all\s+|any\s+|previous\s+|prior\s+)?instructions",
    ),
    ("role_override", r"(?i)you\s+are\s+now"),
    (
        "new_directive",
        r"(?i)new\s+(instructions?|directives?|roles?|personas?)",
    ),
    ("developer_mode", r"(?i)developer\s+mode"),
    ("system_prompt_leak", r"(?i)system\s+prompt"),
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
    ("act_as_if", r"(?i)act\s+as\s+if"),
    ("html_image_exfil", r"(?i)<img\s+[^>]*src\s*="),
    ("delimiter_escape_tool_output", r"(?i)</?tool-output[\s>]"),
    (
        "delimiter_escape_external_data",
        r"(?i)</?external-data[\s>]",
    ),
];

/// Strip Unicode format (Cf) characters and ASCII control characters (except tab/newline)
/// from `text` before injection pattern matching.
///
/// These characters are invisible to humans but can break regex word boundaries,
/// allowing attackers to smuggle injection keywords through zero-width joiners,
/// soft hyphens, BOM, etc.
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
            !matches!(
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
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_format_chars_removes_zero_width_space() {
        let input = "ig\u{200B}nore instructions";
        let result = strip_format_chars(input);
        assert!(!result.contains('\u{200B}'));
        assert!(result.contains("ignore"));
    }

    #[test]
    fn strip_format_chars_preserves_tab_and_newline() {
        let input = "line1\nline2\ttabbed";
        let result = strip_format_chars(input);
        assert!(result.contains('\n'));
        assert!(result.contains('\t'));
    }

    #[test]
    fn strip_format_chars_removes_bom() {
        let input = "\u{FEFF}hello world";
        let result = strip_format_chars(input);
        assert!(!result.contains('\u{FEFF}'));
        assert!(result.contains("hello world"));
    }

    #[test]
    fn strip_format_chars_removes_ascii_control() {
        let input = "hello\x01\x02world";
        let result = strip_format_chars(input);
        assert!(!result.contains('\x01'));
        assert!(result.contains("hello"));
        assert!(result.contains("world"));
    }

    #[test]
    fn raw_injection_patterns_non_empty() {
        assert!(!RAW_INJECTION_PATTERNS.is_empty());
    }

    #[test]
    fn raw_injection_patterns_all_compile() {
        use regex::Regex;
        for (name, pattern) in RAW_INJECTION_PATTERNS {
            assert!(
                Regex::new(pattern).is_ok(),
                "pattern '{name}' failed to compile"
            );
        }
    }
}
