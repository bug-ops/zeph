// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live integration tests for the Claude provider against the real Anthropic API.
//!
//! Skipped by default — requires a real API key. Run with:
//! ```shell
//! ZEPH_CLAUDE_API_KEY=<key> cargo nextest run -p zeph-llm -- --ignored claude_live
//! ```
//!
//! These cover the #5889 capability-gate change: `ThinkingCapability::prefers_effort` now
//! also covers Opus 4.7/4.8 and Sonnet 5, which reject `budget_tokens` outright (400) and
//! must auto-convert `Extended` thinking config to an `effort` level. The same flag also
//! gates trailing-assistant-message stripping (`no_prefill` in `build_request`), so a
//! prefill round-trip is exercised too — both behaviors changed together for the new
//! generation and both are 400-prone if the conversion/stripping is wrong.

use zeph_llm::ThinkingConfig;
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::provider::{LlmProvider, Message, Role};

fn api_key() -> Option<String> {
    match std::env::var("ZEPH_CLAUDE_API_KEY") {
        Ok(k) if !k.is_empty() => Some(k),
        _ => {
            eprintln!("ZEPH_CLAUDE_API_KEY not set, skipping");
            None
        }
    }
}

/// Confirms `Extended { budget_tokens }` on `claude-opus-4-8` is auto-converted to an
/// `effort` level before being sent — the model 400s if `budget_tokens` is sent as-is
/// (S3, `prefers_effort`).
#[tokio::test]
#[ignore = "requires ZEPH_CLAUDE_API_KEY env var and live Anthropic API access"]
async fn claude_live_opus_4_8_extended_thinking_converts_to_effort() {
    let Some(api_key) = api_key() else { return };

    let provider = ClaudeProvider::new(api_key, "claude-opus-4-8".into(), 1024)
        .with_thinking(ThinkingConfig::Extended {
            budget_tokens: 5_000,
        })
        .expect("valid thinking config");

    let messages = vec![Message::from_legacy(Role::User, "Say hello in one word.")];

    match provider.chat(&messages).await {
        Ok(response) => assert!(!response.is_empty(), "response must not be empty"),
        Err(err) => panic!(
            "chat request against claude-opus-4-8 with Extended{{budget_tokens}} thinking \
             failed: {err}\nIf this is a 400/422, prefers_effort likely stopped converting \
             budget_tokens to effort for this model — check thinking_capability() in \
             crates/zeph-llm/src/claude/types.rs."
        ),
    }
}

/// Confirms a trailing assistant message is correctly stripped for `claude-sonnet-5` with
/// thinking enabled (the `no_prefill` gate at `mod.rs:861`, driven by the same
/// `prefers_effort` flag S3 widened) — the API 400s on assistant-message prefill when
/// thinking is active, so a passing round-trip confirms the strip fired.
#[tokio::test]
#[ignore = "requires ZEPH_CLAUDE_API_KEY env var and live Anthropic API access"]
async fn claude_live_sonnet_5_thinking_strips_trailing_assistant_prefill() {
    let Some(api_key) = api_key() else { return };

    let provider = ClaudeProvider::new(api_key, "claude-sonnet-5".into(), 1024)
        .with_thinking(ThinkingConfig::Adaptive { effort: None })
        .expect("valid thinking config");

    let messages = vec![
        Message::from_legacy(Role::User, "Say hello in one word."),
        Message::from_legacy(Role::Assistant, "Hel"),
    ];

    match provider.chat(&messages).await {
        Ok(response) => assert!(!response.is_empty(), "response must not be empty"),
        Err(err) => panic!(
            "chat request against claude-sonnet-5 with a trailing assistant message and \
             thinking enabled failed: {err}\nIf this is a 400, the no_prefill strip at \
             build_request (mod.rs) is not firing for this model — check that \
             thinking_capability(\"claude-sonnet-5\").prefers_effort is true."
        ),
    }
}
