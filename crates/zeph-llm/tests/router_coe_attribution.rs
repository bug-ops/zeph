// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(feature = "testing")]
#![expect(
    clippy::unwrap_used,
    reason = "Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests for #7163 (explicit quality target), #7164 (tool-path escalation
//! attribution), #7165 (no shadow sampling without logprobs on the tool path) and #7166
//! (separate secondary-failure and shadow-timeout counters).
//!
//! The pre-existing `router_coe_tools.rs` test
//! `chat_with_tools_escalation_leaves_reputation_tracker_as_without_coe` asserts the opposite of
//! #7164 and, like the tuple reads of `coe_metrics()` in `router_coe_*.rs`, must be updated by the
//! developer of track c.

use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{ChatResponse, LlmProvider, Message, ResponseUsage, Role, ToolDefinition};
use zeph_llm::router::coe::{CoeConfig, CoePath, should_shadow};
use zeph_llm::router::{QualityTarget, RouterProvider};

fn cfg(sample_rate: f64) -> CoeConfig {
    CoeConfig {
        intra_threshold: 0.8,
        inter_threshold: 0.2,
        shadow_sample_rate: sample_rate,
        shadow_budget: Duration::from_secs(5),
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

fn tools() -> Vec<ToolDefinition> {
    Vec::new()
}

fn usage(input: u64, output: u64) -> ResponseUsage {
    ResponseUsage {
        input,
        output,
        ..ResponseUsage::default()
    }
}

fn observations(router: &RouterProvider, name: &str) -> u64 {
    router
        .reputation_stats()
        .iter()
        .find(|s| s.0 == name)
        .map_or(0, |s| s.4)
}

fn two_provider_router(dir: &tempfile::TempDir) -> RouterProvider {
    let (a, _) = MockProvider::default()
        .with_name("p1")
        .with_tool_use(vec![ChatResponse::text("a")]);
    let (b, _) = MockProvider::default()
        .with_name("p2")
        .with_tool_use(vec![ChatResponse::text("b")]);
    RouterProvider::new(vec![AnyProvider::Mock(a), AnyProvider::Mock(b)]).with_reputation(
        1.0,
        0.5,
        1,
        Some(&dir.path().join("rep.json")),
    )
}

// ---- #7163 -------------------------------------------------------------------------------

#[tokio::test]
async fn test_record_quality_outcome_named_target_mutates_only_that_provider() {
    let dir = tempfile::tempdir().unwrap();
    let router = two_provider_router(&dir);
    router.chat_with_tools(&user(), &tools()).await.unwrap();
    let (p1_before, p2_before) = (observations(&router, "p1"), observations(&router, "p2"));

    router.record_quality_outcome(QualityTarget::Provider("p2"), false);

    assert_eq!(observations(&router, "p1"), p1_before);
    assert_eq!(observations(&router, "p2"), p2_before + 1);
}

#[tokio::test]
async fn test_record_quality_outcome_last_active_targets_serving_provider() {
    let dir = tempfile::tempdir().unwrap();
    let router = two_provider_router(&dir);
    router.chat_with_tools(&user(), &tools()).await.unwrap();
    let (p1_before, p2_before) = (observations(&router, "p1"), observations(&router, "p2"));

    router.record_quality_outcome(QualityTarget::LastActive, false);

    assert_eq!(
        observations(&router, "p1"),
        p1_before + 1,
        "p1 served the call"
    );
    assert_eq!(observations(&router, "p2"), p2_before);
}

#[tokio::test]
async fn test_any_provider_record_quality_outcome_forwards_target_to_router() {
    let dir = tempfile::tempdir().unwrap();
    let router = two_provider_router(&dir);
    let any = AnyProvider::Router(Box::new(router));
    any.chat_with_tools(&user(), &tools()).await.unwrap();
    any.record_quality_outcome(QualityTarget::Provider("p2"), false);
    let AnyProvider::Router(router) = &any else {
        unreachable!()
    };
    assert_eq!(observations(router, "p2"), 1);
}

// ---- #7165 -------------------------------------------------------------------------------

#[test]
fn test_should_shadow_tools_path_without_entropy_never_samples() {
    for rate in [0.0, 0.5, 1.0] {
        assert!(
            !should_shadow(None, &cfg(rate), CoePath::Tools),
            "rate {rate}"
        );
    }
}

#[test]
fn test_should_shadow_tools_path_with_high_entropy_still_shadows() {
    assert!(should_shadow(Some(0.9), &cfg(0.0), CoePath::Tools));
}

#[test]
fn test_should_shadow_chat_path_keeps_sampling_without_entropy() {
    assert!(should_shadow(None, &cfg(1.0), CoePath::Chat));
    assert!(!should_shadow(None, &cfg(0.0), CoePath::Chat));
}

#[tokio::test]
async fn test_chat_with_tools_without_entropy_never_calls_secondary_even_at_full_sample_rate() {
    let (primary, _) = MockProvider::default().with_tool_use(vec![ChatResponse::text("primary")]);
    let (secondary, secondary_calls) =
        MockProvider::default().with_tool_use(vec![ChatResponse::text("secondary")]);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary)]).with_coe(
        cfg(1.0),
        AnyProvider::Mock(secondary),
        embed(),
    );

    let resp = router.chat_with_tools(&user(), &tools()).await.unwrap();

    assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "primary"));
    assert_eq!(*secondary_calls.lock().unwrap(), 0);
}

// ---- #7164 -------------------------------------------------------------------------------

#[tokio::test]
async fn test_tool_path_escalation_serves_secondary_usage_and_demotes_primary_reputation() {
    async fn run(with_coe: bool) -> (RouterProvider, bool) {
        let dir = tempfile::tempdir().unwrap();
        let (primary, _) = MockProvider::default()
            .with_name("primary")
            .with_tool_use(vec![ChatResponse::text("unsure")]);
        let (secondary, _) = MockProvider::default()
            .with_name("secondary")
            .with_tool_use(vec![ChatResponse::text("sure")]);
        let primary = primary.with_entropy(1.0).with_published_usage(usage(1, 1));
        let secondary = secondary.with_published_usage(usage(9, 9));
        let mut router = RouterProvider::new(vec![AnyProvider::Mock(primary)]).with_reputation(
            1.0,
            0.5,
            1,
            Some(&dir.path().join("rep.json")),
        );
        if with_coe {
            router = router.with_coe(cfg(0.0), AnyProvider::Mock(secondary), embed());
        }
        let resp = router.chat_with_tools(&user(), &tools()).await.unwrap();
        let escalated = matches!(resp, ChatResponse::Text { ref text, .. } if text == "sure");
        (router, escalated)
    }

    let (baseline, _) = run(false).await;
    let (escalated_router, escalated) = run(true).await;
    assert!(escalated);

    let served = escalated_router.last_response_usage().expect("usage");
    assert_eq!(
        (served.input, served.output),
        (9, 9),
        "secondary is the served usage"
    );

    let beta = |r: &RouterProvider| {
        r.reputation_stats()
            .iter()
            .find(|s| s.0 == "primary")
            .map_or(0.0, |s| s.2)
    };
    assert!(
        beta(&escalated_router) > beta(&baseline),
        "primary quality demoted"
    );
}

// ---- #7166 -------------------------------------------------------------------------------

#[tokio::test]
async fn test_coe_metrics_secondary_error_counts_only_secondary_failures() {
    let primary = MockProvider::with_responses(vec!["primary".into()]).with_entropy(1.0);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary)]).with_coe(
        cfg(0.0),
        AnyProvider::Mock(MockProvider::failing()),
        embed(),
    );

    assert_eq!(router.chat(&user()).await.unwrap(), "primary");

    let m = router.coe_metrics().unwrap();
    assert_eq!((m.secondary_failures, m.shadow_timeouts), (1, 0));
}

#[tokio::test(start_paused = true)]
async fn test_coe_metrics_shadow_timeout_counts_only_shadow_timeouts() {
    let primary = MockProvider::with_responses(vec!["primary".into()]).with_entropy(1.0);
    let stalled = MockProvider::with_responses(vec!["secondary".into()]).with_delay(600_000);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary)]).with_coe(
        cfg(0.0),
        AnyProvider::Mock(stalled),
        embed(),
    );

    assert_eq!(router.chat(&user()).await.unwrap(), "primary");

    let m = router.coe_metrics().unwrap();
    assert_eq!((m.secondary_failures, m.shadow_timeouts), (0, 1));
}

#[tokio::test]
async fn test_coe_metrics_snapshot_counts_kept_and_intra_escalations() {
    let primary = MockProvider::with_responses(vec!["primary".into()]).with_entropy(0.1);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary)]).with_coe(
        cfg(0.0),
        AnyProvider::Mock(MockProvider::with_responses(vec!["secondary".into()])),
        embed(),
    );
    router.chat(&user()).await.unwrap();
    let m = router.coe_metrics().unwrap();
    assert_eq!(
        (m.kept_primary, m.intra_escalations, m.inter_escalations),
        (1, 0, 0)
    );
}
