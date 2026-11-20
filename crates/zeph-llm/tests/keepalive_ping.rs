// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Fixture construction failures should fail the test immediately.; Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests (tests-first) for `LlmProvider::keepalive_ping` (spec 101, plan r2 Track B
//! with r4): trait default, forwarding through `AnyProvider`, and the Claude minimum-cacheable
//! guard, which must answer before any network traffic.

use std::sync::Arc;

use zeph_llm::any::AnyProvider;
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{
    KeepaliveOutcome, KeepaliveRequest, KeepaliveSkip, LlmProvider, Message, Role, ToolDefinition,
};

fn request(provider: AnyProvider, user_text: &str) -> KeepaliveRequest {
    let messages: Arc<[Message]> = Arc::from(vec![Message {
        role: Role::User,
        content: user_text.to_owned(),
        ..Message::default()
    }]);
    let tools: Arc<[ToolDefinition]> = Arc::from(Vec::<ToolDefinition>::new());
    KeepaliveRequest {
        provider,
        messages,
        tools,
        target: None,
    }
}

#[tokio::test]
async fn keepalive_ping_trait_default_is_unsupported() {
    let mock = AnyProvider::Mock(MockProvider::default());
    let req = request(mock.clone(), "hello");
    let out = mock
        .keepalive_ping(&req)
        .await
        .expect("default never errors");
    assert!(matches!(out, KeepaliveOutcome::Unsupported));
}

#[tokio::test]
async fn keepalive_ping_any_provider_forwards_to_the_inner_provider() {
    let (mock, recorded) = MockProvider::default().with_keepalive(vec![]);
    let any = AnyProvider::Mock(mock);
    let req = request(any.clone(), "hello");
    let out = any.keepalive_ping(&req).await.expect("scripted ping");
    assert!(matches!(out, KeepaliveOutcome::Pinged(_)));
    assert_eq!(
        recorded.lock().unwrap().len(),
        1,
        "inner provider saw the ping"
    );
}

#[tokio::test]
async fn keepalive_ping_claude_below_min_cacheable_skips_without_network() {
    let claude = AnyProvider::Claude(ClaudeProvider::new(
        "sk-ant-test-not-a-real-key".to_owned(),
        "claude-sonnet-4-5".to_owned(),
        1024,
    ));
    let req = request(claude.clone(), "tiny prefix");
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        claude.keepalive_ping(&req),
    )
    .await
    .expect("must decide locally, not wait on the network")
    .expect("skip is not an error");
    assert!(matches!(
        out,
        KeepaliveOutcome::Skipped(KeepaliveSkip::BelowMinCacheable)
    ));
}
