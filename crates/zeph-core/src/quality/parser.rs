// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Best-effort JSON parser with one retry for use in the self-check pipeline.
//!
//! LLMs frequently wrap JSON output in markdown fences or prepend prose. This module
//! strips those artifacts before deserializing.

use std::time::Duration;

use serde::de::DeserializeOwned;
use thiserror::Error;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, Role};

/// Errors from the parser.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("no opening brace found in output")]
    NoBraceSpan,
    #[error("JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Errors from `chat_json` (wraps [`ParseError`] and provider/timeout errors).
#[derive(Debug, Error)]
pub enum ChatJsonError {
    #[error("LLM error: {0}")]
    Llm(#[from] zeph_llm::LlmError),
    #[error("timed out after {0}ms")]
    Timeout(u64),
    #[error("failed to parse JSON after 2 attempts; last raw (truncated): {0}")]
    Parse(String),
}

/// Strip markdown code fences from LLM output.
fn strip_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let after_lang = if let Some(nl) = rest.find('\n') {
            &rest[nl + 1..]
        } else {
            rest
        };
        if let Some(end) = after_lang.rfind("```") {
            return after_lang[..end].trim();
        }
        return after_lang.trim();
    }
    trimmed
}

/// Find the first `{...}` or `[...]` span in the string.
fn find_first_brace_span(s: &str) -> Option<&str> {
    let open = s.find(['{', '['])?;
    let opener = s.as_bytes()[open];
    let closer = if opener == b'{' { b'}' } else { b']' };
    let mut depth = 0i32;
    let bytes = s.as_bytes();
    let mut close = None;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        if b == opener {
            depth += 1;
        } else if b == closer {
            depth -= 1;
            if depth == 0 {
                close = Some(i);
                break;
            }
        }
    }
    let close = close?;
    Some(&s[open..=close])
}

/// Parse JSON from a raw LLM string, stripping fences and finding the first brace span.
///
/// # Errors
///
/// Returns [`ParseError`] if no brace span is found or JSON deserialization fails.
pub fn parse_json<T: DeserializeOwned>(raw: &str) -> Result<T, ParseError> {
    let stripped = strip_fences(raw);
    let span = find_first_brace_span(stripped).ok_or(ParseError::NoBraceSpan)?;
    Ok(serde_json::from_str(span)?)
}

/// Build a two-message `[system, user]` slice for a provider call.
fn build_messages(system: &str, user: &str) -> Vec<Message> {
    vec![
        Message {
            role: Role::System,
            content: system.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: user.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ]
}

/// Approximate token count from raw string (4 chars ≈ 1 token).
#[must_use]
pub fn approx_tokens(s: &str) -> u64 {
    (s.len() as u64).saturating_add(3) / 4
}

/// Timeout duration in milliseconds, clamped to `u64::MAX`.
fn timeout_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Call the provider and parse the JSON result, retrying once on parse failure.
///
/// Returns `(value, approx_tokens, attempt_number)` on success.
///
/// # Errors
///
/// Returns [`ChatJsonError`] if both attempts fail, the provider errors, or timeout is hit.
pub async fn chat_json<T: DeserializeOwned>(
    provider: &AnyProvider,
    system: &str,
    user: &str,
    per_call_timeout: Duration,
) -> Result<(T, u64, u32), ChatJsonError> {
    let msgs = build_messages(system, user);

    // Attempt 1
    let first = tokio::time::timeout(per_call_timeout, provider.chat(&msgs)).await;
    match first {
        Ok(Ok(raw)) => {
            if let Ok(v) = parse_json::<T>(&raw) {
                return Ok((v, approx_tokens(&raw), 1));
            }
            // Attempt 2: corrective nudge
            let retry_user = format!(
                "{user}\n\nPrevious output was not valid JSON. \
                 Re-output strict JSON only, no prose, no fences."
            );
            let retry_msgs = build_messages(system, &retry_user);
            let second = tokio::time::timeout(per_call_timeout, provider.chat(&retry_msgs)).await;
            match second {
                Ok(Ok(raw2)) => parse_json::<T>(&raw2)
                    .map(|v| (v, approx_tokens(&raw2), 2))
                    .map_err(|_| {
                        let truncated = if raw2.len() > 4096 {
                            let end = raw2.floor_char_boundary(4096);
                            format!("{}…", &raw2[..end])
                        } else {
                            raw2.clone()
                        };
                        ChatJsonError::Parse(truncated)
                    }),
                Ok(Err(e)) => Err(ChatJsonError::Llm(e)),
                Err(_) => Err(ChatJsonError::Timeout(timeout_ms(per_call_timeout))),
            }
        }
        Ok(Err(e)) => Err(ChatJsonError::Llm(e)),
        Err(_) => Err(ChatJsonError::Timeout(timeout_ms(per_call_timeout))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_json_markdown_fences() {
        let raw = "```json\n{\"a\":1}\n```";
        let v: serde_json::Value = parse_json(raw).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn strips_plain_fences() {
        let raw = "```\n{\"a\":2}\n```";
        let v: serde_json::Value = parse_json(raw).unwrap();
        assert_eq!(v["a"], 2);
    }

    #[test]
    fn finds_brace_span_in_prose() {
        let raw = "Here is the JSON: {\"x\":42} as requested.";
        let v: serde_json::Value = parse_json(raw).unwrap();
        assert_eq!(v["x"], 42);
    }

    #[test]
    fn returns_error_on_no_brace() {
        let result = parse_json::<serde_json::Value>("no json here");
        assert!(matches!(result, Err(ParseError::NoBraceSpan)));
    }

    #[test]
    fn handles_nested_braces() {
        let raw = r#"{"outer":{"inner":1}}"#;
        let v: serde_json::Value = parse_json(raw).unwrap();
        assert_eq!(v["outer"]["inner"], 1);
    }
}
