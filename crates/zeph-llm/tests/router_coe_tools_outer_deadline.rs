// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(feature = "testing")]
#![expect(
    clippy::unwrap_used,
    reason = "Fixture construction failures should fail the test immediately."
)]

//! #7187 on the `chat_with_tools` path: the `CoE` shadow stage never outlasts the outer request
//! deadline (`CoeConfig::request_timeout`) minus a 1 s margin, so a late primary answer is kept.
//!
//! The mock's queued tool responses return without its delay, so these fixtures use plain
//! `with_responses`: `chat_with_tools` then falls back to the delayed `chat()` path.

use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, LlmProvider, Message, Role, ToolDefinition};
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

async fn run(r: &RouterProvider) -> ChatResponse {
    let msgs = vec![Message::from_legacy(Role::User, "q")];
    let tools: Vec<ToolDefinition> = Vec::new();
    tokio::time::timeout(OUTER, r.chat_with_tools(&msgs, &tools))
        .await
        .expect("outer deadline must not fire after the primary answered")
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn tools_late_primary_is_kept_when_shadow_budget_exceeds_remaining_deadline() {
    let resp = run(&router(13_000, 600_000)).await;
    assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "primary"));
}

#[tokio::test(start_paused = true)]
async fn tools_shadow_is_skipped_when_less_than_one_second_remains() {
    let resp = run(&router(15_500, 100)).await;
    assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "primary"));
}
