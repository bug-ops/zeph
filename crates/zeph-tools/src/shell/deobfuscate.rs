// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shell command deobfuscation for pre-blocklist normalization.
//!
//! Transforms common obfuscation techniques (hex/octal escapes, subshell expansion,
//! variable references, quote-based concatenation) into readable equivalents before
//! the command is evaluated by the blocklist and permission policy.
//!
//! # Limitations
//!
//! Single-pass: subshell content (`$(...)`, `` `...` ``) is replaced with a
//! `[subshell: ...]` placeholder but NOT re-scanned. The blocklist independently
//! rejects `$(` and `` ` `` metacharacters, so nested constructs are caught at
//! that layer rather than here.

use std::sync::LazyLock;

use regex::Regex;
use tracing;

/// Maximum input length processed by the deobfuscator.
///
/// Commands longer than this are silently truncated before normalization.
const MAX_INPUT_BYTES: usize = 8192;

static RE_HEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\x([0-9a-fA-F]{2})").expect("RE_HEX"));
static RE_OCT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\([0-7]{1,3})").expect("RE_OCT"));
static RE_UNI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\u([0-9a-fA-F]{4})").expect("RE_UNI"));
static RE_VAR_BRACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("RE_VAR_BRACE"));
static RE_VAR_PLAIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$([A-Za-z_][A-Za-z0-9_]*)").expect("RE_VAR_PLAIN"));
static RE_BACKTICK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`([^`]*)`").expect("RE_BACKTICK"));
static RE_SPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("RE_SPACE"));

/// Normalize an obfuscated shell command string for blocklist and policy evaluation.
///
/// Applies transformations in order:
/// 1. Truncate to 8 KiB.
/// 2. Decode `\xNN` hex escapes.
/// 3. Decode `\NNN` octal escapes.
/// 4. Decode `\uNNNN` Unicode escapes.
/// 5. Collapse backslash line-continuations (`\↵`).
/// 6. Expand `${VAR}` / `$VAR` to `[var:VAR]`.
/// 7. Replace backtick subshells `` `cmd` `` with `[subshell: cmd]`.
/// 8. Replace `$(cmd)` with `[subshell: cmd]`.
/// 9. Strip unescaped quotes used for string concatenation.
/// 10. Normalize runs of whitespace to a single space and trim.
///
/// # Examples
///
/// ```
/// use zeph_tools::shell::deobfuscate::deobfuscate;
///
/// assert_eq!(deobfuscate(r"\x63url"), "curl");
/// assert_eq!(deobfuscate(r"\143at"), "cat");
/// assert_eq!(deobfuscate("$(whoami)"), "[subshell: whoami]");
/// assert_eq!(deobfuscate("${HOME}/file"), "[var:HOME]/file");
/// ```
#[must_use]
pub fn deobfuscate(command: &str) -> String {
    let _span = tracing::info_span!("tools.deobfuscate.normalize").entered();
    // Step 1: truncate at a valid UTF-8 boundary.
    let input = if command.len() > MAX_INPUT_BYTES {
        let boundary = command.floor_char_boundary(MAX_INPUT_BYTES);
        &command[..boundary]
    } else {
        command
    };

    // Step 2: hex escapes \xNN.
    // After decoding, any newly-introduced backslash (e.g. from \x5c → `\`) is replaced with
    // a safe placeholder to prevent it from becoming an octet or unicode escape prefix in step 3/4.
    // This is a single-pass design: multi-stage decoding (encode a backslash as \x5c to enable
    // a subsequent octal decode) is NOT supported and would be a bypass vector if not handled here.
    let s = RE_HEX.replace_all(input, |caps: &regex::Captures<'_>| {
        u8::from_str_radix(&caps[1], 16)
            .ok()
            .filter(u8::is_ascii)
            .map_or_else(
                || caps[0].to_owned(),
                |b| {
                    // Replace decoded backslash with a safe token to prevent cascading decode.
                    if b == b'\\' {
                        "[bs]".to_owned()
                    } else {
                        (b as char).to_string()
                    }
                },
            )
    });

    // Step 3: octal escapes \NNN — only decode printable ASCII (0x20–0x7E).
    // NUL and control characters (0x00–0x1F) are left unchanged to prevent NUL-truncation
    // attacks where the blocklist sees a different string than the OS executes.
    let s = RE_OCT.replace_all(&s, |caps: &regex::Captures<'_>| {
        u8::from_str_radix(&caps[1], 8)
            .ok()
            .filter(|&b| (0x20u8..=0x7E).contains(&b))
            .map_or_else(|| caps[0].to_owned(), |b| (b as char).to_string())
    });

    // Step 4: unicode escapes \uNNNN.
    let s = RE_UNI.replace_all(&s, |caps: &regex::Captures<'_>| {
        u32::from_str_radix(&caps[1], 16)
            .ok()
            .and_then(char::from_u32)
            .map_or_else(|| caps[0].to_owned(), |c| c.to_string())
    });

    // Step 5: backslash line continuations — join lines without inserting space.
    let s = s.replace("\\\n", "");

    // Step 6: variable expansion — ${VAR} before $VAR to avoid partial match.
    let s = RE_VAR_BRACE.replace_all(&s, "[var:$1]");
    let s = RE_VAR_PLAIN.replace_all(&s, "[var:$1]");

    // Step 7: backtick subshells.
    let s = RE_BACKTICK.replace_all(&s, "[subshell: $1]");

    // Step 8: $(...) subshells — depth-aware to handle nesting.
    let s = replace_dollar_subshells(&s);

    // Step 9: strip quotes used for concatenation only.
    let s = strip_concatenation_quotes(&s);

    // Step 10: normalize whitespace.
    RE_SPACE.replace_all(&s, " ").trim().to_owned()
}

/// Replace `$(...)` patterns with `[subshell: ...]`.
///
/// Uses a manual depth-tracking scan rather than regex to handle balanced
/// parentheses in nested constructs like `$(echo $(whoami))`.
fn replace_dollar_subshells(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            let start = i + 2;
            let mut depth = 1usize;
            let mut j = start;
            while j < bytes.len() && depth > 0 {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            let end = j.saturating_sub(1).min(s.len());
            let inner = s[start..end].trim();
            out.push_str("[subshell: ");
            out.push_str(inner);
            out.push(']');
            i = j;
        } else {
            // Advance by one full UTF-8 char to avoid splitting multi-byte sequences.
            let ch = s[i..].chars().next().unwrap_or('\0');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Remove unescaped single and double quotes used as concatenation delimiters.
///
/// Preserves the quoted content; removes the surrounding quote characters.
/// Example: `'cu'"rl"` → `curl`.
fn strip_concatenation_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    out.push(inner);
                }
            }
            '"' => {
                while let Some(inner) = chars.next() {
                    if inner == '"' {
                        break;
                    }
                    if inner == '\\' {
                        if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                    } else {
                        out.push(inner);
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_escape_decoded() {
        assert_eq!(deobfuscate(r"\x63url"), "curl");
        assert_eq!(deobfuscate(r"\x41\x42\x43"), "ABC");
    }

    #[test]
    fn octal_escape_decoded() {
        assert_eq!(deobfuscate(r"\143at"), "cat");
        assert_eq!(deobfuscate(r"\101"), "A");
    }

    #[test]
    fn unicode_escape_decoded() {
        // c = 'c'
        assert_eq!(deobfuscate(r"curl"), "curl");
    }

    #[test]
    fn variable_expansion_brace() {
        assert_eq!(deobfuscate("${HOME}/file"), "[var:HOME]/file");
    }

    #[test]
    fn variable_expansion_plain() {
        // After brace expansion, bare $VAR is replaced.
        assert_eq!(deobfuscate("echo $PATH"), "echo [var:PATH]");
    }

    #[test]
    fn backtick_subshell() {
        assert_eq!(deobfuscate("`whoami`"), "[subshell: whoami]");
    }

    #[test]
    fn dollar_subshell_simple() {
        assert_eq!(deobfuscate("$(whoami)"), "[subshell: whoami]");
    }

    #[test]
    fn quote_concatenation_collapse() {
        assert_eq!(deobfuscate("'cu'\"rl\""), "curl");
        assert_eq!(deobfuscate("'ab'\"cd\"'ef'"), "abcdef");
    }

    #[test]
    fn line_continuation() {
        assert_eq!(deobfuscate("cu\\\nrl"), "curl");
    }

    #[test]
    fn whitespace_normalized() {
        assert_eq!(deobfuscate("echo   hello"), "echo hello");
        assert_eq!(deobfuscate("  ls  "), "ls");
    }

    #[test]
    fn long_input_truncated() {
        let long = "a".repeat(MAX_INPUT_BYTES + 100);
        let result = deobfuscate(&long);
        assert!(result.len() <= MAX_INPUT_BYTES);
    }
}
