// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `zeph://` URI parser (spec-066).
//!
//! This module provides [`parse_deep_link`], a sync, panic-free, I/O-free parser for
//! `zeph://` URIs. It lives in `zeph-common` (Layer-0a) so it can be used by both the
//! CLI entry point and any future channel that needs to handle deep-link URIs without
//! pulling in higher-level crates.
//!
//! # Supported URIs
//!
//! - `zeph://new-session` — open a new agent session with optional parameters:
//!   - `?prompt=<text>` — percent-encoded prompt text (max 8192 bytes after decoding)
//!   - `?cwd=<path>` — absolute working directory (validated by caller via INV-CWD)
//!   - `?profile=<name>` — named config profile (validated against known profiles by caller)
//!   - `?model=<name>` — provider name from `[[llm.providers]]` (validated by caller)
//!
//! # Security
//!
//! - `auto` and `-y` query parameters are silently dropped with a `WARN` log (INV-NOAUTO).
//! - Prompt length is checked after percent-decoding; URIs with prompt > 8192 bytes are
//!   rejected with [`DeepLinkError::PromptTooLong`].
//! - Prompts containing C0 control characters (except TAB `0x09`, LF `0x0a`, CR `0x0d`) or
//!   DEL `0x7f` are rejected with [`DeepLinkError::PromptContainsControlChars`] to prevent
//!   terminal injection attacks.
//! - `cwd` must be an absolute path; relative paths are rejected with
//!   [`DeepLinkError::CwdNotAbsolute`]. Further validation (canonicalization, denylist,
//!   allowlist) is performed by `validate_deep_link_cwd` in `src/url_scheme/validate.rs`.

use std::path::PathBuf;

const PROMPT_MAX_BYTES: usize = 8192;

/// Parsed representation of a `zeph://` URI.
///
/// Currently only the `new-session` action is defined. Additional variants may be added
/// in future spec revisions.
///
/// # Examples
///
/// ```
/// use zeph_common::deep_link::{DeepLink, parse_deep_link};
///
/// let link = parse_deep_link("zeph://new-session?prompt=Hello").unwrap();
/// assert!(matches!(link, DeepLink::NewSession(_)));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum DeepLink {
    /// Open a new agent session with the given parameters.
    NewSession(NewSessionParams),
}

/// Parameters extracted from a `zeph://new-session` URI.
///
/// All fields are optional; callers validate non-`None` values against their respective
/// registries (profile names, provider names) before use.
///
/// # Examples
///
/// ```
/// use zeph_common::deep_link::{DeepLink, parse_deep_link};
/// use std::path::PathBuf;
///
/// let link = parse_deep_link("zeph://new-session?cwd=%2Fhome%2Fuser&prompt=Hi").unwrap();
/// let DeepLink::NewSession(params) = link;
/// assert_eq!(params.cwd, Some(PathBuf::from("/home/user")));
/// assert_eq!(params.prompt.as_deref(), Some("Hi"));
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewSessionParams {
    /// Absolute working directory (percent-decoded). Callers must invoke
    /// `validate_deep_link_cwd` before using this value.
    pub cwd: Option<PathBuf>,
    /// Percent-decoded prompt text. Length is capped at 8192 bytes.
    /// Not yet sanitized — callers must pass through the content sanitizer.
    pub prompt: Option<String>,
    /// Named config profile alias. Validated against known profiles by the caller.
    pub profile: Option<String>,
    /// Provider name from `[[llm.providers]]`. Validated by the caller.
    pub model: Option<String>,
}

/// Errors returned by [`parse_deep_link`].
#[derive(Debug, thiserror::Error)]
pub enum DeepLinkError {
    /// The URI is structurally invalid (not a valid URL, wrong scheme, etc.).
    #[error("malformed URI: {0}")]
    Malformed(String),

    /// The host portion names an unknown action.
    ///
    /// This typically means the user has a URI intended for a newer version of Zeph.
    /// Unknown hosts are rejected; new host actions added in future versions will still
    /// return this error until Zeph is upgraded.
    #[error("unknown scheme action '{0}'; try upgrading zeph")]
    UnknownHost(String),

    /// The decoded `prompt` parameter exceeds the 8192-byte limit.
    #[error("prompt too long: {0} bytes (limit: 8192)")]
    PromptTooLong(usize),

    /// The `cwd` parameter is a relative path; only absolute paths are accepted.
    #[error("cwd must be an absolute path")]
    CwdNotAbsolute,

    /// The decoded `prompt` contains disallowed control characters.
    #[error("prompt contains disallowed control characters")]
    PromptContainsControlChars,
}

/// Parses a `zeph://` URI string into a typed [`DeepLink`].
///
/// This function is sync, panic-free, and performs no I/O or network calls.
/// Percent-encoding in query parameters is decoded automatically by the `url` crate.
///
/// # Security
///
/// - `auto` and `-y` query parameters are silently dropped with a `WARN` log (INV-NOAUTO).
/// - Prompt length is enforced after decoding; URIs with > 8192-byte prompts are rejected.
/// - `cwd` is checked for absolute path; further validation is the caller's responsibility.
///
/// # Errors
///
/// - [`DeepLinkError::Malformed`] — URI cannot be parsed or has a scheme other than `zeph`.
/// - [`DeepLinkError::UnknownHost`] — the host part does not name a known action.
/// - [`DeepLinkError::PromptTooLong`] — decoded prompt exceeds 8192 bytes.
/// - [`DeepLinkError::CwdNotAbsolute`] — `cwd` query param is a relative path.
/// - [`DeepLinkError::PromptContainsControlChars`] — prompt contains bytes < 0x20 (except TAB/LF/CR) or 0x7f.
///
/// # Examples
///
/// ```
/// use zeph_common::deep_link::{DeepLink, parse_deep_link};
///
/// let link = parse_deep_link("zeph://new-session?prompt=Hello").unwrap();
/// assert!(matches!(link, DeepLink::NewSession(_)));
///
/// let err = parse_deep_link("http://example.com");
/// assert!(err.is_err());
/// ```
pub fn parse_deep_link(uri: &str) -> Result<DeepLink, DeepLinkError> {
    let parsed = url::Url::parse(uri).map_err(|e| DeepLinkError::Malformed(e.to_string()))?;

    if parsed.scheme() != "zeph" {
        return Err(DeepLinkError::Malformed(format!(
            "expected scheme 'zeph', got '{}'",
            parsed.scheme()
        )));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| DeepLinkError::Malformed("missing action host".to_owned()))?;

    match host {
        "new-session" => {
            let params = parse_new_session_params(&parsed)?;
            Ok(DeepLink::NewSession(params))
        }
        other => Err(DeepLinkError::UnknownHost(other.to_owned())),
    }
}

fn contains_control_chars(s: &str) -> bool {
    s.bytes()
        .any(|b| matches!(b, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f))
}

fn parse_new_session_params(url: &url::Url) -> Result<NewSessionParams, DeepLinkError> {
    let mut params = NewSessionParams::default();

    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "prompt" => {
                let text = value.into_owned();
                if text.len() > PROMPT_MAX_BYTES {
                    return Err(DeepLinkError::PromptTooLong(text.len()));
                }
                if contains_control_chars(&text) {
                    return Err(DeepLinkError::PromptContainsControlChars);
                }
                params.prompt = Some(text);
            }
            "cwd" => {
                let path = PathBuf::from(value.as_ref());
                if path.is_relative() {
                    return Err(DeepLinkError::CwdNotAbsolute);
                }
                params.cwd = Some(path);
            }
            "profile" => {
                params.profile = Some(value.into_owned());
            }
            "model" => {
                params.model = Some(value.into_owned());
            }
            "auto" | "-y" => {
                // INV-NOAUTO: auto-escalation params are silently dropped.
                tracing::warn!(param = %key, "deep-link: auto-escalation param dropped (INV-NOAUTO)");
            }
            _ => {
                // Unknown params are silently ignored for forward compatibility.
            }
        }
    }

    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_new_session_with_prompt_and_cwd() {
        let link = parse_deep_link("zeph://new-session?prompt=Hello&cwd=/home/user").unwrap();
        let DeepLink::NewSession(p) = link;
        assert_eq!(p.prompt.as_deref(), Some("Hello"));
        assert_eq!(p.cwd, Some(PathBuf::from("/home/user")));
    }

    #[test]
    fn invalid_scheme_returns_malformed() {
        let err = parse_deep_link("http://example.com").unwrap_err();
        assert!(matches!(err, DeepLinkError::Malformed(_)));
    }

    #[test]
    fn unknown_host_returns_error() {
        let err = parse_deep_link("zeph://unknown-action").unwrap_err();
        assert!(matches!(err, DeepLinkError::UnknownHost(_)));
    }

    #[test]
    fn oversized_prompt_returns_error() {
        let big = "x".repeat(PROMPT_MAX_BYTES + 1);
        let uri = format!("zeph://new-session?prompt={big}");
        let err = parse_deep_link(&uri).unwrap_err();
        assert!(matches!(err, DeepLinkError::PromptTooLong(n) if n == PROMPT_MAX_BYTES + 1));
    }

    #[test]
    fn percent_decoded_prompt() {
        let link = parse_deep_link("zeph://new-session?prompt=Hello%20World").unwrap();
        let DeepLink::NewSession(p) = link;
        assert_eq!(p.prompt.as_deref(), Some("Hello World"));
    }

    #[test]
    fn auto_param_dropped_prompt_still_parsed() {
        let link = parse_deep_link("zeph://new-session?auto=true&prompt=ok").unwrap();
        let DeepLink::NewSession(p) = link;
        assert_eq!(p.prompt.as_deref(), Some("ok"));
    }

    #[test]
    fn relative_cwd_returns_error() {
        let err = parse_deep_link("zeph://new-session?cwd=relative/path").unwrap_err();
        assert!(matches!(err, DeepLinkError::CwdNotAbsolute));
    }

    #[test]
    fn absolute_cwd_stored() {
        let link = parse_deep_link("zeph://new-session?cwd=/home/user").unwrap();
        let DeepLink::NewSession(p) = link;
        assert_eq!(p.cwd, Some(PathBuf::from("/home/user")));
    }

    #[test]
    fn profile_and_model_stored() {
        let link = parse_deep_link("zeph://new-session?profile=dev&model=fast").unwrap();
        let DeepLink::NewSession(p) = link;
        assert_eq!(p.profile.as_deref(), Some("dev"));
        assert_eq!(p.model.as_deref(), Some("fast"));
    }

    #[test]
    fn dash_y_param_dropped() {
        let link = parse_deep_link("zeph://new-session?-y&prompt=ok").unwrap();
        let DeepLink::NewSession(p) = link;
        assert_eq!(p.prompt.as_deref(), Some("ok"));
    }

    #[test]
    fn unknown_params_silently_ignored() {
        let link = parse_deep_link("zeph://new-session?foo=bar&prompt=hi").unwrap();
        let DeepLink::NewSession(p) = link;
        assert_eq!(p.prompt.as_deref(), Some("hi"));
    }

    #[test]
    fn nul_byte_in_prompt_returns_control_chars_error() {
        let uri = "zeph://new-session?prompt=hello%00world";
        let err = parse_deep_link(uri).unwrap_err();
        assert!(
            matches!(err, DeepLinkError::PromptContainsControlChars),
            "expected PromptContainsControlChars, got {err:?}"
        );
    }

    #[test]
    fn tab_and_lf_in_prompt_are_accepted() {
        let uri = "zeph://new-session?prompt=line1%09tab%0Aline2";
        let result = parse_deep_link(uri).unwrap();
        let DeepLink::NewSession(p) = result;
        assert_eq!(p.prompt.as_deref(), Some("line1\ttab\nline2"));
    }

    #[test]
    fn esc_sequence_in_prompt_returns_control_chars_error() {
        // ESC byte is 0x1b — must be rejected
        let uri = "zeph://new-session?prompt=hello%1bworld";
        // 0x1b falls in 0x0e..=0x1f range: rejected
        let err = parse_deep_link(uri).unwrap_err();
        assert!(
            matches!(err, DeepLinkError::PromptContainsControlChars),
            "expected PromptContainsControlChars for ESC, got {err:?}"
        );
    }

    #[test]
    fn prompt_exactly_8192_bytes_ok() {
        let prompt = "a".repeat(8192);
        let uri = format!("zeph://new-session?prompt={prompt}");
        let result = parse_deep_link(&uri);
        assert!(result.is_ok(), "exactly 8192 bytes should be accepted");
    }

    #[test]
    fn bare_new_session_no_params() {
        let result = parse_deep_link("zeph://new-session").unwrap();
        let DeepLink::NewSession(p) = result;
        assert!(p.cwd.is_none());
        assert!(p.prompt.is_none());
        assert!(p.profile.is_none());
        assert!(p.model.is_none());
    }

    proptest::proptest! {
        #[test]
        fn fuzz_parse_deep_link_no_panic(s in ".*") {
            let _ = parse_deep_link(&s);
        }
    }
}
