// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0
#![expect(
    clippy::unwrap_used,
    reason = "Fixture construction failures should fail the test immediately."
)]

//! A fenced LLM reply must be unwrapped before the episodic consolidation sweep parses it.

use zeph_common::types::ProviderName;
use zeph_db::sql;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_memory::store::SqliteStore;
use zeph_memory::{EpisodicConsolidationConfig, run_episodic_consolidation_sweep};

#[tokio::test]
async fn run_episodic_consolidation_sweep_fenced_llm_output_promotes_fact() {
    let store = SqliteStore::new(":memory:").await.unwrap();
    let pool = store.pool().clone();
    let conv_id = store.create_conversation().await.unwrap();

    let msg_id: i64 = sqlx::query_scalar(sql!(
        "INSERT INTO messages (conversation_id, role, content)
         VALUES (?1, 'user', 'Alice uses Rust for systems programming')
         RETURNING id"
    ))
    .bind(conv_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let created_at = chrono::Utc::now().timestamp() - 600;
    let event_id: i64 = sqlx::query_scalar(sql!(
        "INSERT INTO episodic_events (session_id, message_id, event_type, summary, created_at)
         VALUES ('test_session', ?1, 'tool_call', 'Alice prefers Rust', ?2)
         RETURNING id"
    ))
    .bind(msg_id)
    .bind(created_at)
    .fetch_one(&pool)
    .await
    .unwrap();

    let fenced = format!(
        "```json\n[{{\"fact\":\"Alice uses Rust for systems programming\",\"source_event_ids\":[{event_id}]}}]\n```"
    );
    let mut mock = MockProvider::default();
    mock.default_response = fenced;
    let provider = AnyProvider::Mock(mock);
    let config = EpisodicConsolidationConfig {
        enabled: true,
        consolidation_provider: ProviderName::default(),
        interval_secs: 1800,
        batch_size: 30,
        min_age_secs: 300,
        dedup_jaccard_threshold: 0.6,
    };

    let result = run_episodic_consolidation_sweep(pool.clone(), &provider, &config, None)
        .await
        .unwrap();

    assert_eq!(result.facts_promoted, 1);
    let count: i64 = sqlx::query_scalar(sql!("SELECT COUNT(*) FROM consolidated_facts"))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}
