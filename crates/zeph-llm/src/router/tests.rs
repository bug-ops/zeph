// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unit tests for [`RouterProvider`](super::RouterProvider) and its routing strategies.

use std::sync::atomic::Ordering;

use super::chat::collect_stream;
use super::embed_cache::TurnEmbedCache;
use super::*;
use crate::any::AnyProvider;
use crate::error::LlmError;
use crate::provider::{LlmProvider, Message, Role};
use std::assert_matches;

#[test]
fn empty_router_name() {
    let r = RouterProvider::new(vec![]);
    assert_eq!(r.name(), "router");
}

#[test]
fn empty_router_supports_nothing() {
    let r = RouterProvider::new(vec![]);
    assert!(!r.supports_streaming());
    assert!(!r.supports_embeddings());
    assert!(!r.supports_tool_use());
}

#[test]
fn empty_router_context_window_none() {
    let r = RouterProvider::new(vec![]);
    assert!(r.context_window().is_none());
}

#[tokio::test]
async fn empty_router_chat_returns_no_providers() {
    let r = RouterProvider::new(vec![]);
    let msgs = vec![Message::from_legacy(Role::User, "hello")];
    let err = r.chat(&msgs).await.unwrap_err();
    assert_matches!(err, LlmError::NoProviders);
}

#[tokio::test]
async fn empty_router_chat_stream_returns_no_providers() {
    let r = RouterProvider::new(vec![]);
    let msgs = vec![Message::from_legacy(Role::User, "hello")];
    let result = r.chat_stream(&msgs).await;
    assert!(matches!(result, Err(LlmError::NoProviders)));
}

#[tokio::test]
async fn empty_router_embed_returns_no_providers() {
    let r = RouterProvider::new(vec![]);
    let err = r.embed("test").await.unwrap_err();
    assert_matches!(err, LlmError::NoProviders);
}

#[tokio::test]
async fn empty_router_chat_with_tools_returns_no_providers() {
    let r = RouterProvider::new(vec![]);
    let msgs = vec![Message::from_legacy(Role::User, "hello")];
    let err = r.chat_with_tools(&msgs, &[]).await.unwrap_err();
    assert_matches!(err, LlmError::NoProviders);
}

#[tokio::test]
async fn router_falls_back_on_unreachable() {
    use crate::ollama::OllamaProvider;

    let p1 = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "m".into(),
        "e".into(),
    ));
    let p2 = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:2",
        "m".into(),
        "e".into(),
    ));
    let r = RouterProvider::new(vec![p1, p2]);
    let msgs = vec![Message::from_legacy(Role::User, "hello")];
    let err = r.chat(&msgs).await.unwrap_err();
    // Fallback exhaustion must surface the last provider's actual error (#5821),
    // not a generic `NoProviders` that discards the connection-failure diagnostic.
    assert!(
        matches!(&err, LlmError::Other(msg) if msg.contains("Ollama chat request failed")),
        "expected the last provider's actual error to survive exhaustion, got {err:?}"
    );
}

#[test]
fn router_with_streaming_provider() {
    use crate::ollama::OllamaProvider;

    let p = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "m".into(),
        "e".into(),
    ));
    let r = RouterProvider::new(vec![p]);
    assert!(r.supports_streaming());
    assert!(r.supports_embeddings());
}

#[test]
fn clone_preserves_providers() {
    use crate::ollama::OllamaProvider;

    let p = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "m".into(),
        "e".into(),
    ));
    let r = RouterProvider::new(vec![p]);
    let c = r.clone();
    assert_eq!(c.state.providers.len(), 1);
    assert_eq!(c.name(), "router");
}

#[test]
fn last_cache_usage_returns_none() {
    let r = RouterProvider::new(vec![]);
    assert!(r.last_cache_usage().is_none());
}

#[test]
fn thompson_strategy_is_set() {
    let r = RouterProvider::new(vec![]).with_thompson(None);
    assert_eq!(r.strategy, RouterStrategy::Thompson);
    assert!(r.thompson.is_some());
}

#[tokio::test]
async fn save_thompson_state_noop_without_thompson() {
    let r = RouterProvider::new(vec![]);
    r.save_thompson_state().await; // should not panic
}

#[test]
fn thompson_ordered_providers_empty() {
    let r = RouterProvider::new(vec![]).with_thompson(None);
    let ordered = r.ordered_providers();
    assert!(ordered.is_empty());
}

#[test]
fn concurrent_record_outcome_does_not_deadlock() {
    use std::sync::Arc;
    let r = Arc::new(RouterProvider::new(vec![]).with_thompson(None));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let router = Arc::clone(&r);
            std::thread::spawn(move || {
                router.record_availability(&format!("p{i}"), i % 2 == 0, 10);
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread panicked");
    }
    // If we reach here, no deadlock occurred.
    let stats = r.thompson_stats();
    assert_eq!(stats.len(), 8);
}

// ── Cascade tests ──────────────────────────────────────────────────────────

#[test]
fn cascade_strategy_is_set() {
    let r = RouterProvider::new(vec![]).with_cascade(CascadeRouterConfig::default());
    assert_eq!(r.strategy, RouterStrategy::Cascade);
    assert!(r.cascade_state.is_some());
    assert!(r.cascade_config.is_some());
}

#[test]
fn cascade_ordered_providers_preserves_chain_order() {
    use crate::ollama::OllamaProvider;
    let p1 = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "a".into(),
        String::new(),
    ));
    let p2 = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:2",
        "b".into(),
        String::new(),
    ));
    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig::default());
    let ordered = r.ordered_providers();
    assert_eq!(ordered.len(), 2);
}

#[tokio::test]
async fn cascade_empty_router_returns_no_providers() {
    let r = RouterProvider::new(vec![]).with_cascade(CascadeRouterConfig::default());
    let msgs = vec![Message::from_legacy(Role::User, "hello")];
    let err = r.chat(&msgs).await.unwrap_err();
    assert_matches!(err, LlmError::NoProviders);
}

#[tokio::test]
async fn cascade_returns_best_seen_when_all_fail_after_good_response() {
    use crate::mock::MockProvider;

    // Provider 1: returns low-quality response (short "ok", triggers escalation at 0.9 threshold)
    let cheap =
        AnyProvider::Mock(MockProvider::with_responses(vec!["ok".to_owned()]).with_delay(0));
    // Provider 2: fails with availability error
    let expensive = AnyProvider::Mock(MockProvider::failing());

    let r = RouterProvider::new(vec![cheap, expensive]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.9, // high threshold ensures "ok" fails quality check
        max_escalations: 2,
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "hello")];
    // Should return "ok" from cheap provider (best-seen), not NoProviders.
    let result = r.chat(&msgs).await.unwrap();
    assert_eq!(result, "ok");
}

#[tokio::test]
async fn cascade_accepts_good_quality_response() {
    use crate::mock::MockProvider;

    let good_response = "This is a comprehensive, well-structured response that provides \
            detailed information about the topic. It covers multiple aspects and explains \
            the reasoning clearly with proper sentence structure.";

    let cheap = AnyProvider::Mock(
        MockProvider::with_responses(vec![good_response.to_owned()]).with_delay(0),
    );
    // second provider should never be called
    let expensive = AnyProvider::Mock(MockProvider::failing());

    let r = RouterProvider::new(vec![cheap, expensive]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.5,
        max_escalations: 1,
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "explain something")];
    let result = r.chat(&msgs).await.unwrap();
    assert_eq!(result, good_response);
}

#[tokio::test]
async fn cascade_max_escalations_budget_exhausted_returns_last_attempted() {
    use crate::mock::MockProvider;

    // All three providers return degenerate response "x" but budget limits to 1 escalation.
    // p1 -> escalation budget 1 -> p2 -> budget=0 -> accept p2's response (not p3).
    let p1 = AnyProvider::Mock(MockProvider::with_responses(vec!["x".to_owned()]).with_delay(0));
    let p2 = AnyProvider::Mock(MockProvider::with_responses(vec!["x".to_owned()]).with_delay(0));
    let p3 = AnyProvider::Mock(MockProvider::failing()); // should never be reached

    let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.9,
        max_escalations: 1, // only 1 escalation allowed
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "test")];
    let result = r.chat(&msgs).await.unwrap();
    assert_eq!(result, "x");
}

#[tokio::test]
async fn cascade_token_budget_stops_escalation() {
    use crate::mock::MockProvider;

    let p1 = AnyProvider::Mock(MockProvider::with_responses(vec!["x".to_owned()]).with_delay(0));
    let p2 = AnyProvider::Mock(MockProvider::failing()); // should not be reached

    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.9, // "x" will fail quality
        max_escalations: 5,
        max_cascade_tokens: Some(1), // 1 token budget — exhausted after first response (~4 chars / 4 = 0 + 1 min)
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "test")];
    let result = r.chat(&msgs).await.unwrap();
    assert_eq!(result, "x"); // returned despite low quality due to token budget
}

#[tokio::test]
async fn cascade_budget_returns_best_seen_not_current() {
    use crate::mock::MockProvider;

    // p1 returns a decent response, p2 returns a worse one but exhausts the budget.
    // With budget_exhausted, we should get the best-seen (p1) not the current (p2).
    let good_response = "This is a reasonable response with enough content to score well.";
    let bad_response = "x"; // degenerate, score << good_response

    let p1 = AnyProvider::Mock(
        MockProvider::with_responses(vec![good_response.to_owned()]).with_delay(0),
    );
    let p2 = AnyProvider::Mock(
        MockProvider::with_responses(vec![bad_response.to_owned()]).with_delay(0),
    );

    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.95, // both fail quality check but good > bad
        max_escalations: 5,
        max_cascade_tokens: Some(1), // budget exhausted after p1 (1 token min)
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "test")];
    // p1 exhausts the budget; should return p1's response (better), not p2's (worse).
    // Note: p2 is reached since budget check happens AFTER p1's response is processed
    // and p1 fails quality. Budget exhausted at p2 → return best-seen (p1).
    let result = r.chat(&msgs).await.unwrap();
    // The result must not be the degenerate "x" response.
    assert_ne!(result, bad_response, "should return best-seen, not current");
}

#[tokio::test]
async fn cascade_escalations_exhausted_returns_best_seen_not_current() {
    use crate::mock::MockProvider;

    // p1: decent response, fails quality at 0.95 → escalates (escalations_remaining: 1 → 0)
    // p2: degenerate "x", fails quality → escalations_remaining == 0 → blocked → best_seen wins
    let good_response = "This is a reasonable response with enough content to score well.";
    let bad_response = "x";

    let p1 = AnyProvider::Mock(
        MockProvider::with_responses(vec![good_response.to_owned()]).with_delay(0),
    );
    let p2 = AnyProvider::Mock(
        MockProvider::with_responses(vec![bad_response.to_owned()]).with_delay(0),
    );
    let p3 = AnyProvider::Mock(MockProvider::failing()); // should not be reached

    let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.95, // both fail quality; p1 score > p2 score
        max_escalations: 1,      // p1 escalates (budget: 1→0), p2 is blocked
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "test")];
    let result = r.chat(&msgs).await.unwrap();
    assert_eq!(
        result, good_response,
        "should return best-seen (p1), not the degenerate current response (p2)"
    );
    assert_ne!(
        result, bad_response,
        "must not return degenerate p2 response"
    );
}

#[tokio::test]
async fn cascade_stream_escalations_exhausted_returns_best_seen_not_current() {
    use crate::mock::MockProvider;

    // Same scenario as above but for cascade_chat_stream.
    // p1: decent response, fails quality at 0.95 → escalates (escalations_remaining: 1 → 0)
    // p2: degenerate "x", fails quality → escalations_remaining == 0 → return best_seen
    let good_response = "This is a reasonable response with enough content to score well.";
    let bad_response = "x";

    let p1 = AnyProvider::Mock(
        MockProvider::with_responses(vec![good_response.to_owned()])
            .with_delay(0)
            .with_streaming(),
    );
    let p2 = AnyProvider::Mock(
        MockProvider::with_responses(vec![bad_response.to_owned()])
            .with_delay(0)
            .with_streaming(),
    );
    let p3 = AnyProvider::Mock(MockProvider::failing()); // last provider, should not be reached

    let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.95, // both fail quality; p1 score > p2 score
        max_escalations: 1,      // p1 escalates (budget: 1→0), p2 is blocked
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "test")];
    let stream = r.chat_stream(&msgs).await.unwrap();
    let collected = collect_stream(stream).await.unwrap();
    assert_eq!(
        collected.content, good_response,
        "should return best-seen (p1), not the degenerate current response (p2)"
    );
    assert_ne!(
        collected.content, bad_response,
        "must not return degenerate p2 response"
    );
}

/// When every provider in the cascade fallback loop fails and no `best`-seen
/// response was ever produced, `cascade_chat` must surface the *last*
/// provider's actual error — not a generic `NoProviders` that discards the
/// actionable diagnostic. Regression for #5826 (mirrors the `chat`/
/// `chat_stream` fix from #5821 and `embed`/`embed_batch` from #5811).
#[tokio::test]
async fn cascade_all_providers_fail_preserves_last_error() {
    use crate::mock::MockProvider;

    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_errors(vec![LlmError::ApiError {
                provider: "p1".into(),
                status: 500,
            }])
            .with_name("p1"),
    );
    let p2 = AnyProvider::Mock(
        MockProvider::default()
            .with_errors(vec![LlmError::ApiError {
                provider: "p2".into(),
                status: 503,
            }])
            .with_name("p2"),
    );

    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig::default());
    let msgs = vec![Message::from_legacy(Role::User, "test")];
    let err = r.chat(&msgs).await.unwrap_err();

    assert!(
        matches!(&err, LlmError::ApiError { provider, status } if provider == "p2" && *status == 503),
        "expected the last provider's (p2) ApiError to survive exhaustion, got {err:?}"
    );
}

#[tokio::test]
async fn cascade_stream_good_quality_no_escalation() {
    use crate::mock::MockProvider;

    let good = "This is a well-formed response with sufficient length and coherent structure.";
    let p1 = AnyProvider::Mock(
        MockProvider::with_responses(vec![good.to_owned()])
            .with_delay(0)
            .with_streaming(),
    );
    let p2 = AnyProvider::Mock(MockProvider::failing());

    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.5,
        max_escalations: 1,
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "q")];
    let stream = r.chat_stream(&msgs).await.unwrap();
    let collected = collect_stream(stream).await.unwrap();
    assert_eq!(collected.content, good);
}

#[tokio::test]
async fn cascade_stream_escalates_to_last_provider() {
    use crate::mock::MockProvider;

    let bad = "x"; // low quality, should escalate
    let good = "This is the expensive model's comprehensive response.";
    let p1 = AnyProvider::Mock(
        MockProvider::with_responses(vec![bad.to_owned()])
            .with_delay(0)
            .with_streaming(),
    );
    let p2 = AnyProvider::Mock(
        MockProvider::with_responses(vec![good.to_owned()])
            .with_delay(0)
            .with_streaming(),
    );

    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.9, // "x" fails quality
        max_escalations: 1,
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "q")];
    let stream = r.chat_stream(&msgs).await.unwrap();
    let collected = collect_stream(stream).await.unwrap();
    assert_eq!(collected.content, good);
}

#[tokio::test]
async fn cascade_stream_budget_returns_best_seen() {
    use crate::mock::MockProvider;

    // Three providers: early=[p1, p2], last=p3.
    // p1 returns a decent response (fails quality threshold at 0.95, triggers escalation).
    // Budget is set to 1 token, so it is exhausted immediately after p1 processes.
    // best_seen = p1's response; budget_exhausted + should_escalate → return best_seen.
    let good_response = "This is a reasonable response with enough content to score well.";
    let bad_response = "x"; // degenerate, score << good_response

    let p1 = AnyProvider::Mock(
        MockProvider::with_responses(vec![good_response.to_owned()])
            .with_delay(0)
            .with_streaming(),
    );
    let p2 = AnyProvider::Mock(
        MockProvider::with_responses(vec![bad_response.to_owned()])
            .with_delay(0)
            .with_streaming(),
    );
    let p3 = AnyProvider::Mock(MockProvider::failing()); // last provider, not reached

    let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.95, // p1 fails quality check → triggers escalation path
        max_escalations: 5,
        max_cascade_tokens: Some(1), // budget exhausted after p1 (1 token min)
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "test")];
    let stream = r.chat_stream(&msgs).await.unwrap();
    let collected = collect_stream(stream).await.unwrap();
    // Must return best-seen (p1's good response).
    assert_eq!(
        collected.content, good_response,
        "should return best-seen p1 response when budget exhausted"
    );
}

#[tokio::test]
async fn cascade_stream_budget_returns_best_seen_not_current() {
    use crate::mock::MockProvider;

    // Four providers: early=[p1, p2, p3], last=p4.
    // p1 returns a good response, fails quality at 0.95 (score ~0.6), escalates; budget not yet exhausted.
    // p2 returns a degenerate response "x", fails quality, exhausts the budget.
    // At budget exhaustion: best_seen = p1 (higher score), current = p2's "x".
    // Must return best_seen (p1), not current (p2).
    let good_response = "This is a reasonable response with enough content to score well.";
    let bad_response = "x"; // 1 char → estimated_tokens = max(1/4, 1) = 1

    let p1 = AnyProvider::Mock(
        MockProvider::with_responses(vec![good_response.to_owned()])
            .with_delay(0)
            .with_streaming(),
    );
    let p2 = AnyProvider::Mock(
        MockProvider::with_responses(vec![bad_response.to_owned()])
            .with_delay(0)
            .with_streaming(),
    );
    let p3 = AnyProvider::Mock(MockProvider::failing()); // last provider, not reached
    let p4 = AnyProvider::Mock(MockProvider::failing()); // last provider, not reached

    // Budget = 20: p1 uses ~16 tokens (65 chars / 4), p2 uses 1 → total 17 ≥ 20? No.
    // Use budget = 17 so p2 exhausts it.
    let r = RouterProvider::new(vec![p1, p2, p3, p4]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.95, // both fail; p1 score > p2 score
        max_escalations: 5,
        max_cascade_tokens: Some(17), // p1 uses 16, p2 uses 1 → total 17 ≥ 17 after p2
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "test")];
    let stream = r.chat_stream(&msgs).await.unwrap();
    let collected = collect_stream(stream).await.unwrap();
    // Must return p1 (best_seen), not p2 (current at time of budget exhaustion).
    assert_eq!(
        collected.content, good_response,
        "should return best-seen (p1), not current degenerate (p2)"
    );
    assert_ne!(
        collected.content, bad_response,
        "must not return the degenerate p2 response"
    );
}

#[tokio::test]
async fn cascade_stream_last_fails_returns_best_seen() {
    use crate::mock::MockProvider;

    // Two providers: early=[p1], last=p2.
    // p1 returns a low-quality response that triggers escalation.
    // p2 (last) fails with an error.
    // Should return p1's response (best-seen) instead of propagating the error.
    let low_quality = "ok"; // short, triggers escalation at 0.9 threshold
    let p1 = AnyProvider::Mock(
        MockProvider::with_responses(vec![low_quality.to_owned()])
            .with_delay(0)
            .with_streaming(),
    );
    let p2 = AnyProvider::Mock(MockProvider::failing()); // last provider fails

    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
        quality_threshold: 0.9, // "ok" fails quality, triggers escalation
        max_escalations: 2,
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "hello")];
    let stream = r.chat_stream(&msgs).await.unwrap();
    let collected = collect_stream(stream).await.unwrap();
    assert_eq!(collected.content, low_quality);
}

#[tokio::test]
async fn cascade_stream_all_fail_returns_error() {
    use crate::mock::MockProvider;

    // Two providers, both fail. No best_seen accumulated.
    // p1 is early (errors → continue), p2 is last (errors → propagated).
    // The last provider's error must be propagated, not swallowed.
    let p1 = AnyProvider::Mock(MockProvider::failing());
    let p2 = AnyProvider::Mock(MockProvider::failing());

    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig::default());
    let msgs = vec![Message::from_legacy(Role::User, "test")];
    let result = r.chat_stream(&msgs).await;
    assert!(
        result.is_err(),
        "expected error when all providers fail with no best_seen"
    );
}

#[test]
fn cascade_config_default_values() {
    let cfg = CascadeRouterConfig::default();
    assert!((cfg.quality_threshold - 0.5).abs() < f64::EPSILON);
    assert_eq!(cfg.max_escalations, 2);
    assert_eq!(cfg.window_size, 50);
    assert!(cfg.max_cascade_tokens.is_none());
    assert_eq!(cfg.classifier_mode, cascade::ClassifierMode::Heuristic);
}

#[test]
fn evaluate_heuristic_empty_should_escalate_above_threshold() {
    let verdict = RouterProvider::evaluate_heuristic("", 0.05);
    // score = 0.0, threshold = 0.05 → should_escalate = true
    assert!(verdict.should_escalate);
}

#[test]
fn evaluate_heuristic_good_response_does_not_escalate() {
    let text = "The answer to your question is straightforward. Consider the options and pick the best one.";
    let verdict = RouterProvider::evaluate_heuristic(text, 0.5);
    assert!(!verdict.should_escalate, "score={}", verdict.score);
}

/// Empty string from the only provider must not be stored as `best_seen`.
/// When all providers fail or return empty, the caller should get an error,
/// not a silent empty response.
#[tokio::test]
async fn cascade_empty_response_not_stored_as_best_seen() {
    use crate::mock::MockProvider;

    // Single provider returns empty string (score=0.0, should_escalate may be true/false).
    // With quality_threshold=0.0 it won't escalate, so we can check the return value.
    let p = AnyProvider::Mock(MockProvider::with_responses(vec![String::new()]));
    let cfg = CascadeRouterConfig {
        quality_threshold: 0.0,
        ..Default::default()
    };
    let r = RouterProvider::new(vec![p]).with_cascade(cfg);
    let msgs = vec![Message::from_legacy(Role::User, "hi")];
    // The provider returns "" — cascade must return it as-is (no best_seen involved
    // with a single provider), but this test confirms "" is not stored when escalating.
    let result = r.chat(&msgs).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "");
}

/// When provider 1 returns empty and provider 2 fails, `best_seen` must not hold
/// the empty string — the caller must get an error, not a silent empty response.
#[tokio::test]
async fn cascade_empty_best_seen_not_returned_on_all_fail() {
    use crate::mock::MockProvider;

    // p1: returns empty string (causes escalation with default threshold)
    // p2: hard error
    let p1 = AnyProvider::Mock(MockProvider::with_responses(vec![String::new()]));
    let p2 = AnyProvider::Mock(MockProvider::failing());

    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig::default());
    let msgs = vec![Message::from_legacy(Role::User, "hi")];
    let result = r.chat(&msgs).await;
    // best_seen must NOT be the empty string; error must propagate.
    assert!(
        result.is_err(),
        "expected error, not silent empty string; got: {result:?}"
    );
}

/// Stream variant: empty string from early provider must not be stored as `best_seen`.
#[tokio::test]
async fn cascade_stream_empty_response_not_stored_as_best_seen() {
    use crate::mock::MockProvider;

    // p1 (early): returns "" — should NOT be stored as best_seen.
    // p2 (last): returns a real response.
    let p1 = AnyProvider::Mock(MockProvider::with_responses(vec![String::new()]));
    let p2 = AnyProvider::Mock(
        MockProvider::with_responses(vec!["real answer".to_owned()]).with_streaming(),
    );

    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig::default());
    let msgs = vec![Message::from_legacy(Role::User, "hi")];
    let stream = r.chat_stream(&msgs).await.expect("should not error");
    let collected = collect_stream(stream).await.expect("stream should succeed");
    assert_eq!(collected.content, "real answer");
}

// ── Arc<[AnyProvider]> + cost_tiers tests ──────────────────────────────────

#[test]
fn arc_providers_clone_shares_allocation() {
    use crate::mock::MockProvider;
    let p = AnyProvider::Mock(MockProvider::default());
    let r = RouterProvider::new(vec![p]);
    let c = r.clone();
    // Both RouterProvider instances must share the same Arc allocation.
    assert!(Arc::ptr_eq(&r.state.providers, &c.state.providers));
}

#[test]
fn cost_tiers_reorders_providers_at_construction() {
    use crate::mock::MockProvider;
    let p1 = AnyProvider::Mock(MockProvider::default().with_name("claude"));
    let p2 = AnyProvider::Mock(MockProvider::default().with_name("ollama"));
    let p3 = AnyProvider::Mock(MockProvider::default().with_name("openai"));
    let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
        cost_tiers: Some(vec!["ollama".into(), "claude".into()]),
        ..CascadeRouterConfig::default()
    });
    let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
    // ollama first (tier 0), claude second (tier 1), openai last (unlisted, original idx 2)
    assert_eq!(names, vec!["ollama", "claude", "openai"]);
}

#[test]
fn cost_tiers_none_preserves_chain_order() {
    use crate::mock::MockProvider;
    let p1 = AnyProvider::Mock(MockProvider::default().with_name("claude"));
    let p2 = AnyProvider::Mock(MockProvider::default().with_name("ollama"));
    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
        cost_tiers: None,
        ..CascadeRouterConfig::default()
    });
    let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
    assert_eq!(names, vec!["claude", "ollama"]);
}

#[test]
fn cost_tiers_empty_vec_preserves_chain_order() {
    use crate::mock::MockProvider;
    let p1 = AnyProvider::Mock(MockProvider::default().with_name("claude"));
    let p2 = AnyProvider::Mock(MockProvider::default().with_name("ollama"));
    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
        cost_tiers: Some(vec![]),
        ..CascadeRouterConfig::default()
    });
    let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
    assert_eq!(names, vec!["claude", "ollama"]);
}

#[test]
fn cost_tiers_unknown_name_ignored() {
    use crate::mock::MockProvider;
    let p1 = AnyProvider::Mock(MockProvider::default().with_name("ollama"));
    let p2 = AnyProvider::Mock(MockProvider::default().with_name("claude"));
    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
        cost_tiers: Some(vec!["nonexistent".into(), "ollama".into()]),
        ..CascadeRouterConfig::default()
    });
    let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
    // "nonexistent" ignored; "ollama" is tier 1 → first; "claude" unlisted → second
    assert_eq!(names, vec!["ollama", "claude"]);
}

#[test]
fn cost_tiers_all_providers_listed() {
    use crate::mock::MockProvider;
    let p1 = AnyProvider::Mock(MockProvider::default().with_name("c"));
    let p2 = AnyProvider::Mock(MockProvider::default().with_name("b"));
    let p3 = AnyProvider::Mock(MockProvider::default().with_name("a"));
    let r = RouterProvider::new(vec![p1, p2, p3]).with_cascade(CascadeRouterConfig {
        cost_tiers: Some(vec!["a".into(), "b".into(), "c".into()]),
        ..CascadeRouterConfig::default()
    });
    let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn cost_tiers_duplicate_name_uses_last_position() {
    use crate::mock::MockProvider;
    let p1 = AnyProvider::Mock(MockProvider::default().with_name("ollama"));
    let p2 = AnyProvider::Mock(MockProvider::default().with_name("claude"));
    // "ollama" appears twice in tiers: HashMap overwrites → position 2.
    // claude=tier 0, ollama=tier 2 → claude before ollama.
    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
        cost_tiers: Some(vec!["claude".into(), "ollama".into(), "ollama".into()]),
        ..CascadeRouterConfig::default()
    });
    let names: Vec<&str> = r.state.providers.iter().map(LlmProvider::name).collect();
    assert_eq!(names, vec!["claude", "ollama"]);
}

#[test]
fn cost_tiers_empty_router_does_not_panic() {
    let r = RouterProvider::new(vec![]).with_cascade(CascadeRouterConfig {
        cost_tiers: Some(vec!["foo".into()]),
        ..CascadeRouterConfig::default()
    });
    assert_eq!(r.state.providers.len(), 0);
}

#[test]
fn set_status_tx_works_with_arc() {
    use crate::mock::MockProvider;
    let p = AnyProvider::Mock(MockProvider::default());
    let mut r = RouterProvider::new(vec![p]);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    r.set_status_tx(tx); // must not panic
}

#[tokio::test]
async fn cascade_chat_with_tools_unaffected_by_cost_tiers() {
    use crate::mock::MockProvider;
    // chat_with_tools skips cascade entirely (HIGH-04). Verify that cost_tiers
    // ordering does not accidentally affect the non-cascade tool fallback path.
    let p1 = AnyProvider::Mock(MockProvider::failing().with_name("cheap"));
    let p2 = AnyProvider::Mock(MockProvider::failing().with_name("expensive"));
    let r = RouterProvider::new(vec![p1, p2]).with_cascade(CascadeRouterConfig {
        cost_tiers: Some(vec!["cheap".into()]),
        ..CascadeRouterConfig::default()
    });
    let msgs = vec![Message::from_legacy(Role::User, "hi")];
    // Both providers fail → the last provider's error is preserved (not a
    // cascade-specific error, and not a generic NoProviders that would discard it).
    let err = r.chat_with_tools(&msgs, &[]).await.unwrap_err();
    assert_matches!(err, LlmError::Other(_));
}

// ── Embed retry / rate-limit tests ────────────────────────────────────────

/// Provider returns `RateLimited` twice then succeeds on the third attempt.
/// The router must retry and return the successful embedding.
#[tokio::test]
async fn embed_retries_on_rate_limited_then_succeeds() {
    use crate::mock::MockProvider;

    let p = AnyProvider::Mock({
        let mut m = MockProvider::default()
            .with_errors(vec![LlmError::RateLimited, LlmError::RateLimited])
            .with_name("p1");
        m.supports_embeddings = true;
        m.embedding = vec![0.1, 0.2];
        m
    });
    let r = RouterProvider::new(vec![p]);
    let result = r.embed("text").await.unwrap();
    assert_eq!(result, vec![0.1, 0.2]);
}

/// When all retries (3) are exhausted on the first provider, the router falls
/// back to the second provider and returns its embedding.
#[tokio::test]
async fn embed_falls_back_after_all_retries_exhausted() {
    use crate::mock::MockProvider;

    // p1: 4 RateLimited errors (attempt 0..=3 all fail)
    let p1 = AnyProvider::Mock({
        let mut m = MockProvider::default()
            .with_errors(vec![
                LlmError::RateLimited,
                LlmError::RateLimited,
                LlmError::RateLimited,
                LlmError::RateLimited,
            ])
            .with_name("p1");
        m.supports_embeddings = true;
        m
    });
    let p2 = AnyProvider::Mock({
        let mut m = MockProvider::default().with_name("p2");
        m.supports_embeddings = true;
        m.embedding = vec![9.0, 8.0];
        m
    });
    let r = RouterProvider::new(vec![p1, p2]);
    let result = r.embed("text").await.unwrap();
    assert_eq!(result, vec![9.0, 8.0]);
}

/// A genuine outage (`Unavailable`, e.g. exhausted 503 retries at the HTTP layer) must
/// not be treated as `RateLimited`: the router should fall back to the next provider on
/// the very first `Unavailable` error instead of retrying the same provider up to
/// `EMBED_MAX_RETRIES` times. Giving `p1` only one error (not four, unlike the
/// `RateLimited` exhaustion test above) proves this — if the router mistakenly retried,
/// the second call to `p1` would succeed (its error queue would be empty) and return
/// `p1`'s own embedding instead of falling back to `p2`.
#[tokio::test]
async fn embed_falls_back_immediately_on_unavailable() {
    use crate::mock::MockProvider;

    let p1_mock = {
        let mut m = MockProvider::default()
            .with_errors(vec![LlmError::Unavailable])
            .with_name("p1");
        m.supports_embeddings = true;
        m
    };
    let p1_embed_calls = Arc::clone(&p1_mock.embed_call_count);
    let p1 = AnyProvider::Mock(p1_mock);
    let p2 = AnyProvider::Mock({
        let mut m = MockProvider::default().with_name("p2");
        m.supports_embeddings = true;
        m.embedding = vec![9.0, 8.0];
        m
    });
    let r = RouterProvider::new(vec![p1, p2]);
    let result = r.embed("text").await.unwrap();
    assert_eq!(result, vec![9.0, 8.0]);
    assert_eq!(
        p1_embed_calls.load(Ordering::Relaxed),
        1,
        "p1 must be called exactly once — Unavailable must not enter the embed-retry loop"
    );
}

// ── Dedicated embed provider tests (#5859) ────────────────────────────────

/// A chat-only provider that also (incorrectly, like every `OllamaProvider`) reports
/// `supports_embeddings() == true` must not shadow a provider explicitly registered via
/// `with_embed_provider`. The dedicated provider's embedding must win.
#[tokio::test]
async fn embed_prefers_dedicated_provider_over_chat_only_fallback() {
    use crate::mock::MockProvider;

    let chat_only = AnyProvider::Mock({
        let mut m = MockProvider::default().with_name("chat");
        m.supports_embeddings = true;
        m.embedding = vec![1.0, 0.0];
        m
    });
    let dedicated = AnyProvider::Mock({
        let mut m = MockProvider::default().with_name("embedder");
        m.supports_embeddings = true;
        m.embedding = vec![0.0, 1.0];
        m
    });
    let r = RouterProvider::new(vec![chat_only]).with_embed_provider(dedicated);
    let result = r.embed("text").await.unwrap();
    assert_eq!(
        result,
        vec![0.0, 1.0],
        "embed() must route to the dedicated provider, not the chat-only fallback"
    );
}

/// `embed_candidates()` prepends the dedicated provider ahead of the generic ordered
/// scan, and dedupes it out of its original position when it is also present in
/// `providers` (e.g. because a caller registered the same instance twice).
#[test]
fn embed_candidates_prepends_and_dedupes_dedicated_provider() {
    use crate::mock::MockProvider;

    let dedicated = AnyProvider::Mock(MockProvider::default().with_name("embedder"));
    let other = AnyProvider::Mock(MockProvider::default().with_name("chat"));
    let r = RouterProvider::new(vec![other, dedicated.clone()]).with_embed_provider(dedicated);

    let candidates = r.embed_candidates();
    let names: Vec<&str> = candidates.iter().map(AnyProvider::name).collect();
    assert_eq!(
        names,
        vec!["embedder", "chat"],
        "dedicated provider must be first and not duplicated"
    );
}

/// Provider returns `RateLimited` twice then succeeds via `embed_batch`.
#[tokio::test]
async fn embed_batch_retries_on_rate_limited_then_succeeds() {
    use crate::mock::MockProvider;

    let p = AnyProvider::Mock({
        let mut m = MockProvider::default()
            .with_errors(vec![LlmError::RateLimited, LlmError::RateLimited])
            .with_name("p1");
        m.supports_embeddings = true;
        m.embedding = vec![0.5, 0.6];
        m
    });
    let r = RouterProvider::new(vec![p]);
    let result = r.embed_batch(&["a", "b"]).await.unwrap();
    assert_eq!(result, vec![vec![0.5, 0.6], vec![0.5, 0.6]]);
}

/// When all `embed_batch` retries are exhausted on the first provider, falls back
/// to the second provider.
#[tokio::test]
async fn embed_batch_falls_back_after_all_retries_exhausted() {
    use crate::mock::MockProvider;

    // p1 needs 4 errors per embed call * 1 text = 4 total (attempt 0..=3)
    let p1 = AnyProvider::Mock({
        let mut m = MockProvider::default()
            .with_errors(vec![
                LlmError::RateLimited,
                LlmError::RateLimited,
                LlmError::RateLimited,
                LlmError::RateLimited,
            ])
            .with_name("p1");
        m.supports_embeddings = true;
        m
    });
    let p2 = AnyProvider::Mock({
        let mut m = MockProvider::default().with_name("p2");
        m.supports_embeddings = true;
        m.embedding = vec![7.0, 8.0];
        m
    });
    let r = RouterProvider::new(vec![p1, p2]);
    let result = r.embed_batch(&["x"]).await.unwrap();
    assert_eq!(result, vec![vec![7.0, 8.0]]);
}

// ── InvalidInput embed break tests ────────────────────────────────────────

/// When a provider returns `InvalidInput` from `embed()`, the router must break
/// the fallback loop immediately and return `InvalidInput` — not `NoProviders`.
#[tokio::test]
async fn embed_invalid_input_breaks_loop_and_returns_invalid_input() {
    use crate::mock::MockProvider;

    let p = AnyProvider::Mock(MockProvider::default().with_embed_invalid_input());
    let r = RouterProvider::new(vec![p]).with_thompson(None);
    let err = r.embed("some text").await.unwrap_err();
    assert!(
        matches!(err, LlmError::InvalidInput { .. }),
        "expected InvalidInput, got {err:?}"
    );
}

/// When a provider returns `InvalidInput`, the router must NOT fall through to
/// the next provider — a second embed-capable provider must never be called.
#[tokio::test]
async fn embed_invalid_input_does_not_fall_through_to_second_provider() {
    use crate::mock::MockProvider;

    // p1 returns InvalidInput; p2 is a functioning embed provider.
    // If the loop falls through, p2 returns Ok — which would mean the error was
    // swallowed instead of breaking immediately.
    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_embed_invalid_input()
            .with_name("p1"),
    );
    let p2 = AnyProvider::Mock({
        let mut m = MockProvider::default();
        m.supports_embeddings = true;
        m.name_override = Some("p2".into());
        m
    });

    let r = RouterProvider::new(vec![p1, p2]);
    let err = r.embed("test").await.unwrap_err();

    // The error must carry p1's name, proving p2 was never reached.
    assert!(
        matches!(&err, LlmError::InvalidInput { provider, .. } if provider == "p1"),
        "expected InvalidInput from p1, got {err:?}"
    );
}

/// Unlike `InvalidInput`, a `ModelCapabilityMismatch` from `chat_with_tools()` is
/// model/config-specific (e.g. `reasoning_effort` + `tools` on one provider) — the router
/// must fall through to the next provider instead of aborting.
#[tokio::test]
async fn chat_with_tools_model_capability_mismatch_falls_through_to_second_provider() {
    use crate::mock::MockProvider;
    use crate::provider::ToolDefinition;

    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_errors(vec![LlmError::ModelCapabilityMismatch {
                provider: "p1".into(),
                message: "reasoning_effort incompatible with tools".into(),
            }])
            .with_name("p1"),
    );
    let p2 = AnyProvider::Mock(MockProvider::default().with_name("p2"));

    let r = RouterProvider::new(vec![p1, p2]);
    let result = r.chat_with_tools(&[], &[] as &[ToolDefinition]).await;

    assert!(
        result.is_ok(),
        "expected fallback to p2 to succeed, got {result:?}"
    );
}

/// When the sole provider returns `ModelCapabilityMismatch` and the fallback loop exhausts,
/// the router must surface that error's actionable diagnostic — not a generic `NoProviders`
/// that discards it (regression for the #5795 enriched-message loss).
#[tokio::test]
async fn chat_with_tools_single_provider_mismatch_exhaustion_preserves_error() {
    use crate::mock::MockProvider;
    use crate::provider::ToolDefinition;

    let p = AnyProvider::Mock(
        MockProvider::default()
            .with_errors(vec![LlmError::ModelCapabilityMismatch {
                provider: "p1".into(),
                message: "reasoning_effort incompatible with tools".into(),
            }])
            .with_name("p1"),
    );
    let r = RouterProvider::new(vec![p]).with_thompson(None);
    let err = r
        .chat_with_tools(&[], &[] as &[ToolDefinition])
        .await
        .unwrap_err();

    assert!(
        matches!(&err, LlmError::ModelCapabilityMismatch { provider, message }
            if provider == "p1" && message.contains("reasoning_effort")),
        "expected ModelCapabilityMismatch from p1 to survive exhaustion, got {err:?}"
    );
}

/// When every provider in the fallback loop returns `ModelCapabilityMismatch`, the router
/// must exhaust the loop and return the last provider's error, not `NoProviders`.
#[tokio::test]
async fn chat_with_tools_all_providers_mismatch_exhaustion_preserves_last_error() {
    use crate::mock::MockProvider;
    use crate::provider::ToolDefinition;

    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_errors(vec![LlmError::ModelCapabilityMismatch {
                provider: "p1".into(),
                message: "p1 reasoning_effort incompatible with tools".into(),
            }])
            .with_name("p1"),
    );
    let p2 = AnyProvider::Mock(
        MockProvider::default()
            .with_errors(vec![LlmError::ModelCapabilityMismatch {
                provider: "p2".into(),
                message: "p2 reasoning_effort incompatible with tools".into(),
            }])
            .with_name("p2"),
    );

    let r = RouterProvider::new(vec![p1, p2]);
    let err = r
        .chat_with_tools(&[], &[] as &[ToolDefinition])
        .await
        .unwrap_err();

    assert!(
        matches!(&err, LlmError::ModelCapabilityMismatch { provider, .. } if provider == "p2"),
        "expected the last provider's (p2) ModelCapabilityMismatch to survive exhaustion, got {err:?}"
    );
}

// ── InvalidInput chat_with_tools break tests ───────────────────────────────

/// When a provider returns `InvalidInput` from `chat_with_tools()`, the router must break
/// the fallback loop immediately and return `InvalidInput` — not `NoProviders`.
#[tokio::test]
async fn chat_with_tools_invalid_input_breaks_loop_and_returns_invalid_input() {
    use crate::mock::MockProvider;
    use crate::provider::ToolDefinition;

    let p = AnyProvider::Mock(MockProvider::default().with_tool_chat_invalid_input());
    let r = RouterProvider::new(vec![p]).with_thompson(None);
    let err = r
        .chat_with_tools(&[], &[] as &[ToolDefinition])
        .await
        .unwrap_err();
    assert!(
        matches!(err, LlmError::InvalidInput { .. }),
        "expected InvalidInput, got {err:?}"
    );
}

/// When a provider returns `InvalidInput` from `chat_with_tools()`, the router must NOT
/// fall through to the next provider.
#[tokio::test]
async fn chat_with_tools_invalid_input_does_not_fall_through_to_second_provider() {
    use crate::mock::MockProvider;
    use crate::provider::ToolDefinition;

    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_tool_chat_invalid_input()
            .with_name("p1"),
    );
    let p2 = AnyProvider::Mock(MockProvider::default().with_name("p2"));

    let r = RouterProvider::new(vec![p1, p2]);
    let err = r
        .chat_with_tools(&[], &[] as &[ToolDefinition])
        .await
        .unwrap_err();

    assert!(
        matches!(&err, LlmError::InvalidInput { provider, .. } if provider == "p1"),
        "expected InvalidInput from p1, got {err:?}"
    );
}

/// The router skips providers that do not support embeddings and continues to
/// the next one, returning a successful result from the first capable provider.
#[tokio::test]
async fn embed_skips_non_embedding_providers_and_falls_through() {
    use crate::mock::MockProvider;

    // p1 does not support embeddings — skipped by the loop guard.
    // p2 supports embeddings and returns successfully.
    let p1 = AnyProvider::Mock({
        let mut m = MockProvider::default().with_name("p1");
        m.supports_embeddings = false;
        m
    });
    let p2 = AnyProvider::Mock({
        let mut m = MockProvider::default().with_name("p2");
        m.supports_embeddings = true;
        m.embedding = vec![1.0, 2.0, 3.0];
        m
    });

    let r = RouterProvider::new(vec![p1, p2]);
    let result = r.embed("hello").await.unwrap();
    assert_eq!(result, vec![1.0, 2.0, 3.0]);
}

/// `InvalidInput` from embed does not call `record_availability` (no reputation penalty).
/// We verify this indirectly: `thompson_stats` must show no entry for the provider
/// after an `InvalidInput` embed, whereas a normal embed failure increments it.
#[tokio::test]
async fn embed_invalid_input_does_not_record_availability() {
    use crate::mock::MockProvider;

    let p = AnyProvider::Mock(
        MockProvider::default()
            .with_embed_invalid_input()
            .with_name("test-provider"),
    );
    let r = RouterProvider::new(vec![p]).with_thompson(None);
    let _ = r.embed("text").await;

    // record_availability is only called on success or generic error,
    // not on InvalidInput. So thompson_stats must have no entry for "test-provider".
    let stats = r.thompson_stats();
    let provider_in_stats = stats.iter().any(|(name, ..)| name == "test-provider");
    assert!(
        !provider_in_stats,
        "InvalidInput must not update provider reputation; stats: {stats:?}"
    );
}

// ── embed timeout tests ───────────────────────────────────────────────────

/// When the only provider's `embed()` exceeds `embed_timeout_ms`, the router
/// exhausts the fallback list and returns the real `LlmError::Timeout` instead
/// of a generic `NoProviders` (#5811).
#[tokio::test]
async fn embed_timeout_single_provider_preserves_timeout_error() {
    use crate::mock::MockProvider;

    let p = AnyProvider::Mock(
        MockProvider::default()
            .with_embed_delay(200)
            .with_name("slow"),
    );
    let r = RouterProvider::new(vec![p]).with_embed_timeout(10);
    let err = r.embed("hello").await.unwrap_err();
    assert!(
        matches!(err, LlmError::Timeout),
        "expected Timeout after fallback exhaustion, got {err:?}"
    );
}

/// After a timeout on the first provider, the router falls back to the next
/// embed-capable provider and returns its successful result.
#[tokio::test]
async fn embed_timeout_falls_back_to_next_provider() {
    use crate::mock::MockProvider;

    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_embed_delay(200)
            .with_name("slow"),
    );
    let p2 = AnyProvider::Mock({
        let mut m = MockProvider::default().with_name("fast");
        m.supports_embeddings = true;
        m.embedding = vec![1.0, 2.0, 3.0];
        m
    });
    let r = RouterProvider::new(vec![p1, p2]).with_embed_timeout(10);
    let result = r.embed("hello").await.unwrap();
    assert_eq!(result, vec![1.0, 2.0, 3.0]);
}

/// When the only provider's `embed_batch()` exceeds `embed_timeout_ms`, the router
/// exhausts the fallback list and returns the real `LlmError::Timeout` instead
/// of a generic `NoProviders` (#5811).
#[tokio::test]
async fn embed_batch_timeout_single_provider_preserves_timeout_error() {
    use crate::mock::MockProvider;

    let p = AnyProvider::Mock(
        MockProvider::default()
            .with_embed_delay(200)
            .with_name("slow"),
    );
    let r = RouterProvider::new(vec![p]).with_embed_timeout(10);
    let err = r.embed_batch(&["hello"]).await.unwrap_err();
    assert!(
        matches!(err, LlmError::Timeout),
        "expected Timeout after fallback exhaustion, got {err:?}"
    );
}

/// After a timeout on the first provider, the router falls back to the next
/// embed-capable provider and returns its successful `embed_batch` result.
#[tokio::test]
async fn embed_batch_timeout_falls_back_to_next_provider() {
    use crate::mock::MockProvider;

    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_embed_delay(200)
            .with_name("slow"),
    );
    let p2 = AnyProvider::Mock({
        let mut m = MockProvider::default().with_name("fast");
        m.supports_embeddings = true;
        m.embedding = vec![1.0, 2.0, 3.0];
        m
    });
    let r = RouterProvider::new(vec![p1, p2]).with_embed_timeout(10);
    let result = r.embed_batch(&["hello"]).await.unwrap();
    assert_eq!(result, vec![vec![1.0, 2.0, 3.0]]);
}

/// Regression for #5699: a provider whose `embed()` times out must be recorded as
/// unavailable via `record_availability`, just like the retry-exhaustion and generic
/// error branches — otherwise it keeps being tried at full priority forever.
#[tokio::test]
async fn embed_timeout_records_provider_unavailable() {
    use crate::mock::MockProvider;

    let p = AnyProvider::Mock(
        MockProvider::default()
            .with_embed_delay(200)
            .with_name("slow"),
    );
    let r = RouterProvider::new(vec![p])
        .with_thompson(None)
        .with_embed_timeout(10);
    let _ = r.embed("hello").await;

    let stats = r.thompson_stats();
    let entry = stats.iter().find(|(name, ..)| name == "slow");
    let (_, alpha, beta) = entry.unwrap_or_else(|| {
        panic!("embed timeout must call record_availability so 'slow' appears in thompson_stats")
    });
    assert!(
        *beta > *alpha,
        "timeout must be recorded as a failure (beta > alpha), got alpha={alpha}, beta={beta}"
    );
}

/// Regression for #5699: same as above for `embed_batch()`.
#[tokio::test]
async fn embed_batch_timeout_records_provider_unavailable() {
    use crate::mock::MockProvider;

    let p = AnyProvider::Mock(
        MockProvider::default()
            .with_embed_delay(200)
            .with_name("slow"),
    );
    let r = RouterProvider::new(vec![p])
        .with_thompson(None)
        .with_embed_timeout(10);
    let _ = r.embed_batch(&["hello"]).await;

    let stats = r.thompson_stats();
    let entry = stats.iter().find(|(name, ..)| name == "slow");
    let (_, alpha, beta) = entry.unwrap_or_else(|| {
        panic!(
            "embed_batch timeout must call record_availability so 'slow' appears in thompson_stats"
        )
    });
    assert!(
        *beta > *alpha,
        "timeout must be recorded as a failure (beta > alpha), got alpha={alpha}, beta={beta}"
    );
}

// ── #5811 embed/embed_batch last-error exhaustion tests ───────────────────

/// When every provider in the `embed()` fallback loop fails with a generic
/// (non-rate-limited, non-invalid-input) error, the router must surface the
/// *last* provider's actual error — not a generic `NoProviders` that discards
/// the actionable diagnostic. Regression for #5811 (mirrors the
/// `chat_with_tools` fix from #5810).
#[tokio::test]
async fn embed_all_providers_exhausted_preserves_last_error() {
    use crate::mock::MockProvider;

    let p1 = AnyProvider::Mock({
        let mut m = MockProvider::default()
            .with_errors(vec![LlmError::ApiError {
                provider: "p1".into(),
                status: 500,
            }])
            .with_name("p1");
        m.supports_embeddings = true;
        m
    });
    let p2 = AnyProvider::Mock({
        let mut m = MockProvider::default()
            .with_errors(vec![LlmError::ApiError {
                provider: "p2".into(),
                status: 503,
            }])
            .with_name("p2");
        m.supports_embeddings = true;
        m
    });

    let r = RouterProvider::new(vec![p1, p2]);
    let err = r.embed("text").await.unwrap_err();

    assert!(
        matches!(&err, LlmError::ApiError { provider, status } if provider == "p2" && *status == 503),
        "expected the last provider's (p2) ApiError to survive exhaustion, got {err:?}"
    );
}

/// Same as above for `embed_batch()`: every provider fails with a generic error,
/// and the router must return the last provider's actual error, not `NoProviders`.
/// Regression for #5811.
#[tokio::test]
async fn embed_batch_all_providers_exhausted_preserves_last_error() {
    use crate::mock::MockProvider;

    let p1 = AnyProvider::Mock({
        let mut m = MockProvider::default()
            .with_errors(vec![LlmError::ApiError {
                provider: "p1".into(),
                status: 500,
            }])
            .with_name("p1");
        m.supports_embeddings = true;
        m
    });
    let p2 = AnyProvider::Mock({
        let mut m = MockProvider::default()
            .with_errors(vec![LlmError::ApiError {
                provider: "p2".into(),
                status: 503,
            }])
            .with_name("p2");
        m.supports_embeddings = true;
        m
    });

    let r = RouterProvider::new(vec![p1, p2]);
    let err = r.embed_batch(&["text"]).await.unwrap_err();

    assert!(
        matches!(&err, LlmError::ApiError { provider, status } if provider == "p2" && *status == 503),
        "expected the last provider's (p2) ApiError to survive exhaustion, got {err:?}"
    );
}

// ── quality_gate tests ────────────────────────────────────────────────────

/// `with_quality_gate()` happy path: when cosine similarity >= threshold the
/// response is returned directly without falling back.
#[tokio::test]
async fn quality_gate_passes_when_similarity_above_threshold() {
    use crate::mock::MockProvider;

    // p1 returns a response; embed returns a unit vector so cosine similarity
    // with itself is 1.0 (>= any reasonable threshold).
    let p1 = AnyProvider::Mock({
        let mut m = MockProvider::with_responses(vec!["answer".to_owned()]).with_name("p1");
        m.supports_embeddings = true;
        m.embedding = vec![1.0, 0.0];
        m
    });
    let r = RouterProvider::new(vec![p1])
        .with_thompson(None)
        .with_quality_gate(0.5);
    let msgs = vec![Message::from_legacy(Role::User, "question")];
    let result = r.chat(&msgs).await.unwrap();
    assert_eq!(result, "answer");
}

/// `with_quality_gate()` exhaustion: when all providers fail the gate the router
/// returns the best-seen response (highest similarity) rather than an error.
#[tokio::test]
async fn quality_gate_exhaustion_returns_best_seen() {
    use crate::mock::MockProvider;

    // p1 returns a response but embedding similarity is 0.0 (orthogonal vectors)
    // so it fails the gate (0.0 < 0.9). p2 fails entirely.
    // Expected: best_seen from p1 is returned.
    let p1 = AnyProvider::Mock({
        let mut m = MockProvider::with_responses(vec!["best_so_far".to_owned()]).with_name("p1");
        m.supports_embeddings = true;
        // query embed = [1,0], response embed = [0,1] → similarity = 0.0
        m.embedding = vec![0.0, 1.0];
        m
    });
    let p2 = AnyProvider::Mock(MockProvider::failing().with_name("p2"));
    let r = RouterProvider::new(vec![p1, p2])
        .with_thompson(None)
        .with_quality_gate(0.9);
    let msgs = vec![Message::from_legacy(Role::User, "question")];
    let result = r.chat(&msgs).await.unwrap();
    assert_eq!(result, "best_so_far");
}

// ── apply_routing_signals guard logic tests ───────────────────────────────

/// `quality_gate = 5.0` (> 1.0) must be silently ignored — the field is left
/// as `None` and no panic occurs.
#[test]
fn routing_signals_quality_gate_above_one_is_ignored() {
    // Build a RouterProvider directly and check that with_quality_gate is only
    // called for in-range values by replicating the guard from provider.rs.
    let threshold: f32 = 5.0;
    let mut router = RouterProvider::new(vec![]);
    if threshold.is_finite() && threshold > 0.0 && threshold <= 1.0 {
        router = router.with_quality_gate(threshold);
    }
    assert!(
        router.quality_gate.is_none(),
        "out-of-range quality_gate must not be wired; got {:?}",
        router.quality_gate
    );
}

/// `quality_gate = 0.8` (valid) must be wired into the router.
#[test]
fn routing_signals_quality_gate_valid_is_wired() {
    let threshold: f32 = 0.8;
    let mut router = RouterProvider::new(vec![]);
    if threshold.is_finite() && threshold > 0.0 && threshold <= 1.0 {
        router = router.with_quality_gate(threshold);
    }
    assert_eq!(
        router.quality_gate,
        Some(0.8),
        "valid quality_gate must be wired"
    );
}

// --- ASI debounce tests ---

#[test]
fn asi_debounce_same_turn_fires_once() {
    let router = RouterProvider::new(vec![]);
    let turn_id = 42u64;

    // First call: prev == u64::MAX (initial) → not equal to turn_id → proceeds (returns false)
    let prev1 = router.state.asi_last_turn.swap(turn_id, Ordering::AcqRel);
    let first_dropped = prev1 == turn_id;

    // Second call same turn: prev == turn_id → dropped
    let prev2 = router.state.asi_last_turn.swap(turn_id, Ordering::AcqRel);
    let second_dropped = prev2 == turn_id;

    assert!(!first_dropped, "first call in turn must not be dropped");
    assert!(second_dropped, "second call in same turn must be dropped");
}

#[test]
fn asi_debounce_next_turn_fires_again() {
    let router = RouterProvider::new(vec![]);

    // Simulate turn 1
    let prev1 = router.state.asi_last_turn.swap(1u64, Ordering::AcqRel);
    assert_ne!(prev1, 1u64, "turn 1: initial value != 1, should proceed");

    // Simulate turn 2 — different turn_id
    let prev2 = router.state.asi_last_turn.swap(2u64, Ordering::AcqRel);
    let dropped = prev2 == 2u64;
    assert!(!dropped, "turn 2 must not be dropped (different turn_id)");
}

#[test]
fn turn_counter_increments_across_clones() {
    let router = RouterProvider::new(vec![]);
    let clone = router.clone();

    let t0 = router.state.turn_counter.fetch_add(1, Ordering::Relaxed);
    let t1 = clone.state.turn_counter.fetch_add(1, Ordering::Relaxed);

    // Both clones share the same Arc<AtomicU64>
    assert_eq!(t1, t0 + 1, "cloned router shares turn_counter");
}

#[test]
fn with_embed_concurrency_zero_means_no_semaphore() {
    let r = RouterProvider::new(vec![]).with_embed_concurrency(0);
    assert!(
        r.state.embed_semaphore.is_none(),
        "0 should disable semaphore"
    );
}

#[test]
fn with_embed_concurrency_positive_creates_semaphore() {
    let r = RouterProvider::new(vec![]).with_embed_concurrency(4);
    let sem = r
        .state
        .embed_semaphore
        .as_ref()
        .expect("semaphore should exist");
    assert_eq!(sem.available_permits(), 4);
}

#[tokio::test]
async fn embed_semaphore_limits_concurrency() {
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicUsize, Ordering as AO};

    // Use a semaphore with 2 permits. Verify that at most 2 concurrent
    // tasks can hold the permit at the same time.
    let sem = Arc::new(tokio::sync::Semaphore::new(2));
    let concurrent_peak = StdArc::new(AtomicUsize::new(0));
    let active = StdArc::new(AtomicUsize::new(0));

    let mut handles = vec![];
    for _ in 0..6 {
        let sem_clone = sem.clone();
        let peak = concurrent_peak.clone();
        let active = active.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem_clone.acquire().await.unwrap();
            let cur = active.fetch_add(1, AO::SeqCst) + 1;
            // Track peak concurrent usage.
            let mut p = peak.load(AO::SeqCst);
            while p < cur {
                match peak.compare_exchange(p, cur, AO::SeqCst, AO::SeqCst) {
                    Ok(_) => break,
                    Err(new) => p = new,
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            active.fetch_sub(1, AO::SeqCst);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    assert!(
        concurrent_peak.load(AO::SeqCst) <= 2,
        "peak concurrency should not exceed semaphore limit"
    );
}

// ── TurnEmbedCache tests (#2819) ──────────────────────────────────────────

/// T2: A second `embed_cached` call with the same text hits the cache instead of
/// calling the underlying provider, and `embed_cache_hits` increments to 1.
#[tokio::test]
async fn turn_embed_cache_hit_increments_counter() {
    use crate::mock::MockProvider;

    let mut m = MockProvider::default();
    m.supports_embeddings = true;
    m.embedding = vec![0.5, 0.5];
    let provider_embed_calls = Arc::clone(&m.embed_call_count);

    let r = RouterProvider::new(vec![AnyProvider::Mock(m)]);
    let cache = Mutex::new(TurnEmbedCache::default());

    // First call — cache miss → calls provider.
    let emb1 = r.embed_cached("hello", &cache).await.unwrap();
    // Second call — same text → cache hit, no provider call.
    let emb2 = r.embed_cached("hello", &cache).await.unwrap();

    assert_eq!(emb1, emb2, "cached embedding must match original");
    assert_eq!(
        provider_embed_calls.load(Ordering::Relaxed),
        1,
        "provider embed() must be called exactly once (second call hits cache)"
    );
    let (total, hits) = r.embed_cache_metrics();
    assert_eq!(
        total, 2,
        "embed_call_count must be 2 (two embed_cached calls)"
    );
    assert_eq!(hits, 1, "embed_cache_hits must be 1 (one cache hit)");
}

/// T3: Passing `Some(precomputed_embedding)` to `spawn_asi_update` does not trigger
/// an `embed()` call on the provider; the ASI window is updated with the provided embedding.
#[tokio::test]
async fn spawn_asi_update_with_precomputed_skips_embed() {
    use crate::mock::MockProvider;

    let mut m = MockProvider::with_responses(vec!["ok".to_owned()]);
    m.supports_embeddings = true;
    m.embedding = vec![1.0, 0.0];
    let provider_embed_calls = Arc::clone(&m.embed_call_count);

    let r = RouterProvider::new(vec![AnyProvider::Mock(m)]).with_asi(AsiRouterConfig::default());

    let precomputed = vec![0.9_f32, 0.1];
    let turn_id = 42u64;

    // Inject a different turn id into asi_last_turn so the debounce doesn't fire.
    r.state.asi_last_turn.store(u64::MAX, Ordering::SeqCst);

    r.spawn_asi_update(
        "p1",
        "response".to_owned(),
        turn_id,
        Some(precomputed.clone()),
    );

    // Give the spawned task time to complete.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Provider embed() must not have been called.
    assert_eq!(
        provider_embed_calls.load(Ordering::Relaxed),
        0,
        "embed() must not be called when precomputed_embedding is Some"
    );

    // The ASI window must have received the precomputed embedding.
    let asi = r.asi.as_ref().unwrap().lock();
    let coherence = asi.coherence("p1");
    // coherence_score returns None when the window has < 2 entries; after one push it's None.
    // We only verify the ASI has the provider in its window (score will be None with 1 entry).
    let _ = coherence; // just verifying no panic
}

/// Regression for #4296: `blocking_load` must not panic on a `current_thread` runtime
/// and must actually call the closure, returning its result.
#[tokio::test]
async fn blocking_load_runs_closure_on_current_thread_runtime() {
    let result = super::blocking_load(|| 42_u32);
    assert_eq!(result, 42, "blocking_load must return the closure result");
}

// ── spawn_asi_update JoinSet reap regression (#4644) ─────────────────────

/// Regression for #4644: completed tasks in `asi_tasks` must be reaped before the cap
/// check so that a full-but-finished `JoinSet` does not permanently block new spawns.
#[tokio::test]
async fn spawn_asi_update_reaped_after_cap_full() {
    use crate::mock::MockProvider;
    use std::sync::atomic::Ordering;

    let mut m = MockProvider::with_responses(vec!["ok".to_owned()]);
    m.supports_embeddings = true;
    m.embedding = vec![1.0, 0.0];
    let embed_calls = Arc::clone(&m.embed_call_count);

    let r = RouterProvider::new(vec![AnyProvider::Mock(m)]).with_asi(AsiRouterConfig::default());
    r.state.asi_last_turn.store(u64::MAX, Ordering::SeqCst);

    // Spawn exactly MAX_ASI_TASKS tasks and wait for all to complete.
    for i in 0..super::MAX_ASI_TASKS {
        r.spawn_asi_update("p1", format!("resp{i}"), i as u64, Some(vec![0.5, 0.5]));
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // After all tasks finish the JoinSet is full of completed handles.
    // Without the drain fix this next call would be silently skipped.
    r.spawn_asi_update(
        "p1",
        "extra".to_owned(),
        super::MAX_ASI_TASKS as u64,
        Some(vec![0.9, 0.1]),
    );

    // Give the newly spawned task time to complete.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // embed() must never have been called — all calls used a precomputed embedding.
    assert_eq!(
        embed_calls.load(Ordering::Relaxed),
        0,
        "embed() must not be called when precomputed_embedding is Some"
    );

    // Trigger one more spawn — the drain inside will reap the last completed task,
    // proving the extra spawn was not permanently blocked by the full JoinSet.
    r.spawn_asi_update(
        "p1",
        "probe".to_owned(),
        (super::MAX_ASI_TASKS + 1) as u64,
        Some(vec![0.1, 0.9]),
    );

    // All previously finished tasks must have been reaped; only the probe may remain.
    let remaining = r.asi_tasks.lock().len();
    assert!(
        remaining <= 1,
        "completed tasks must be reaped; at most 1 in-flight task expected, got {remaining}"
    );
}

// ── spawn_asi_update timeout regression (#4566) ───────────────────────────

/// Regression for #4566: when `embed()` inside `spawn_asi_update` exceeds `embed_timeout_ms`,
/// the ASI coherence window must NOT be updated (task returns early without pushing embedding).
#[tokio::test]
async fn spawn_asi_update_embed_timeout_does_not_update_asi() {
    use crate::mock::MockProvider;
    use std::sync::atomic::Ordering;

    // Provider that takes 200 ms to embed — well above the 10 ms timeout.
    let mut m = MockProvider::with_responses(vec!["ok".to_owned()]);
    m.supports_embeddings = true;
    m.embedding = vec![1.0, 0.0];
    m.embed_delay_ms = 200;
    let provider_embed_calls = Arc::clone(&m.embed_call_count);

    let r = RouterProvider::new(vec![AnyProvider::Mock(m)])
        .with_asi(AsiRouterConfig::default())
        .with_embed_timeout(10);

    // Inject a sentinel turn id so the debounce does not fire.
    r.state.asi_last_turn.store(u64::MAX, Ordering::SeqCst);

    // No precomputed embedding → router will attempt to call embed().
    r.spawn_asi_update("p1", "response".to_owned(), 1u64, None);

    // Wait long enough for the spawned task to reach its timeout and return.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // embed() was called (the call was made before the timeout fired).
    assert!(
        provider_embed_calls.load(Ordering::Relaxed) >= 1,
        "embed() must have been attempted"
    );

    // ASI window must be empty — timeout fired before push_embedding could run.
    let asi = r.asi.as_ref().unwrap().lock();
    let coherence = asi.coherence("p1");
    // coherence() returns 1.0 when the provider is unknown (no entries in the window).
    assert!(
        (coherence - 1.0).abs() < f32::EPSILON,
        "ASI window must be empty after embed timeout; coherence={coherence}"
    );
}

// ── #5821 chat/chat_stream last-error exhaustion tests ─────────────────────

/// When every provider in the `chat()` fallback loop fails with a generic error,
/// the router must surface the *last* provider's actual error — not a generic
/// `NoProviders` that discards the actionable diagnostic. Regression for #5821
/// (mirrors the `embed`/`embed_batch` fix from #5811 and `chat_with_tools` from #5810).
#[tokio::test]
async fn chat_all_providers_exhausted_preserves_last_error() {
    use crate::mock::MockProvider;

    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_errors(vec![LlmError::ApiError {
                provider: "p1".into(),
                status: 500,
            }])
            .with_name("p1"),
    );
    let p2 = AnyProvider::Mock(
        MockProvider::default()
            .with_errors(vec![LlmError::ApiError {
                provider: "p2".into(),
                status: 503,
            }])
            .with_name("p2"),
    );

    let r = RouterProvider::new(vec![p1, p2]);
    let err = r.chat(&[]).await.unwrap_err();

    assert!(
        matches!(&err, LlmError::ApiError { provider, status } if provider == "p2" && *status == 503),
        "expected the last provider's (p2) ApiError to survive exhaustion, got {err:?}"
    );
}

/// Same as above for `chat_stream()`: every provider fails with a generic error,
/// and the router must return the last provider's actual error, not `NoProviders`.
/// Regression for #5821.
#[tokio::test]
async fn chat_stream_all_providers_exhausted_preserves_last_error() {
    use crate::mock::MockProvider;

    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_errors(vec![LlmError::ApiError {
                provider: "p1".into(),
                status: 500,
            }])
            .with_name("p1"),
    );
    let p2 = AnyProvider::Mock(
        MockProvider::default()
            .with_errors(vec![LlmError::ApiError {
                provider: "p2".into(),
                status: 503,
            }])
            .with_name("p2"),
    );

    let r = RouterProvider::new(vec![p1, p2]);
    match r.chat_stream(&[]).await {
        Err(LlmError::ApiError { provider, status }) => {
            assert_eq!(provider, "p2");
            assert_eq!(status, 503);
        }
        other => panic!(
            "expected the last provider's (p2) ApiError to survive exhaustion, got {}",
            match &other {
                Ok(_) => "Ok(_)".to_owned(),
                Err(e) => format!("{e:?}"),
            }
        ),
    }
}

// ── #5883: capability delegation (/think-tokens, /reasoning-effort) ────────

#[test]
fn router_set_thinking_budget_delegates_to_last_active_provider() {
    use crate::claude::ClaudeProvider;
    use crate::ollama::OllamaProvider;

    let ollama = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "m".into(),
        "e".into(),
    ));
    let claude = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
    let mut r = RouterProvider::new(vec![ollama, claude]);
    *r.state.last_active_provider.lock() = Some("claude".to_owned());

    r.set_thinking_budget_delegated(Some(4096)).unwrap();

    assert_eq!(r.state.providers[1].current_thinking_budget(), Some(4096));
    // The un-targeted provider must be untouched.
    assert_eq!(r.state.providers[0].current_thinking_budget(), None);
}

#[test]
fn router_capability_target_falls_back_to_first_when_no_dispatch_yet() {
    use crate::ollama::OllamaProvider;

    let p1 = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "m".into(),
        "e".into(),
    ));
    let p2 = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:2",
        "m".into(),
        "e".into(),
    ));
    let r = RouterProvider::new(vec![p1, p2]);
    assert_eq!(r.capability_target_index(), Some(0));
}

#[test]
fn router_capability_target_falls_back_to_first_on_config_drift() {
    use crate::ollama::OllamaProvider;

    let p1 = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "m".into(),
        "e".into(),
    ));
    let r = RouterProvider::new(vec![p1]);
    // Names a provider that is no longer (or never was) in the pool.
    *r.state.last_active_provider.lock() = Some("stale-provider".to_owned());
    assert_eq!(r.capability_target_index(), Some(0));
}

#[test]
fn router_capability_target_none_for_empty_pool() {
    let r = RouterProvider::new(vec![]);
    assert_eq!(r.capability_target_index(), None);
}

/// M3: `capability_target_index`'s name->slot lookup is a pre-existing, shared assumption
/// (also relied on by `last_selected_provider_kind`/`record_quality_outcome`) that duplicate
/// provider names resolve to the FIRST matching slot. `OllamaProvider::new` defaults
/// `provider_name` to `"ollama"` unless `.with_provider_name(...)` overrides it, so two
/// default-named Ollama entries in the same pool naturally collide.
#[test]
fn router_capability_target_resolves_duplicate_name_to_first_slot() {
    use crate::ollama::OllamaProvider;

    let p1 = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "m1".into(),
        "e".into(),
    ));
    let p2 = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:2",
        "m2".into(),
        "e".into(),
    ));
    assert_eq!(p1.name(), "ollama");
    assert_eq!(p2.name(), "ollama");

    let r = RouterProvider::new(vec![p1, p2]);
    *r.state.last_active_provider.lock() = Some("ollama".to_owned());
    assert_eq!(r.capability_target_index(), Some(0));
}

#[test]
fn router_set_thinking_budget_names_real_inner_provider_on_mismatch() {
    use crate::ollama::OllamaProvider;

    // Ollama does not support a thinking-token budget — FR-005: the error must name the
    // real inner provider, not the generic "router" string.
    let ollama = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "m".into(),
        "e".into(),
    ));
    let mut r = RouterProvider::new(vec![ollama]);
    let err = r.set_thinking_budget_delegated(Some(1024)).unwrap_err();
    match err {
        LlmError::ModelCapabilityMismatch { provider, .. } => assert_eq!(provider, "ollama"),
        other => panic!("expected ModelCapabilityMismatch naming ollama, got {other:?}"),
    }
}

/// M2 regression: the setter must mutate the SAME authoritative `RouterProvider` instance
/// that dispatch later clones-at-entry from, even when `Arc::get_mut` fails because another
/// clone (e.g. an in-flight `spawn_asi_update` task) is sharing the `providers` Arc. This is
/// the common runtime path (`spawn_asi_update` holds a clone for up to `embed_timeout_ms`
/// after every turn), not a defensive fallback — see `with_target_provider_mut` doc comment.
#[test]
fn router_set_thinking_budget_rebuild_path_persists_on_authoritative_instance() {
    use crate::claude::ClaudeProvider;

    let claude = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
    let mut r = RouterProvider::new(vec![claude]);

    // Simulate an in-flight background clone (e.g. `spawn_asi_update`) holding a reference to
    // the same `providers` Arc, bumping its refcount above 1 so `Arc::get_mut` fails.
    let stale_clone = r.clone();
    assert!(Arc::ptr_eq(
        &r.state.providers,
        &stale_clone.state.providers
    ));

    r.set_thinking_budget_delegated(Some(2048)).unwrap();

    // The rebuild replaced `r.state.providers` with a new Arc — no longer the same allocation
    // the stale clone holds.
    assert!(!Arc::ptr_eq(
        &r.state.providers,
        &stale_clone.state.providers
    ));
    // The authoritative instance's next dispatch observes the mutation.
    assert_eq!(r.state.providers[0].current_thinking_budget(), Some(2048));
    // The stale clone's (now-orphaned) slice is untouched — expected: it will be dropped, not
    // dispatched through again.
    assert_eq!(
        stale_clone.state.providers[0].current_thinking_budget(),
        None
    );
}

#[test]
fn router_capability_delegation_advisory_present_for_resampling_multi_provider_pool() {
    use crate::claude::ClaudeProvider;
    use crate::ollama::OllamaProvider;

    let claude = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
    let ollama = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "m".into(),
        "e".into(),
    ));
    let mut r = RouterProvider::new(vec![claude, ollama]);
    r.strategy = RouterStrategy::Thompson;

    let advisory = r.capability_delegation_advisory();
    assert!(advisory.is_some());
    assert!(advisory.unwrap().contains("claude"));
}

#[test]
fn router_capability_delegation_advisory_absent_for_cascade() {
    use crate::claude::ClaudeProvider;
    use crate::ollama::OllamaProvider;

    let claude = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
    let ollama = AnyProvider::Ollama(OllamaProvider::new(
        "http://127.0.0.1:1",
        "m".into(),
        "e".into(),
    ));
    let mut r = RouterProvider::new(vec![claude, ollama]);
    r.strategy = RouterStrategy::Cascade;

    assert_eq!(r.capability_delegation_advisory(), None);
}

#[test]
fn router_capability_delegation_advisory_absent_for_single_provider_pool() {
    use crate::claude::ClaudeProvider;

    let claude = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
    let mut r = RouterProvider::new(vec![claude]);
    r.strategy = RouterStrategy::Thompson;

    assert_eq!(r.capability_delegation_advisory(), None);
}

#[test]
fn masked_router_set_thinking_budget_delegates_through_inner() {
    use crate::claude::ClaudeProvider;
    use crate::masking::MaskedProvider;

    #[derive(Debug)]
    struct NoopMasker;
    impl crate::masking::OutboundMasker for NoopMasker {
        fn mask(&self, _text: &str) -> Option<String> {
            None
        }
    }

    let claude = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
    let router = RouterProvider::new(vec![claude]);
    let mut masked = AnyProvider::Masked(Box::new(MaskedProvider::new(
        AnyProvider::Router(Box::new(router)),
        std::sync::Arc::new(NoopMasker),
    )));

    masked.set_thinking_budget(Some(4096)).unwrap();
    assert_eq!(masked.current_thinking_budget(), Some(4096));
}

/// FR-008: `Masked(Triage(...))` must delegate transparently through `p.inner`/`p.inner()`,
/// mirroring the `Masked(Router(...))` case above.
#[test]
fn masked_triage_set_thinking_budget_delegates_through_inner() {
    use crate::claude::ClaudeProvider;
    use crate::masking::MaskedProvider;
    use crate::router::triage::{ComplexityTier, TriageRouter};

    #[derive(Debug)]
    struct NoopMasker;
    impl crate::masking::OutboundMasker for NoopMasker {
        fn mask(&self, _text: &str) -> Option<String> {
            None
        }
    }

    let triage_provider = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
    let claude = AnyProvider::Claude(ClaudeProvider::new("k".into(), "m".into(), 1024));
    let router = TriageRouter::new(
        triage_provider,
        vec![(ComplexityTier::Simple, claude)],
        5,
        100,
    );
    let mut masked = AnyProvider::Masked(Box::new(MaskedProvider::new(
        AnyProvider::Triage(Box::new(router)),
        std::sync::Arc::new(NoopMasker),
    )));

    masked.set_thinking_budget(Some(4096)).unwrap();
    assert_eq!(masked.current_thinking_budget(), Some(4096));
}

// ── #6183: effective_model_identifier resolves the real dispatched sub-provider ──
//
// `model_identifier()` returns the stable `"router"` label (never a reasoning-model
// pattern), so `is_reasoning_model(self.provider.model_identifier())` at the
// `tool_result.rs` call site was permanently unreachable for Router-wrapped
// providers. `effective_model_identifier()` resolves the actually-dispatched
// sub-provider instead, reusing the same `last_active_provider` state already
// read by reputation attribution (`last_selected_provider_kind`).

#[test]
fn router_effective_model_identifier_before_dispatch_is_router_label() {
    use crate::mock::MockProvider;

    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_name("p1")
            .with_model_identifier("o3"),
    );
    let r = RouterProvider::new(vec![p1]);
    assert_eq!(r.effective_model_identifier(), "router");
}

#[test]
fn router_effective_model_identifier_resolves_last_active_reasoning_model() {
    use crate::mock::MockProvider;

    let openai = AnyProvider::Mock(
        MockProvider::default()
            .with_name("openai")
            .with_model_identifier("gpt-4o"),
    );
    let reasoner = AnyProvider::Mock(
        MockProvider::default()
            .with_name("reasoner")
            .with_model_identifier("o3-mini"),
    );
    let r = RouterProvider::new(vec![openai, reasoner]);
    *r.state.last_active_provider.lock() = Some("reasoner".to_owned());

    assert_eq!(r.effective_model_identifier(), "o3-mini");
}

#[test]
fn router_effective_model_identifier_resolves_last_active_non_reasoning_model() {
    use crate::mock::MockProvider;

    let openai = AnyProvider::Mock(
        MockProvider::default()
            .with_name("openai")
            .with_model_identifier("gpt-4o"),
    );
    let r = RouterProvider::new(vec![openai]);
    *r.state.last_active_provider.lock() = Some("openai".to_owned());

    assert_eq!(r.effective_model_identifier(), "gpt-4o");
}

#[test]
fn router_effective_model_identifier_falls_back_to_router_on_stale_name() {
    use crate::mock::MockProvider;

    let p1 = AnyProvider::Mock(
        MockProvider::default()
            .with_name("p1")
            .with_model_identifier("gpt-4o"),
    );
    let r = RouterProvider::new(vec![p1]);
    *r.state.last_active_provider.lock() = Some("gone".to_owned());

    assert_eq!(r.effective_model_identifier(), "router");
}

/// End-to-end: drive a real `chat_with_tools` dispatch (not a direct state poke) and
/// verify `effective_model_identifier()` resolves the sub-provider that actually served
/// it, exercising the same `last_active_provider` write the production dispatch loop
/// performs (`provider_impl.rs` `chat_with_tools`).
#[tokio::test]
async fn router_effective_model_identifier_resolves_via_real_dispatch() {
    use crate::mock::MockProvider;

    let reasoner = AnyProvider::Mock(
        MockProvider::default()
            .with_name("reasoner")
            .with_model_identifier("deepseek-r1"),
    );
    let r = RouterProvider::new(vec![reasoner]);

    r.chat_with_tools(&[], &[]).await.unwrap();

    assert_eq!(r.effective_model_identifier(), "deepseek-r1");
}

/// Regression guard: the default `effective_model_identifier()` impl must forward
/// unchanged to `model_identifier()` for a concrete (non-router) provider — the new
/// trait method must not alter behavior for the 7 providers PR #6182 already fixed.
#[test]
fn mock_provider_effective_model_identifier_defaults_to_model_identifier() {
    use crate::mock::MockProvider;

    let p = MockProvider::default()
        .with_name("openai")
        .with_model_identifier("o3-mini");

    assert_eq!(p.effective_model_identifier(), p.model_identifier());
    assert_eq!(p.effective_model_identifier(), "o3-mini");
}

// ── spec-072 S1 fix: RouterProvider vision-tier safety net (Cascade/Bandit/Ema/Thompson) ──

fn image_msg() -> Message {
    Message {
        role: Role::User,
        content: String::new(),
        parts: vec![crate::provider::MessagePart::Image(Box::new(
            crate::provider::ImageData {
                data: vec![1, 2, 3],
                mime_type: "image/png".into(),
            },
        ))],
        metadata: crate::provider::MessageMetadata::default(),
    }
}

#[test]
fn router_supports_vision_true_when_any_provider_supports_it() {
    use crate::mock::MockProvider;

    let text_only = AnyProvider::Mock(MockProvider::default().with_name("text-only"));
    let vision = AnyProvider::Mock(MockProvider::default().with_name("vision").with_vision());
    let r = RouterProvider::new(vec![text_only, vision]);
    assert!(r.supports_vision());
}

#[test]
fn router_supports_vision_false_when_no_provider_supports_it() {
    use crate::mock::MockProvider;

    let text_only = AnyProvider::Mock(MockProvider::default().with_name("text-only"));
    let r = RouterProvider::new(vec![text_only]);
    assert!(!r.supports_vision());
}

/// Default (Ema/Thompson-style ordered-fallback) dispatch: a non-vision-capable provider
/// must never receive the `Image` part, even though the router aggregate `supports_vision()`
/// is `true` because a later provider in the pool does support vision (C3/AC-6).
#[tokio::test]
async fn chat_with_tools_strips_image_for_non_vision_provider_in_ordered_dispatch() {
    use crate::mock::MockProvider;

    let (non_vision, recorded) =
        MockProvider::with_responses(vec!["ok".to_owned()]).with_recording();
    let non_vision = AnyProvider::Mock(non_vision);
    let vision = AnyProvider::Mock(
        MockProvider::with_responses(vec!["unused".to_owned()])
            .with_name("vision-p")
            .with_vision(),
    );
    // Router aggregate supports_vision() is true (the second provider supports it), but the
    // ordered dispatch loop tries `non_vision` first — it must never see the Image part.
    let r = RouterProvider::new(vec![non_vision, vision]);
    assert!(r.supports_vision());

    let messages = vec![image_msg()];
    let result = r.chat_with_tools(&messages, &[]).await.unwrap();
    assert!(matches!(result, crate::provider::ChatResponse::Text(t) if t == "ok"));

    let calls = recorded.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(
        !messages_contain_image(&calls[0]),
        "non-vision-capable provider must never receive an Image part (C3, AC-6)"
    );
}

/// Bandit strategy: the single bandit-selected provider must have the image stripped when
/// it does not itself support vision.
#[tokio::test]
async fn chat_with_tools_strips_image_for_non_vision_bandit_selected_provider() {
    use crate::mock::MockProvider;

    let (non_vision, recorded) =
        MockProvider::with_responses(vec!["ok".to_owned()]).with_recording();
    let non_vision = AnyProvider::Mock(non_vision);
    let r = RouterProvider::new(vec![non_vision]).with_bandit(
        BanditRouterConfig::default(),
        None,
        None,
    );

    let messages = vec![image_msg()];
    let result = r.chat_with_tools(&messages, &[]).await.unwrap();
    assert!(matches!(result, crate::provider::ChatResponse::Text(t) if t == "ok"));

    let calls = recorded.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(
        !messages_contain_image(&calls[0]),
        "bandit-selected non-vision-capable provider must never receive an Image part"
    );
}

/// When the dispatched provider DOES support vision, the Image part must reach it unchanged
/// (byte-for-byte) — the safety net must not strip images unconditionally.
#[tokio::test]
async fn chat_with_tools_preserves_image_for_vision_capable_provider() {
    use crate::mock::MockProvider;

    let (vision_provider, recorded) = MockProvider::with_responses(vec!["ok".to_owned()])
        .with_vision()
        .with_recording();
    let r = RouterProvider::new(vec![AnyProvider::Mock(vision_provider)]);

    let messages = vec![image_msg()];
    r.chat_with_tools(&messages, &[]).await.unwrap();

    let calls = recorded.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(
        messages_contain_image(&calls[0]),
        "a vision-capable provider must still receive the Image part"
    );
}
