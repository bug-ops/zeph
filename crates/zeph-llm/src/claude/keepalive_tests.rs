// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `keepalive_ping` over the wire: a minimal, side-effect-free replay of the main request.

use std::sync::Arc;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::ClaudeProvider;
use crate::any::AnyProvider;
use crate::provider::{
    KeepaliveOutcome, KeepaliveRequest, KeepaliveSkip, LlmProvider as _, Message, Role,
    ToolDefinition,
};

fn provider(server: &MockServer) -> ClaudeProvider {
    ClaudeProvider::new("test-key".into(), "claude-sonnet-4-5".into(), 4096)
        .with_api_url(format!("{}/v1/messages", server.uri()))
}

fn request(provider: &ClaudeProvider, user_text: &str) -> KeepaliveRequest {
    KeepaliveRequest {
        provider: AnyProvider::Claude(provider.clone()),
        messages: Arc::from(vec![Message::from_legacy(Role::User, user_text)]),
        tools: Arc::from(Vec::<ToolDefinition>::new()),
        target: None,
    }
}

fn big_prefix() -> String {
    "cacheable prefix ".repeat(2_000)
}

fn usage_response(cache_read: u64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "max_tokens",
        "usage": {
            "input_tokens": 12,
            "output_tokens": 1,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": cache_read
        }
    }))
}

#[tokio::test]
async fn ping_sends_minimal_non_streaming_replay_and_leaves_provider_untouched() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(usage_response(9000))
        .expect(1)
        .mount(&server)
        .await;
    let provider = provider(&server);
    let req = request(&provider, &big_prefix());

    let outcome = provider.keepalive_ping(&req).await.expect("ping succeeds");
    let KeepaliveOutcome::Pinged(usage) = outcome else {
        panic!("expected Pinged, got {outcome:?}");
    };
    assert_eq!(usage.cache.map(|c| c.read), Some(9000));
    assert_eq!(usage.cache.map(|c| c.creation), Some(0));

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(
        body["max_tokens"], 1,
        "no thinking: one output token suffices"
    );
    assert!(
        body.get("stream").is_none_or(|v| v == false),
        "a ping never streams"
    );
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
    assert!(
        provider.last_response_usage().is_none(),
        "a ping must not publish usage on the provider"
    );
}

#[tokio::test]
async fn ping_with_extended_thinking_uses_budget_plus_one_as_max_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(usage_response(5000))
        .mount(&server)
        .await;
    let mut provider = provider(&server);
    provider
        .set_thinking(Some(zeph_config::ThinkingConfig::Extended {
            budget_tokens: 2048,
        }))
        .unwrap();
    let req = request(&provider, &big_prefix());

    let outcome = provider.keepalive_ping(&req).await.unwrap();
    assert!(matches!(outcome, KeepaliveOutcome::Pinged(_)));
    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["max_tokens"], 2049);
    assert_eq!(body["thinking"]["budget_tokens"], 2048);
}

#[tokio::test]
async fn ping_below_min_cacheable_sends_nothing() {
    let server = MockServer::start().await;
    let provider = provider(&server);
    let req = request(&provider, "tiny");

    let outcome = provider.keepalive_ping(&req).await.unwrap();
    assert!(matches!(
        outcome,
        KeepaliveOutcome::Skipped(KeepaliveSkip::BelowMinCacheable)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn ping_http_error_is_returned_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = provider(&server);
    let req = request(&provider, &big_prefix());

    assert!(provider.keepalive_ping(&req).await.is_err());
}
