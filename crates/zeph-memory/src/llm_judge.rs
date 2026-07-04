// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared "bounded LLM call, fail open" scaffolding for write-path scoring gates.
//!
//! [`crate::admission`] and [`crate::quality_gate`] both score candidate memories with
//! best-effort LLM/embedding calls that must never block a write: every external call is
//! wrapped in a timeout, and any timeout or provider error falls back to a neutral score
//! rather than propagating an error. This module owns that scaffolding so each gate only
//! supplies its own prompts, similarity computation, or response parsing.

use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

/// Embed `content` with a bounded timeout, failing open on any error.
///
/// Returns `Ok(vector)` on success. Returns `Err(fail_open_score)` on embed error or
/// timeout — callers should treat `fail_open_score` as the final score for whatever
/// factor they were computing, skipping any further similarity comparison.
///
/// `log_context` is prefixed to the tracing messages emitted on failure so each call site
/// stays identifiable in logs (e.g. `"A-MAC: novelty"`, `"quality_gate: info_value"`).
pub(crate) async fn embed_with_timeout_fail_open(
    provider: &AnyProvider,
    content: &str,
    embed_timeout: Duration,
    fail_open_score: f32,
    log_context: &str,
) -> Result<Vec<f32>, f32> {
    match tokio::time::timeout(embed_timeout, provider.embed(content)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "{log_context}: embed failed, using fail-open score");
            Err(fail_open_score)
        }
        Err(_) => {
            tracing::warn!("{log_context}: embed timed out, using fail-open score");
            Err(fail_open_score)
        }
    }
}

/// Run a single-shot LLM judge call: build a system/user message pair, call `provider.chat`
/// under a timeout, and parse the response into a score.
///
/// Fails open to `fail_open_score` — returned as-is, unclamped — on any provider error or
/// timeout. When a response is received, `parse` extracts a score from it; if `parse`
/// returns `None` the result also falls back to `fail_open_score`, but (matching the
/// original per-call-site behavior) this fallback is clamped to `[0.0, 1.0]` same as a
/// successfully parsed score.
///
/// `log_context` is prefixed to the tracing messages emitted on failure.
pub(crate) async fn llm_judge_score(
    provider: &AnyProvider,
    system_prompt: &str,
    user_prompt: String,
    timeout: Duration,
    fail_open_score: f32,
    log_context: &str,
    parse: impl Fn(&str) -> Option<f32>,
) -> f32 {
    let messages = vec![
        Message {
            role: Role::System,
            content: system_prompt.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: user_prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];

    let result = match tokio::time::timeout(timeout, provider.chat(&messages)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "{log_context}: LLM call failed, using fail-open score");
            return fail_open_score;
        }
        Err(_) => {
            tracing::debug!("{log_context}: LLM call timed out, using fail-open score");
            return fail_open_score;
        }
    };

    parse(&result).unwrap_or(fail_open_score).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn embed_with_timeout_fail_open_returns_ok_on_success() {
        let mock = zeph_llm::mock::MockProvider::default().with_embedding(vec![1.0, 2.0, 3.0]);
        let provider = zeph_llm::any::AnyProvider::Mock(mock);
        let result =
            embed_with_timeout_fail_open(&provider, "hello", Duration::from_secs(5), 0.5, "test")
                .await;
        assert_eq!(result, Ok(vec![1.0, 2.0, 3.0]));
    }

    #[tokio::test]
    async fn embed_with_timeout_fail_open_fails_open_on_timeout() {
        tokio::time::pause();
        let mock = zeph_llm::mock::MockProvider::default()
            .with_embed_delay(10_000)
            .with_embedding(vec![0.0; 4]);
        let provider = zeph_llm::any::AnyProvider::Mock(mock);
        let fut = async move {
            embed_with_timeout_fail_open(&provider, "hello", Duration::from_secs(5), 0.42, "test")
                .await
        };
        let handle = tokio::spawn(fut); // EXEMPT: test-only tokio::time::pause harness
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        let result = handle.await.expect("task panicked");
        assert_eq!(result, Err(0.42));
    }

    #[tokio::test]
    async fn embed_with_timeout_fail_open_fails_open_on_error() {
        let mock = zeph_llm::mock::MockProvider::default().with_embed_invalid_input();
        let provider = zeph_llm::any::AnyProvider::Mock(mock);
        let result =
            embed_with_timeout_fail_open(&provider, "hello", Duration::from_secs(5), 0.77, "test")
                .await;
        assert_eq!(result, Err(0.77));
    }

    #[tokio::test]
    async fn llm_judge_score_parses_valid_response() {
        let mock = zeph_llm::mock::MockProvider::with_responses(vec!["0.75".into()]);
        let provider = zeph_llm::any::AnyProvider::Mock(mock);
        let score = llm_judge_score(
            &provider,
            "system",
            "user".to_owned(),
            Duration::from_secs(5),
            0.5,
            "test",
            |s| s.trim().parse::<f32>().ok(),
        )
        .await;
        assert!((score - 0.75).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn llm_judge_score_fails_open_on_provider_error() {
        let mock = zeph_llm::mock::MockProvider::default()
            .with_errors(vec![zeph_llm::LlmError::Other("boom".into())]);
        let provider = zeph_llm::any::AnyProvider::Mock(mock);
        let score = llm_judge_score(
            &provider,
            "system",
            "user".to_owned(),
            Duration::from_secs(5),
            0.33,
            "test",
            |s| s.trim().parse::<f32>().ok(),
        )
        .await;
        assert!((score - 0.33).abs() < f32::EPSILON);
    }

    // Regression pair for the clamped-vs-unclamped fail-open asymmetry described in this
    // module's doc comment: the SAME out-of-[0,1]-range fail_open_score (1.5) must come back
    // unclamped on timeout/error, but clamped when it's used as the parse-failure fallback.
    // If someone "fixes" this asymmetry later (e.g. by clamping both, or neither), exactly one
    // of these two tests will fail.

    #[tokio::test]
    async fn llm_judge_score_falls_back_unclamped_on_timeout() {
        tokio::time::pause();
        let mock = zeph_llm::mock::MockProvider::default().with_delay(10_000);
        let provider = zeph_llm::any::AnyProvider::Mock(mock);
        let fut = async move {
            llm_judge_score(
                &provider,
                "system",
                "user".to_owned(),
                Duration::from_secs(5),
                1.5,
                "test",
                |s| s.trim().parse::<f32>().ok(),
            )
            .await
        };
        let handle = tokio::spawn(fut); // EXEMPT: test-only tokio::time::pause harness
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        let score = handle.await.expect("task panicked");
        assert!(
            (score - 1.5).abs() < f32::EPSILON,
            "timeout fail-open must be returned unclamped, got {score}"
        );
    }

    #[tokio::test]
    async fn llm_judge_score_clamps_unparseable_response_fallback() {
        let mock = zeph_llm::mock::MockProvider::with_responses(vec!["not-a-number".into()]);
        let provider = zeph_llm::any::AnyProvider::Mock(mock);
        let score = llm_judge_score(
            &provider,
            "system",
            "user".to_owned(),
            Duration::from_secs(5),
            1.5,
            "test",
            |s| s.trim().parse::<f32>().ok(),
        )
        .await;
        assert!(
            (score - 1.0).abs() < f32::EPSILON,
            "unparseable-response fallback must be clamped to [0,1], got {score}"
        );
    }
}
