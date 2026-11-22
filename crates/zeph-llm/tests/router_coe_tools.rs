// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(feature = "testing")]
#![expect(
    clippy::unwrap_used,
    reason = "Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests (tests-first) for #7143: Chain-of-Entropy on `RouterProvider::chat_with_tools`.
//!
//! Scope per the plan: text answers can escalate (intra entropy), tool-call answers are kept
//! without a shadow call, a secondary tool-call answer may replace a primary text answer, the
//! secondary honours vision/tool capabilities, escalation keeps primary request metadata, and the
//! tool path makes no reputation writes. `chat_stream` is out of scope (design decision D1).

use std::sync::{Arc, Mutex};

use zeph_common::{ToolCallId, ToolName};
use zeph_llm::ThinkingReplay;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{
    AssistantContent, ChatResponse, ImageData, LlmProvider, Message, MessagePart, Role,
    ToolDefinition, ToolUseRequest,
};
use zeph_llm::router::RouterProvider;
use zeph_llm::router::coe::CoeConfig;

fn cfg() -> CoeConfig {
    CoeConfig {
        intra_threshold: 0.8,
        inter_threshold: 0.2,
        shadow_sample_rate: 0.0,
        ..CoeConfig::default()
    }
}

fn embed() -> AnyProvider {
    let mut m = MockProvider::default();
    m.supports_embeddings = true;
    AnyProvider::Mock(m)
}

fn tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: ToolName::new("shell"),
        description: "run".into(),
        parameters: serde_json::json!({"type": "object"}),
        output_schema: None,
    }]
}

fn tool_use() -> ChatResponse {
    ChatResponse::ToolUse {
        content: AssistantContent::from_text_and_calls(
            None,
            vec![ToolUseRequest {
                id: ToolCallId::new("call_1"),
                name: ToolName::new("shell"),
                input: serde_json::json!({}),
            }],
        ),
        replay: ThinkingReplay::Intact,
        compaction: None,
    }
}

fn text_with(text: &str, replay: ThinkingReplay, compaction: Option<&str>) -> ChatResponse {
    ChatResponse::Text {
        text: text.into(),
        replay,
        compaction: compaction.map(str::to_owned),
    }
}

fn user(text: &str) -> Vec<Message> {
    vec![Message::from_legacy(Role::User, text)]
}

fn calls(counter: &Arc<Mutex<u32>>) -> u32 {
    *counter.lock().unwrap()
}

#[tokio::test]
async fn chat_with_tools_high_entropy_text_escalates_to_secondary_text() {
    let (primary, _) = MockProvider::default()
        .with_name("primary")
        .with_tool_use(vec![ChatResponse::text("primary answer")]);
    let (secondary, _) = MockProvider::default()
        .with_name("secondary")
        .with_tool_use(vec![ChatResponse::text("secondary answer")]);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary.with_entropy(1.0))]).with_coe(
        cfg(),
        AnyProvider::Mock(secondary),
        embed(),
    );

    let resp = router.chat_with_tools(&user("q"), &tools()).await.unwrap();

    assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "secondary answer"));
    let (kept, intra, _inter, _fail) = router.coe_metrics().unwrap();
    assert_eq!((kept, intra), (0, 1));
}

#[tokio::test]
async fn chat_with_tools_low_entropy_text_keeps_primary_and_counts_kept() {
    let (primary, _) = MockProvider::default().with_tool_use(vec![ChatResponse::text("primary")]);
    let (secondary, secondary_calls) =
        MockProvider::default().with_tool_use(vec![ChatResponse::text("secondary")]);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary.with_entropy(0.1))]).with_coe(
        cfg(),
        AnyProvider::Mock(secondary),
        embed(),
    );

    let resp = router.chat_with_tools(&user("q"), &tools()).await.unwrap();

    assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "primary"));
    assert_eq!(calls(&secondary_calls), 0);
    assert_eq!(router.coe_metrics().unwrap().0, 1);
}

#[tokio::test]
async fn chat_with_tools_tool_use_primary_is_kept_without_shadow_call_or_metric() {
    let (primary, _) = MockProvider::default().with_tool_use(vec![tool_use()]);
    let (secondary, secondary_calls) =
        MockProvider::default().with_tool_use(vec![ChatResponse::text("secondary")]);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary.with_entropy(1.0))]).with_coe(
        cfg(),
        AnyProvider::Mock(secondary),
        embed(),
    );

    let resp = router.chat_with_tools(&user("q"), &tools()).await.unwrap();

    assert!(matches!(resp, ChatResponse::ToolUse { .. }));
    assert_eq!(calls(&secondary_calls), 0);
    assert_eq!(router.coe_metrics().unwrap(), (0, 0, 0, 0));
}

#[tokio::test]
async fn chat_with_tools_secondary_tool_use_replaces_primary_text() {
    let (primary, _) = MockProvider::default().with_tool_use(vec![ChatResponse::text("unsure")]);
    let (secondary, _) = MockProvider::default().with_tool_use(vec![tool_use()]);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary.with_entropy(1.0))]).with_coe(
        cfg(),
        AnyProvider::Mock(secondary),
        embed(),
    );

    let resp = router.chat_with_tools(&user("q"), &tools()).await.unwrap();

    assert!(matches!(resp, ChatResponse::ToolUse { .. }));
}

#[tokio::test]
async fn chat_with_tools_secondary_without_tool_support_skips_coe() {
    let (primary, _) = MockProvider::default().with_tool_use(vec![ChatResponse::text("primary")]);
    let (secondary, secondary_calls) =
        MockProvider::default().with_tool_use(vec![ChatResponse::text("secondary")]);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary.with_entropy(1.0))]).with_coe(
        cfg(),
        AnyProvider::Mock(secondary.without_tool_use()),
        embed(),
    );

    let resp = router.chat_with_tools(&user("q"), &tools()).await.unwrap();

    assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "primary"));
    assert_eq!(calls(&secondary_calls), 0);
}

#[tokio::test]
async fn chat_with_tools_non_vision_secondary_receives_messages_without_images() {
    let message = Message::from_parts(
        Role::User,
        vec![
            MessagePart::Text {
                text: "look".into(),
            },
            MessagePart::Image(Box::new(ImageData {
                data: vec![1, 2, 3],
                mime_type: "image/png".into(),
            })),
        ],
    );
    let (primary, _) = MockProvider::default().with_tool_use(vec![ChatResponse::text("unsure")]);
    let (secondary, _) = MockProvider::default().with_tool_use(vec![ChatResponse::text("sure")]);
    let (secondary, recorded) = secondary.with_recording();
    let router = RouterProvider::new(vec![AnyProvider::Mock(
        primary.with_vision().with_entropy(1.0),
    )])
    .with_coe(cfg(), AnyProvider::Mock(secondary), embed());

    router.chat_with_tools(&[message], &tools()).await.unwrap();

    let recorded = recorded.lock().unwrap();
    assert_eq!(
        recorded.len(),
        1,
        "secondary must be consulted exactly once"
    );
    assert!(
        !recorded[0]
            .iter()
            .flat_map(|m| &m.parts)
            .any(|p| matches!(p, MessagePart::Image(_)))
    );
}

#[tokio::test]
async fn chat_with_tools_escalation_keeps_primary_replay_and_compaction() {
    let dropped = ThinkingReplay::Dropped {
        first_message_index: 3,
    };
    let (primary, _) = MockProvider::default().with_tool_use(vec![text_with(
        "unsure",
        dropped,
        Some("primary summary"),
    )]);
    let (secondary, _) = MockProvider::default().with_tool_use(vec![text_with(
        "secondary answer",
        ThinkingReplay::Intact,
        None,
    )]);
    let router = RouterProvider::new(vec![AnyProvider::Mock(primary.with_entropy(1.0))]).with_coe(
        cfg(),
        AnyProvider::Mock(secondary),
        embed(),
    );

    let mut resp = router.chat_with_tools(&user("q"), &tools()).await.unwrap();

    assert!(matches!(resp, ChatResponse::Text { ref text, .. } if text == "secondary answer"));
    assert_eq!(resp.replay(), dropped);
    assert_eq!(resp.take_compaction().as_deref(), Some("primary summary"));
}

#[tokio::test]
async fn chat_with_tools_escalation_leaves_reputation_tracker_as_without_coe() {
    async fn stats(with_coe: bool) -> (Vec<(String, f64, f64, f64, u64)>, bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reputation.json");
        let (primary, _) = MockProvider::default()
            .with_name("primary")
            .with_tool_use(vec![ChatResponse::text("unsure")]);
        let (secondary, _) = MockProvider::default()
            .with_name("secondary")
            .with_tool_use(vec![ChatResponse::text("sure")]);
        let mut router = RouterProvider::new(vec![AnyProvider::Mock(primary.with_entropy(1.0))])
            .with_reputation(1.0, 0.5, 1, Some(&path));
        if with_coe {
            router = router.with_coe(cfg(), AnyProvider::Mock(secondary), embed());
        }
        let resp = router.chat_with_tools(&user("q"), &tools()).await.unwrap();
        let escalated = matches!(resp, ChatResponse::Text { ref text, .. } if text == "sure");
        let mut stats = router.reputation_stats();
        stats.sort_by(|a, b| a.0.cmp(&b.0));
        (stats, escalated)
    }

    let (baseline, _) = stats(false).await;
    let (with_coe, escalated) = stats(true).await;

    assert!(escalated, "CoE must have escalated on the tool path");
    assert_eq!(with_coe, baseline);
}
