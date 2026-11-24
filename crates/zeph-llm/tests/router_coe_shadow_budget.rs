// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(feature = "testing")]
#![expect(
    clippy::unwrap_used,
    reason = "Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests (tests-first) for #7143 (S1/N4): the `CoE` shadow stage is bounded by
//! `CoeConfig::shadow_budget = min(30 s, llm_timeout / 4)`; a stalled or zero-budget shadow keeps
//! the primary. A zero budget must short-circuit before the secondary is polled.

use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, LlmProvider, Message, Role, ToolDefinition};
use zeph_llm::router::RouterProvider;
use zeph_llm::router::coe::{CoeConfig, shadow_budget_for};

fn cfg(budget: Duration) -> CoeConfig {
    CoeConfig {
        intra_threshold: 0.8,
        inter_threshold: 0.2,
        shadow_sample_rate: 0.0,
        shadow_budget: budget,
        request_timeout: Duration::from_secs(600),
    }
}

fn embed() -> AnyProvider {
    let mut m = MockProvider::default();
    m.supports_embeddings = true;
    AnyProvider::Mock(m)
}

fn user() -> Vec<Message> {
    vec![Message::from_legacy(Role::User, "q")]
}

fn no_tools() -> Vec<ToolDefinition> {
    Vec::new()
}

#[test]
fn shadow_budget_for_is_min_of_30s_and_quarter_of_llm_timeout() {
    assert_eq!(
        shadow_budget_for(Duration::from_secs(120)),
        Duration::from_secs(30)
    );
    assert_eq!(
        shadow_budget_for(Duration::from_secs(20)),
        Duration::from_secs(5)
    );
    assert_eq!(shadow_budget_for(Duration::ZERO), Duration::ZERO);
}

#[test]
fn coe_config_default_shadow_budget_is_30s() {
    assert_eq!(CoeConfig::default().shadow_budget, Duration::from_secs(30));
}

#[tokio::test(start_paused = true)]
async fn chat_with_tools_stalled_secondary_keeps_primary_within_budget() {
    let (primary, _) = MockProvider::default().with_tool_use(vec![ChatResponse::text("primary")]);
    let stalled = MockProvider::default().with_delay(600_000);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary.with_entropy(1.0))]).with_coe(
        cfg(Duration::from_secs(5)),
        AnyProvider::Mock(stalled),
        embed(),
    );

    let resp = router.chat_with_tools(&user(), &no_tools()).await.unwrap();

    assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "primary"));
    assert_eq!(
        router.coe_metrics().unwrap().shadow_timeouts,
        1,
        "shadow_timeouts counts the timeout"
    );
}

#[tokio::test(start_paused = true)]
async fn chat_stalled_secondary_keeps_primary_within_budget() {
    let primary = MockProvider::with_responses(vec!["primary".into()]).with_entropy(1.0);
    let stalled = MockProvider::with_responses(vec!["secondary".into()]).with_delay(600_000);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary)]).with_coe(
        cfg(Duration::from_secs(5)),
        AnyProvider::Mock(stalled),
        embed(),
    );

    assert_eq!(router.chat(&user()).await.unwrap(), "primary");
    assert_eq!(router.coe_metrics().unwrap().shadow_timeouts, 1);
}

#[tokio::test]
async fn chat_with_tools_zero_budget_keeps_primary_without_polling_secondary() {
    let (primary, _) = MockProvider::default().with_tool_use(vec![ChatResponse::text("primary")]);
    let (secondary, secondary_calls) =
        MockProvider::default().with_tool_use(vec![ChatResponse::text("secondary")]);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary.with_entropy(1.0))]).with_coe(
        cfg(Duration::ZERO),
        AnyProvider::Mock(secondary),
        embed(),
    );

    let resp = router.chat_with_tools(&user(), &no_tools()).await.unwrap();

    assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "primary"));
    assert_eq!(*secondary_calls.lock().unwrap(), 0);
}
