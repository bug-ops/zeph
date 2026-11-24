// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(feature = "testing")]
#![expect(
    clippy::unwrap_used,
    reason = "Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests for #7187: the `CoE` shadow stage never outlasts the outer request deadline
//! (`CoeConfig::request_timeout`) minus a 1 s margin, so a late primary answer is kept.

use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{LlmProvider, Message, Role};
use zeph_llm::router::RouterProvider;
use zeph_llm::router::coe::CoeConfig;

const OUTER: Duration = Duration::from_secs(16);

fn cfg() -> CoeConfig {
    CoeConfig {
        intra_threshold: 0.8,
        inter_threshold: 0.2,
        shadow_sample_rate: 0.0,
        shadow_budget: Duration::from_secs(4),
        request_timeout: OUTER,
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

fn router(primary_delay_ms: u64, secondary_delay_ms: u64) -> RouterProvider {
    let primary = MockProvider::with_responses(vec!["primary".into()])
        .with_entropy(1.0)
        .with_delay(primary_delay_ms);
    let secondary =
        MockProvider::with_responses(vec!["secondary".into()]).with_delay(secondary_delay_ms);
    RouterProvider::new(vec![AnyProvider::Mock(primary)]).with_coe(
        cfg(),
        AnyProvider::Mock(secondary),
        embed(),
    )
}

#[tokio::test(start_paused = true)]
async fn chat_late_primary_is_kept_when_shadow_budget_exceeds_remaining_deadline() {
    let r = router(13_000, 600_000);
    let out = tokio::time::timeout(OUTER, r.chat(&user()))
        .await
        .expect("outer deadline must not fire after the primary answered");
    assert_eq!(out.unwrap(), "primary");
}

#[tokio::test(start_paused = true)]
async fn chat_shadow_is_skipped_when_less_than_one_second_remains() {
    let r = router(15_500, 100);
    let out = tokio::time::timeout(OUTER, r.chat(&user()))
        .await
        .expect("outer deadline must not fire");
    assert_eq!(out.unwrap(), "primary");
}
