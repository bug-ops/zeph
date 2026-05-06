// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Streaming partial-JSON parser for speculative tool-call dispatch.
//!
//! The Anthropic SSE tool-use stream emits a single JSON object across many
//! `input_json_delta` events. Standard JSON parsers (including
//! `serde_json::StreamDeserializer`) wait for the closing brace before yielding
//! any value — providing no benefit for speculative dispatch.
//!
//! `PartialJsonParser` is a ~120-line brace/string/escape state machine that
//! accumulates delta strings and extracts top-level leaf keys whose values have
//! been **fully closed** (primitives, fully closed nested objects/arrays).
//! When all required fields of a tool's `input_schema` are present, the engine
//! can speculatively dispatch the tool call without waiting for `ToolUseStop`.
//!
//! ## Invariants
//!
//! - **Escape state**: the escape flag is set after a literal `\` inside a string
//!   and cleared after the following character, regardless of what that character is.
//!   Multi-byte escape sequences (e.g. `\uXXXX`) are not individually validated;
//!   the parser only tracks structural JSON tokens.
//! - **Mid-array edge case**: array values at depth > 1 are treated as opaque; their
//!   contents are never surfaced as `known_leaves`. Only top-level keys (depth == 1)
//!   whose values close cleanly are included.
//! - **No allocation on malformed input**: `Malformed` is returned immediately when
//!   a structural invariant is violated; the buffer is not reallocated.

#![allow(dead_code)]

use serde_json::Map;

/// Result of feeding accumulated JSON delta bytes to [`PartialJsonParser::push`].
#[derive(Debug, Clone, PartialEq)]
pub enum PrefixState {
    /// Input is still inside an unterminated string, imbalanced braces, or ends mid-escape.
    Incomplete,
    /// A valid prefix has been parsed: the `known_leaves` map contains fully closed top-level
    /// key-value pairs. `missing_required` lists required schema keys not yet seen.
    ValidPrefix {
        /// Top-level keys whose values are fully closed.
        known_leaves: Map<String, serde_json::Value>,
        /// Required tool input schema keys not yet present in the buffer.
        missing_required: Vec<String>,
    },
    /// The buffer contains a sequence that cannot be a valid JSON object prefix.
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    TopObject,
    InKey,
    AfterKey,
    InValue,
    InStringValue,
    InNestedValue { depth: u32 },
}

/// Streaming structural parser for partial Anthropic SSE tool-input JSON.
///
/// Feed each `InputJsonDelta` string via [`push`](Self::push). When the result is
/// [`PrefixState::ValidPrefix`] with an empty `missing_required`, the engine may
/// synthesize a `ToolCall` and speculatively dispatch it.
///
/// # Examples
///
/// ```rust
/// use zeph_core::agent::speculative::partial_json::{PartialJsonParser, PrefixState};
///
/// let mut p = PartialJsonParser::new();
/// // Simulate incremental SSE deltas
/// p.push(r#"{"command": "ls "#);
/// let state = p.push(r#"-la"}"#);
/// assert!(matches!(state, PrefixState::ValidPrefix { .. }));
/// ```
pub struct PartialJsonParser {
    buf: String,
    required: Vec<String>,
    /// Cached known leaves from the last successful scan.
    known_cache: Map<String, serde_json::Value>,
    /// Byte offset in `buf` up to which the cache is valid.
    /// When `buf.len() > scan_watermark` we re-scan only the tail, rebuilding from the
    /// cached state (H2: avoids O(N²) full-buffer rescan on every push).
    scan_watermark: usize,
}

impl PartialJsonParser {
    /// Create a new parser. Call [`set_required`](Self::set_required) to configure
    /// the list of required schema keys before the first [`push`](Self::push).
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            required: Vec::new(),
            known_cache: Map::new(),
            scan_watermark: 0,
        }
    }

    /// Set the list of keys required by the tool's `input_schema`.
    ///
    /// Keys in this list that are absent from the accumulated buffer are reported as
    /// `missing_required` in [`PrefixState::ValidPrefix`].
    pub fn set_required(&mut self, required: Vec<String>) {
        self.required = required;
    }

    /// Append `delta` and re-scan the buffer.
    ///
    /// Returns the current [`PrefixState`]. May be called repeatedly; each call
    /// replaces the previous result. Returns [`PrefixState::Malformed`] when the
    /// accumulated buffer would exceed 512 KiB (protection against oversized inputs).
    pub fn push(&mut self, delta: &str) -> PrefixState {
        const MAX_TOOL_INPUT_BYTES: usize = 512 * 1024;
        if self.buf.len() + delta.len() > MAX_TOOL_INPUT_BYTES {
            tracing::warn!(
                buf_len = self.buf.len(),
                delta_len = delta.len(),
                "PartialJsonParser: tool input exceeded 512 KiB cap; treating as malformed"
            );
            return PrefixState::Malformed;
        }
        self.buf.push_str(delta);
        self.scan()
    }

    /// Reset the parser for reuse after a commit or cancel.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.known_cache.clear();
        self.scan_watermark = 0;
    }

    /// Scan `self.buf` and extract fully closed top-level key-value pairs.
    ///
    /// Uses a watermark cursor to avoid replaying already-scanned bytes (H2).
    /// Previously confirmed key-value pairs are carried in `known_cache`; only the
    /// bytes after `scan_watermark` are newly examined.
    fn scan(&mut self) -> PrefixState {
        let bytes = self.buf.as_bytes();
        let len = bytes.len();

        // On the first call, consume the opening '{'.
        let mut i = if self.scan_watermark == 0 {
            let start = skip_ws(bytes, 0);
            if start >= len || bytes[start] != b'{' {
                return if self.buf.trim().is_empty() {
                    PrefixState::Incomplete
                } else {
                    PrefixState::Malformed
                };
            }
            start + 1 // consume '{'
        } else {
            self.scan_watermark
        };

        // Start with previously confirmed pairs; we may add more this call.
        let mut known = self.known_cache.clone();

        loop {
            i = skip_ws(bytes, i);
            if i >= len {
                break; // still incomplete
            }

            // End of object
            if bytes[i] == b'}' {
                self.scan_watermark = i + 1;
                let missing = self.missing(&known);
                self.known_cache.clone_from(&known);
                return PrefixState::ValidPrefix {
                    known_leaves: known,
                    missing_required: missing,
                };
            }

            // Comma between pairs
            if bytes[i] == b',' {
                i += 1;
                i = skip_ws(bytes, i);
                if i >= len {
                    break;
                }
            }

            // Key string
            if bytes[i] != b'"' {
                return PrefixState::Malformed;
            }
            let Some((key, after_key)) = read_string(bytes, i) else {
                break; // incomplete string
            };
            i = after_key;
            i = skip_ws(bytes, i);

            if i >= len {
                break;
            }
            if bytes[i] != b':' {
                return PrefixState::Malformed;
            }
            i += 1; // consume ':'
            i = skip_ws(bytes, i);

            if i >= len {
                break; // value not yet arrived
            }

            // Try to read a fully closed value
            match read_value(bytes, i) {
                ReadValue::Complete(value, end) => {
                    known.insert(key, value);
                    // Advance watermark: this pair is confirmed; next call starts here.
                    self.scan_watermark = end;
                    self.known_cache.clone_from(&known);
                    i = end;
                }
                ReadValue::Incomplete => break,
                ReadValue::Malformed => return PrefixState::Malformed,
            }
        }

        // Buffer ended mid-object — partial but valid so far
        let missing = self.missing(&known);
        PrefixState::ValidPrefix {
            known_leaves: known,
            missing_required: missing,
        }
    }

    fn missing(&self, known: &Map<String, serde_json::Value>) -> Vec<String> {
        self.required
            .iter()
            .filter(|k| !known.contains_key(k.as_str()))
            .cloned()
            .collect()
    }
}

impl Default for PartialJsonParser {
    fn default() -> Self {
        Self::new()
    }
}

// --- Internal helpers -------------------------------------------------------

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

/// Read a JSON string starting at `i` (which must point at `"`).
/// Returns `(string_content, index_after_closing_quote)` or `None` if incomplete.
///
/// Operates on raw bytes to avoid the Latin-1 `b as char` cast that corrupts non-ASCII.
/// The closed string slice is validated as UTF-8 and decoded at object boundary.
fn read_string(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    debug_assert_eq!(bytes[start], b'"');
    let mut i = start + 1;
    let mut escape = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
        } else if b == b'\\' {
            escape = true;
        } else if b == b'"' {
            // Decode the full slice [start+1..i] as UTF-8 in one shot.
            let content = std::str::from_utf8(&bytes[start + 1..i]).ok()?;
            // Re-process escape sequences by delegating to serde_json, which handles
            // \uXXXX, \n, \t, etc. correctly without re-implementing the JSON string spec.
            let json_str = [b"\"", &bytes[start + 1..i], b"\""].concat();
            let decoded: String =
                serde_json::from_slice(&json_str).unwrap_or_else(|_| content.to_owned());
            return Some((decoded, i + 1));
        }
        i += 1;
    }
    None // unterminated string
}

enum ReadValue {
    Complete(serde_json::Value, usize),
    Incomplete,
    Malformed,
}

/// Attempt to read a fully closed JSON value starting at `i`.
fn read_value(bytes: &[u8], i: usize) -> ReadValue {
    if i >= bytes.len() {
        return ReadValue::Incomplete;
    }
    match bytes[i] {
        b'"' => match read_string(bytes, i) {
            Some((s, end)) => ReadValue::Complete(serde_json::Value::String(s), end),
            None => ReadValue::Incomplete,
        },
        b'{' | b'[' => read_nested(bytes, i),
        b't' => read_literal(bytes, i, b"true", serde_json::Value::Bool(true)),
        b'f' => read_literal(bytes, i, b"false", serde_json::Value::Bool(false)),
        b'n' => read_literal(bytes, i, b"null", serde_json::Value::Null),
        b'-' | b'0'..=b'9' => read_number(bytes, i),
        _ => ReadValue::Malformed,
    }
}

fn read_literal(bytes: &[u8], i: usize, lit: &[u8], val: serde_json::Value) -> ReadValue {
    if bytes.len() < i + lit.len() {
        return ReadValue::Incomplete;
    }
    if &bytes[i..i + lit.len()] == lit {
        ReadValue::Complete(val, i + lit.len())
    } else {
        ReadValue::Malformed
    }
}

fn read_number(bytes: &[u8], mut i: usize) -> ReadValue {
    let start = i;
    if i < bytes.len() && bytes[i] == b'-' {
        i += 1;
    }
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    // Exponent
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        i += 1;
        if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == start {
        return ReadValue::Malformed;
    }
    // Must be followed by structural char or end
    if i < bytes.len() && !matches!(bytes[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
        return ReadValue::Incomplete;
    }
    let s = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(v) => ReadValue::Complete(v, i),
        Err(_) => ReadValue::Malformed,
    }
}

/// Read a nested `{...}` or `[...]`, tracking depth and string escapes.
fn read_nested(bytes: &[u8], start: usize) -> ReadValue {
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 1u32;
    let mut i = start + 1;
    let mut in_string = false;
    let mut escape = false;

    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
        } else if in_string {
            if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else if b == b'"' {
            in_string = true;
        } else if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                // Re-parse just the slice [start..=i] as JSON
                let parsed = std::str::from_utf8(&bytes[start..=i])
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
                return match parsed {
                    Some(v) => ReadValue::Complete(v, i + 1),
                    None => ReadValue::Malformed,
                };
            }
        }
        i += 1;
    }
    ReadValue::Incomplete
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(p: &mut PartialJsonParser, parts: &[&str]) -> PrefixState {
        let mut state = PrefixState::Incomplete;
        for part in parts {
            state = p.push(part);
        }
        state
    }

    /// Fixture 1: simple single-field bash command arriving in two deltas.
    #[test]
    fn fixture_simple_command_two_deltas() {
        let mut p = PartialJsonParser::new();
        p.set_required(vec!["command".into()]);
        p.push(r#"{"command": "ls "#);
        let state = p.push(r#"-la"}"#);
        match state {
            PrefixState::ValidPrefix {
                known_leaves,
                missing_required,
            } => {
                assert!(missing_required.is_empty());
                let v = known_leaves["command"].as_str().unwrap();
                assert!(v.contains("ls") && v.contains("la"), "got: {v}");
            }
            other => panic!("expected ValidPrefix, got {other:?}"),
        }
    }

    /// Fixture 2: multi-field tool call with nested object arrives incrementally.
    #[test]
    fn fixture_multi_field_incremental() {
        let mut p = PartialJsonParser::new();
        p.set_required(vec!["path".into(), "content".into()]);
        let state = push_all(
            &mut p,
            &[
                r#"{"path": "/tmp/f"#,
                r#"oo.txt", "content": "hel"#,
                r#"lo world"}"#,
            ],
        );
        match state {
            PrefixState::ValidPrefix {
                known_leaves,
                missing_required,
            } => {
                assert!(missing_required.is_empty(), "missing: {missing_required:?}");
                assert!(known_leaves.contains_key("path"));
                assert!(known_leaves.contains_key("content"));
            }
            other => panic!("expected ValidPrefix, got {other:?}"),
        }
    }

    /// Fixture 3: escape sequence inside string value does not break parser.
    #[test]
    fn fixture_escape_in_string() {
        let mut p = PartialJsonParser::new();
        p.set_required(vec!["msg".into()]);
        let state = p.push(r#"{"msg": "say \"hello\""}"#);
        match state {
            PrefixState::ValidPrefix {
                known_leaves,
                missing_required,
            } => {
                assert!(missing_required.is_empty());
                let v = known_leaves["msg"].as_str().unwrap();
                assert!(v.contains("hello"), "got: {v}");
            }
            other => panic!("expected ValidPrefix, got {other:?}"),
        }
    }

    /// Fixture 4: mid-delta truncation → Incomplete, then resolved on next delta.
    #[test]
    fn fixture_incomplete_then_resolved() {
        let mut p = PartialJsonParser::new();
        p.set_required(vec!["x".into()]);
        let mid = p.push(r#"{"x": 42"#);
        // No closing brace yet; key present but object not closed → ValidPrefix with x known
        match &mid {
            PrefixState::ValidPrefix {
                known_leaves,
                missing_required,
            } => {
                assert!(missing_required.is_empty());
                assert_eq!(known_leaves["x"], 42);
            }
            PrefixState::Incomplete => {} // also acceptable
            other @ PrefixState::Malformed => panic!("unexpected: {other:?}"),
        }
        let done = p.push("}");
        assert!(matches!(done, PrefixState::ValidPrefix { .. }));
    }

    /// Fixture 5: malformed input returns Malformed.
    #[test]
    fn fixture_malformed_input() {
        let mut p = PartialJsonParser::new();
        let state = p.push("not-json");
        assert!(matches!(state, PrefixState::Malformed));
    }

    /// Fixture 6: mid-array value at top level is opaque (depth > 1 skipped).
    #[test]
    fn fixture_top_level_array_value() {
        let mut p = PartialJsonParser::new();
        p.set_required(vec!["items".into()]);
        let state = p.push(r#"{"items": [1, 2, 3]}"#);
        match state {
            PrefixState::ValidPrefix {
                known_leaves,
                missing_required,
            } => {
                assert!(missing_required.is_empty());
                assert!(known_leaves["items"].is_array());
            }
            other => panic!("expected ValidPrefix, got {other:?}"),
        }
    }

    #[test]
    fn reset_clears_buffer() {
        let mut p = PartialJsonParser::new();
        p.push(r#"{"x": 1}"#);
        p.reset();
        let state = p.push(r#"{"y": 2}"#);
        match state {
            PrefixState::ValidPrefix { known_leaves, .. } => {
                assert!(
                    !known_leaves.contains_key("x"),
                    "should be cleared after reset"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// Fixture 7: Unicode / non-ASCII bytes must not be corrupted (C5 regression).
    #[test]
    fn fixture_unicode_filename() {
        let mut p = PartialJsonParser::new();
        p.set_required(vec!["path".into()]);
        let state = p.push(r#"{"path": "/tmp/Привет.txt"}"#);
        match state {
            PrefixState::ValidPrefix {
                known_leaves,
                missing_required,
            } => {
                assert!(missing_required.is_empty());
                let v = known_leaves["path"].as_str().unwrap();
                assert!(v.contains("Привет"), "non-ASCII corrupted: {v}");
            }
            other => panic!("expected ValidPrefix, got {other:?}"),
        }
    }

    /// Fixture 8: incremental watermark — second push does not re-parse completed pairs (H2).
    #[test]
    fn fixture_incremental_watermark() {
        let mut p = PartialJsonParser::new();
        p.set_required(vec!["a".into(), "b".into()]);
        // First push: 'a' complete, 'b' incomplete
        let s1 = p.push(r#"{"a": 1, "b": "#);
        match &s1 {
            PrefixState::ValidPrefix {
                known_leaves,
                missing_required,
            } => {
                assert!(known_leaves.contains_key("a"));
                assert!(missing_required.contains(&"b".to_string()));
            }
            PrefixState::Incomplete => {} // also acceptable before 'b' is fully parsed
            other @ PrefixState::Malformed => panic!("unexpected s1: {other:?}"),
        }
        // Second push: completes 'b'
        let s2 = p.push("2}");
        match s2 {
            PrefixState::ValidPrefix {
                known_leaves,
                missing_required,
            } => {
                assert!(
                    missing_required.is_empty(),
                    "still missing: {missing_required:?}"
                );
                assert_eq!(known_leaves["a"], 1);
                assert_eq!(known_leaves["b"], 2);
            }
            other => panic!("expected ValidPrefix, got {other:?}"),
        }
    }
}
