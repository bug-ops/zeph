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
/// Both `zeph-mcp` and `zeph-core::sanitizer` compile their own `regex::Regex` instances
/// from this slice. Do not export a compiled `LazyLock` — let each consumer own its state.
pub const RAW_INJECTION_PATTERNS: &[(&str, &str)] = &[
    (
        "ignore_instructions",
        r"(?i)ignore\s+(all\s+|any\s+|previous\s+|prior\s+)?instructions",
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
    ("act_as_if", r"(?i)act\s+as\s+if"),
    ("html_image_exfil", r"(?i)<img\s+[^>]*src\s*="),
    ("delimiter_escape_tool_output", r"(?i)</?tool-output[\s>]"),
    (
        "delimiter_escape_external_data",
        r"(?i)</?external-data[\s>]",
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

/// Strip Unicode format (Cf) characters and ASCII control characters (except tab/newline)
/// from `text` before injection pattern matching.
///
/// These characters are invisible to humans but can break regex word boundaries,
/// allowing attackers to smuggle injection keywords through zero-width joiners,
/// soft hyphens, BOM, etc.
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
