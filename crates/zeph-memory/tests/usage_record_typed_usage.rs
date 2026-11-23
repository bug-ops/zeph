// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Fixture construction failures should fail the test immediately.; Fixture construction failures should fail the test immediately."
)]

//! Acceptance tests (tests-first) for #7137: `UsageRecord` carries a typed
//! `usage: TokenUsage` instead of four loose counters; DB columns are unchanged.

use zeph_llm::provider::TokenUsage;
use zeph_memory::store::SqliteStore;
use zeph_memory::{UsageRecord, UsageSource};

fn record(
    message_id: Option<zeph_memory::MessageId>,
    cid: Option<zeph_memory::ConversationId>,
) -> UsageRecord {
    UsageRecord {
        message_id,
        conversation_id: cid,
        source: UsageSource::Conversation,
        provider_name: "quality".to_owned(),
        model_name: "m".to_owned(),
        usage: TokenUsage {
            input: 100,
            output: 50,
            cache_write: 7,
            cache_read: 3,
        },
        reasoning_tokens: Some(11),
        cost_cents: zeph_common::money::Cents::new(0.25),
        latency_ms: 800,
        ttft_ms: Some(120),
        tokens_per_sec: Some(42.5),
    }
}

#[tokio::test]
async fn usage_record_round_trips_typed_usage_through_the_store() {
    let store = SqliteStore::new(":memory:").await.unwrap();
    let cid = store.create_conversation().await.unwrap();
    let mid = store.save_message(cid, "assistant", "hi").await.unwrap();
    let written = record(Some(mid), Some(cid));
    store.record_usage_row(&written).await.unwrap();

    let read = store.message_usage(mid).await.unwrap().expect("row exists");
    assert_eq!(read, written);
    assert_eq!(
        read.usage.cache_write, 7,
        "cache_write maps to cache_write_tokens"
    );
    assert_eq!(
        read.usage.cache_read, 3,
        "cache_read maps to cache_read_tokens"
    );
}

#[tokio::test]
async fn usage_record_conversation_usage_returns_typed_usage() {
    let store = SqliteStore::new(":memory:").await.unwrap();
    let cid = store.create_conversation().await.unwrap();
    let mid = store.save_message(cid, "assistant", "hi").await.unwrap();
    store
        .record_usage_row(&record(Some(mid), Some(cid)))
        .await
        .unwrap();
    let rows = store.conversation_usage(cid).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].usage,
        TokenUsage {
            input: 100,
            output: 50,
            cache_write: 7,
            cache_read: 3
        }
    );
}

#[tokio::test]
async fn usage_record_planner_row_without_ids_is_accepted() {
    let store = SqliteStore::new(":memory:").await.unwrap();
    let mut planner = record(None, None);
    planner.source = UsageSource::Planner;
    store.record_usage_row(&planner).await.unwrap();
    assert!(store.usage_cost_since(0).await.unwrap() > zeph_common::money::Cents::ZERO);
}
