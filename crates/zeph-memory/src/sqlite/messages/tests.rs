// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

async fn test_store() -> SqliteStore {
    SqliteStore::new(":memory:").await.unwrap()
}

#[tokio::test]
async fn create_conversation_returns_id() {
    let store = test_store().await;
    let id1 = store.create_conversation().await.unwrap();
    let id2 = store.create_conversation().await.unwrap();
    assert_eq!(id1, ConversationId(1));
    assert_eq!(id2, ConversationId(2));
}

#[tokio::test]
async fn save_and_load_messages() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    let msg_id1 = store.save_message(cid, "user", "hello").await.unwrap();
    let msg_id2 = store
        .save_message(cid, "assistant", "hi there")
        .await
        .unwrap();

    assert_eq!(msg_id1, MessageId(1));
    assert_eq!(msg_id2, MessageId(2));

    let history = store.load_history(cid, 50).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].role, Role::User);
    assert_eq!(history[0].content, "hello");
    assert_eq!(history[1].role, Role::Assistant);
    assert_eq!(history[1].content, "hi there");
}

#[tokio::test]
async fn load_history_respects_limit() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    for i in 0..10 {
        store
            .save_message(cid, "user", &format!("msg {i}"))
            .await
            .unwrap();
    }

    let history = store.load_history(cid, 3).await.unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].content, "msg 7");
    assert_eq!(history[1].content, "msg 8");
    assert_eq!(history[2].content, "msg 9");
}

#[tokio::test]
async fn latest_conversation_id_empty() {
    let store = test_store().await;
    assert!(store.latest_conversation_id().await.unwrap().is_none());
}

#[tokio::test]
async fn latest_conversation_id_returns_newest() {
    let store = test_store().await;
    store.create_conversation().await.unwrap();
    let id2 = store.create_conversation().await.unwrap();
    assert_eq!(store.latest_conversation_id().await.unwrap(), Some(id2));
}

#[tokio::test]
async fn messages_isolated_per_conversation() {
    let store = test_store().await;
    let cid1 = store.create_conversation().await.unwrap();
    let cid2 = store.create_conversation().await.unwrap();

    store.save_message(cid1, "user", "conv1").await.unwrap();
    store.save_message(cid2, "user", "conv2").await.unwrap();

    let h1 = store.load_history(cid1, 50).await.unwrap();
    let h2 = store.load_history(cid2, 50).await.unwrap();
    assert_eq!(h1.len(), 1);
    assert_eq!(h1[0].content, "conv1");
    assert_eq!(h2.len(), 1);
    assert_eq!(h2[0].content, "conv2");
}

#[tokio::test]
async fn pool_accessor_returns_valid_pool() {
    let store = test_store().await;
    let pool = store.pool();
    let row: (i64,) = sqlx::query_as("SELECT 1").fetch_one(pool).await.unwrap();
    assert_eq!(row.0, 1);
}

#[tokio::test]
async fn embeddings_metadata_table_exists() {
    let store = test_store().await;
    let result: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='embeddings_metadata'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(result.0, 1);
}

#[tokio::test]
async fn cascade_delete_removes_embeddings_metadata() {
    let store = test_store().await;
    let pool = store.pool();

    let cid = store.create_conversation().await.unwrap();
    let msg_id = store.save_message(cid, "user", "test").await.unwrap();

    let point_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions) \
         VALUES (?, ?, ?)",
    )
    .bind(msg_id)
    .bind(&point_id)
    .bind(768_i64)
    .execute(pool)
    .await
    .unwrap();

    let before: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM embeddings_metadata WHERE message_id = ?")
            .bind(msg_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(before.0, 1);

    sqlx::query("DELETE FROM messages WHERE id = ?")
        .bind(msg_id)
        .execute(pool)
        .await
        .unwrap();

    let after: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM embeddings_metadata WHERE message_id = ?")
            .bind(msg_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(after.0, 0);
}

#[tokio::test]
async fn messages_by_ids_batch_fetch() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id1 = store.save_message(cid, "user", "hello").await.unwrap();
    let id2 = store.save_message(cid, "assistant", "hi").await.unwrap();
    let _id3 = store.save_message(cid, "user", "bye").await.unwrap();

    let results = store.messages_by_ids(&[id1, id2]).await.unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, id1);
    assert_eq!(results[0].1.content, "hello");
    assert_eq!(results[1].0, id2);
    assert_eq!(results[1].1.content, "hi");
}

#[tokio::test]
async fn messages_by_ids_empty_input() {
    let store = test_store().await;
    let results = store.messages_by_ids(&[]).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn messages_by_ids_nonexistent() {
    let store = test_store().await;
    let results = store
        .messages_by_ids(&[MessageId(999), MessageId(1000)])
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn message_by_id_fetches_existing() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let msg_id = store.save_message(cid, "user", "hello").await.unwrap();

    let msg = store.message_by_id(msg_id).await.unwrap();
    assert!(msg.is_some());
    let msg = msg.unwrap();
    assert_eq!(msg.role, Role::User);
    assert_eq!(msg.content, "hello");
}

#[tokio::test]
async fn message_by_id_returns_none_for_nonexistent() {
    let store = test_store().await;
    let msg = store.message_by_id(MessageId(999)).await.unwrap();
    assert!(msg.is_none());
}

#[tokio::test]
async fn unembedded_message_ids_returns_all_when_none_embedded() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store.save_message(cid, "user", "msg1").await.unwrap();
    store.save_message(cid, "assistant", "msg2").await.unwrap();

    let unembedded = store.unembedded_message_ids(None).await.unwrap();
    assert_eq!(unembedded.len(), 2);
    assert_eq!(unembedded[0].3, "msg1");
    assert_eq!(unembedded[1].3, "msg2");
}

#[tokio::test]
async fn unembedded_message_ids_excludes_embedded() {
    let store = test_store().await;
    let pool = store.pool();
    let cid = store.create_conversation().await.unwrap();

    let msg_id1 = store.save_message(cid, "user", "msg1").await.unwrap();
    let msg_id2 = store.save_message(cid, "assistant", "msg2").await.unwrap();

    let point_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO embeddings_metadata (message_id, qdrant_point_id, dimensions) \
         VALUES (?, ?, ?)",
    )
    .bind(msg_id1)
    .bind(&point_id)
    .bind(768_i64)
    .execute(pool)
    .await
    .unwrap();

    let unembedded = store.unembedded_message_ids(None).await.unwrap();
    assert_eq!(unembedded.len(), 1);
    assert_eq!(unembedded[0].0, msg_id2);
    assert_eq!(unembedded[0].3, "msg2");
}

#[tokio::test]
async fn unembedded_message_ids_respects_limit() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    for i in 0..10 {
        store
            .save_message(cid, "user", &format!("msg{i}"))
            .await
            .unwrap();
    }

    let unembedded = store.unembedded_message_ids(Some(3)).await.unwrap();
    assert_eq!(unembedded.len(), 3);
}

#[tokio::test]
async fn count_messages_returns_correct_count() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    assert_eq!(store.count_messages(cid).await.unwrap(), 0);

    store.save_message(cid, "user", "msg1").await.unwrap();
    store.save_message(cid, "assistant", "msg2").await.unwrap();

    assert_eq!(store.count_messages(cid).await.unwrap(), 2);
}

#[tokio::test]
async fn count_messages_after_filters_correctly() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    let id1 = store.save_message(cid, "user", "msg1").await.unwrap();
    let _id2 = store.save_message(cid, "assistant", "msg2").await.unwrap();
    let id3 = store.save_message(cid, "user", "msg3").await.unwrap();

    assert_eq!(
        store.count_messages_after(cid, MessageId(0)).await.unwrap(),
        3
    );
    assert_eq!(store.count_messages_after(cid, id1).await.unwrap(), 2);
    assert_eq!(store.count_messages_after(cid, id3).await.unwrap(), 0);
}

#[tokio::test]
async fn load_messages_range_basic() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    let msg_id1 = store.save_message(cid, "user", "msg1").await.unwrap();
    let msg_id2 = store.save_message(cid, "assistant", "msg2").await.unwrap();
    let msg_id3 = store.save_message(cid, "user", "msg3").await.unwrap();

    let msgs = store.load_messages_range(cid, msg_id1, 10).await.unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].0, msg_id2);
    assert_eq!(msgs[0].2, "msg2");
    assert_eq!(msgs[1].0, msg_id3);
    assert_eq!(msgs[1].2, "msg3");
}

#[tokio::test]
async fn load_messages_range_respects_limit() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store.save_message(cid, "user", "msg1").await.unwrap();
    store.save_message(cid, "assistant", "msg2").await.unwrap();
    store.save_message(cid, "user", "msg3").await.unwrap();

    let msgs = store
        .load_messages_range(cid, MessageId(0), 2)
        .await
        .unwrap();
    assert_eq!(msgs.len(), 2);
}

#[tokio::test]
async fn keyword_search_basic() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store
        .save_message(cid, "user", "rust programming language")
        .await
        .unwrap();
    store
        .save_message(cid, "assistant", "python is great too")
        .await
        .unwrap();
    store
        .save_message(cid, "user", "I love rust and cargo")
        .await
        .unwrap();

    let results = store.keyword_search("rust", 10, None).await.unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|(_, score)| *score > 0.0));
}

#[tokio::test]
async fn keyword_search_with_conversation_filter() {
    let store = test_store().await;
    let cid1 = store.create_conversation().await.unwrap();
    let cid2 = store.create_conversation().await.unwrap();

    store
        .save_message(cid1, "user", "hello world")
        .await
        .unwrap();
    store
        .save_message(cid2, "user", "hello universe")
        .await
        .unwrap();

    let results = store.keyword_search("hello", 10, Some(cid1)).await.unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn keyword_search_no_match() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store
        .save_message(cid, "user", "hello world")
        .await
        .unwrap();

    let results = store.keyword_search("nonexistent", 10, None).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn keyword_search_respects_limit() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    for i in 0..10 {
        store
            .save_message(cid, "user", &format!("test message {i}"))
            .await
            .unwrap();
    }

    let results = store.keyword_search("test", 3, None).await.unwrap();
    assert_eq!(results.len(), 3);
}

#[test]
fn sanitize_fts5_query_strips_special_chars() {
    assert_eq!(sanitize_fts5_query("skill-audit"), "skill audit");
    assert_eq!(sanitize_fts5_query("hello, world"), "hello world");
    assert_eq!(sanitize_fts5_query("a+b*c^d"), "a b c d");
    assert_eq!(sanitize_fts5_query("  "), "");
    assert_eq!(sanitize_fts5_query("rust programming"), "rust programming");
}

#[tokio::test]
async fn keyword_search_with_special_chars_does_not_error() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    store
        .save_message(cid, "user", "skill audit info")
        .await
        .unwrap();
    // query with comma and special chars — previously caused FTS5 syntax error
    // result may be empty; important is that no error is returned
    store
        .keyword_search("skill-audit, confidence=0.1", 10, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn save_message_with_metadata_stores_visibility() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    let id = store
        .save_message_with_metadata(cid, "user", "hello", "[]", false, true)
        .await
        .unwrap();

    let history = store.load_history(cid, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert!(!history[0].metadata.agent_visible);
    assert!(history[0].metadata.user_visible);
    assert_eq!(id, MessageId(1));
}

#[tokio::test]
async fn load_history_filtered_by_agent_visible() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store
        .save_message_with_metadata(cid, "user", "visible to agent", "[]", true, true)
        .await
        .unwrap();
    store
        .save_message_with_metadata(cid, "user", "user only", "[]", false, true)
        .await
        .unwrap();

    let agent_msgs = store
        .load_history_filtered(cid, 50, Some(true), None)
        .await
        .unwrap();
    assert_eq!(agent_msgs.len(), 1);
    assert_eq!(agent_msgs[0].content, "visible to agent");
}

#[tokio::test]
async fn load_history_filtered_by_user_visible() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store
        .save_message_with_metadata(cid, "system", "agent only summary", "[]", true, false)
        .await
        .unwrap();
    store
        .save_message_with_metadata(cid, "user", "user sees this", "[]", true, true)
        .await
        .unwrap();

    let user_msgs = store
        .load_history_filtered(cid, 50, None, Some(true))
        .await
        .unwrap();
    assert_eq!(user_msgs.len(), 1);
    assert_eq!(user_msgs[0].content, "user sees this");
}

#[tokio::test]
async fn load_history_filtered_no_filter_returns_all() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store
        .save_message_with_metadata(cid, "user", "msg1", "[]", true, false)
        .await
        .unwrap();
    store
        .save_message_with_metadata(cid, "user", "msg2", "[]", false, true)
        .await
        .unwrap();

    let all_msgs = store
        .load_history_filtered(cid, 50, None, None)
        .await
        .unwrap();
    assert_eq!(all_msgs.len(), 2);
}

#[tokio::test]
async fn replace_conversation_marks_originals_and_inserts_summary() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    let id1 = store.save_message(cid, "user", "first").await.unwrap();
    let id2 = store
        .save_message(cid, "assistant", "second")
        .await
        .unwrap();
    let id3 = store.save_message(cid, "user", "third").await.unwrap();

    let summary_id = store
        .replace_conversation(cid, id1..=id2, "system", "summary text")
        .await
        .unwrap();

    // Original messages should be user_only
    let all = store.load_history(cid, 50).await.unwrap();
    // id1 and id2 marked agent_visible=false, id3 untouched, summary inserted
    let by_id1 = all.iter().find(|m| m.content == "first").unwrap();
    assert!(!by_id1.metadata.agent_visible);
    assert!(by_id1.metadata.user_visible);

    let by_id2 = all.iter().find(|m| m.content == "second").unwrap();
    assert!(!by_id2.metadata.agent_visible);

    let by_id3 = all.iter().find(|m| m.content == "third").unwrap();
    assert!(by_id3.metadata.agent_visible);

    // Summary is agent_only (agent_visible=1, user_visible=0)
    let summary = all.iter().find(|m| m.content == "summary text").unwrap();
    assert!(summary.metadata.agent_visible);
    assert!(!summary.metadata.user_visible);
    assert!(summary_id > id3);
}

#[tokio::test]
async fn oldest_message_ids_returns_in_order() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    let id1 = store.save_message(cid, "user", "a").await.unwrap();
    let id2 = store.save_message(cid, "assistant", "b").await.unwrap();
    let id3 = store.save_message(cid, "user", "c").await.unwrap();

    let ids = store.oldest_message_ids(cid, 2).await.unwrap();
    assert_eq!(ids, vec![id1, id2]);
    assert!(ids[0] < ids[1]);

    let all_ids = store.oldest_message_ids(cid, 10).await.unwrap();
    assert_eq!(all_ids, vec![id1, id2, id3]);
}

#[tokio::test]
async fn message_metadata_default_both_visible() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store.save_message(cid, "user", "normal").await.unwrap();

    let history = store.load_history(cid, 10).await.unwrap();
    assert!(history[0].metadata.agent_visible);
    assert!(history[0].metadata.user_visible);
    assert!(history[0].metadata.compacted_at.is_none());
}

#[tokio::test]
async fn load_history_empty_parts_json_fast_path() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store
        .save_message_with_parts(cid, "user", "hello", "[]")
        .await
        .unwrap();

    let history = store.load_history(cid, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert!(
        history[0].parts.is_empty(),
        "\"[]\" fast-path must yield empty parts Vec"
    );
}

#[tokio::test]
async fn load_history_non_empty_parts_json_parsed() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    let parts_json = serde_json::to_string(&vec![MessagePart::ToolResult {
        tool_use_id: "t1".into(),
        content: "result".into(),
        is_error: false,
    }])
    .unwrap();

    store
        .save_message_with_parts(cid, "user", "hello", &parts_json)
        .await
        .unwrap();

    let history = store.load_history(cid, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].parts.len(), 1);
    assert!(
        matches!(&history[0].parts[0], MessagePart::ToolResult { content, .. } if content == "result")
    );
}

#[tokio::test]
async fn message_by_id_empty_parts_json_fast_path() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    let id = store
        .save_message_with_parts(cid, "user", "msg", "[]")
        .await
        .unwrap();

    let msg = store.message_by_id(id).await.unwrap().unwrap();
    assert!(
        msg.parts.is_empty(),
        "\"[]\" fast-path must yield empty parts Vec in message_by_id"
    );
}

#[tokio::test]
async fn messages_by_ids_empty_parts_json_fast_path() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    let id = store
        .save_message_with_parts(cid, "user", "msg", "[]")
        .await
        .unwrap();

    let results = store.messages_by_ids(&[id]).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].1.parts.is_empty(),
        "\"[]\" fast-path must yield empty parts Vec in messages_by_ids"
    );
}

#[tokio::test]
async fn load_history_filtered_empty_parts_json_fast_path() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store
        .save_message_with_metadata(cid, "user", "msg", "[]", true, true)
        .await
        .unwrap();

    let msgs = store
        .load_history_filtered(cid, 10, Some(true), None)
        .await
        .unwrap();
    assert_eq!(msgs.len(), 1);
    assert!(
        msgs[0].parts.is_empty(),
        "\"[]\" fast-path must yield empty parts Vec in load_history_filtered"
    );
}

// ── keyword_search_with_time_range tests ─────────────────────────────────

#[tokio::test]
async fn keyword_search_with_time_range_empty_query_returns_empty() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    store
        .save_message(cid, "user", "rust programming")
        .await
        .unwrap();

    // Empty query after sanitization returns Ok([]) without hitting FTS5.
    let results = store
        .keyword_search_with_time_range("", 10, None, None, None)
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn keyword_search_with_time_range_no_bounds_matches_like_keyword_search() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    store
        .save_message(cid, "user", "rust async programming")
        .await
        .unwrap();
    store
        .save_message(cid, "assistant", "python tutorial")
        .await
        .unwrap();

    // With no time bounds, should behave like keyword_search.
    let results = store
        .keyword_search_with_time_range("rust", 10, None, None, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn keyword_search_with_time_range_after_bound_excludes_old_messages() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store
        .save_message(cid, "user", "rust programming guide")
        .await
        .unwrap();
    store
        .save_message(cid, "user", "rust async patterns")
        .await
        .unwrap();

    // Use a far-future after bound — should exclude all messages.
    let results = store
        .keyword_search_with_time_range("rust", 10, None, Some("2099-01-01 00:00:00"), None)
        .await
        .unwrap();
    assert!(results.is_empty(), "no messages after year 2099");
}

#[tokio::test]
async fn keyword_search_with_time_range_before_bound_excludes_future_messages() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store
        .save_message(cid, "user", "rust programming guide")
        .await
        .unwrap();

    // Use a far-past before bound — should exclude all messages (created now, not in 2000).
    let results = store
        .keyword_search_with_time_range("rust", 10, None, None, Some("2000-01-01 00:00:00"))
        .await
        .unwrap();
    assert!(results.is_empty(), "no messages before year 2000");
}

#[tokio::test]
async fn keyword_search_with_time_range_wide_bounds_returns_results() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    store
        .save_message(cid, "user", "rust programming guide")
        .await
        .unwrap();
    store
        .save_message(cid, "assistant", "python basics")
        .await
        .unwrap();

    // Wide time window (past to future) should return all matching messages.
    let results = store
        .keyword_search_with_time_range(
            "rust",
            10,
            None,
            Some("2000-01-01 00:00:00"),
            Some("2099-12-31 23:59:59"),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn keyword_search_with_time_range_conversation_filter() {
    let store = test_store().await;
    let cid1 = store.create_conversation().await.unwrap();
    let cid2 = store.create_conversation().await.unwrap();

    store
        .save_message(cid1, "user", "rust memory safety")
        .await
        .unwrap();
    store
        .save_message(cid2, "user", "rust async patterns")
        .await
        .unwrap();

    let results = store
        .keyword_search_with_time_range(
            "rust",
            10,
            Some(cid1),
            Some("2000-01-01 00:00:00"),
            Some("2099-12-31 23:59:59"),
        )
        .await
        .unwrap();
    assert_eq!(
        results.len(),
        1,
        "conversation filter must restrict to cid1 only"
    );
}

// ── importance_score + access_count tests (#2021) ─────────────────────────

#[tokio::test]
async fn fetch_importance_scores_empty_input() {
    let store = test_store().await;
    let result = store.fetch_importance_scores(&[]).await.unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn fetch_importance_scores_batch_fetch() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    // Neutral content, user role → low marker/density, but non-zero overall.
    let id1 = store
        .save_message(cid, "user", "hello world")
        .await
        .unwrap();
    // Explicit marker → high importance.
    let id2 = store
        .save_message(cid, "user", "remember: the API key rotates weekly")
        .await
        .unwrap();

    let scores = store.fetch_importance_scores(&[id1, id2]).await.unwrap();
    assert_eq!(scores.len(), 2);

    let s1 = *scores.get(&id1).unwrap();
    let s2 = *scores.get(&id2).unwrap();
    assert!(s1 > 0.0 && s1 <= 1.0, "score must be in (0,1], got {s1}");
    assert!(
        s2 > s1,
        "marker message must score higher than plain hello, got s1={s1} s2={s2}"
    );
}

#[tokio::test]
async fn increment_access_counts_empty_guard() {
    // Empty slice must return Ok without any SQL execution.
    let store = test_store().await;
    store.increment_access_counts(&[]).await.unwrap();
}

#[tokio::test]
async fn increment_access_counts_updates_rows() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store.save_message(cid, "user", "test").await.unwrap();

    // Verify initial access_count is 0.
    let before: (i64,) = sqlx::query_as("SELECT access_count FROM messages WHERE id = ?")
        .bind(id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(before.0, 0);

    store.increment_access_counts(&[id]).await.unwrap();

    let after: (i64,) = sqlx::query_as("SELECT access_count FROM messages WHERE id = ?")
        .bind(id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(after.0, 1);
}

#[tokio::test]
async fn migration_039_default_importance_score_for_preexisting_rows() {
    // Simulate a row that existed before migration 039 by checking that
    // SQLite applies the DEFAULT 0.5 when importance_score is not specified.
    // In practice, SqliteStore::new applies all migrations including 039, so
    // any save_message call that omits importance_score would have defaulted.
    // Here we directly INSERT without the column to verify the schema default.
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    sqlx::query(
        "INSERT INTO messages (conversation_id, role, content, parts, agent_visible, user_visible) \
         VALUES (?, 'user', 'legacy row', '[]', 1, 1)",
    )
    .bind(cid)
    .execute(store.pool())
    .await
    .unwrap();

    let row: (f64,) =
        sqlx::query_as("SELECT importance_score FROM messages WHERE content = 'legacy row'")
            .fetch_one(store.pool())
            .await
            .unwrap();

    assert!(
        (row.0 - 0.5).abs() < f64::EPSILON,
        "legacy rows must default to importance_score = 0.5, got {}",
        row.0
    );
}

// ── Tier DB method tests (#2094) ─────────────────────────────────────────────

#[tokio::test]
async fn fetch_tiers_empty_input_returns_empty_map() {
    let store = test_store().await;
    let result = store.fetch_tiers(&[]).await.unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn fetch_tiers_new_messages_default_to_episodic() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id1 = store.save_message(cid, "user", "hello").await.unwrap();
    let id2 = store.save_message(cid, "assistant", "hi").await.unwrap();

    let tiers = store.fetch_tiers(&[id1, id2]).await.unwrap();
    assert_eq!(tiers.len(), 2);
    assert_eq!(tiers.get(&id1).map(String::as_str), Some("episodic"));
    assert_eq!(tiers.get(&id2).map(String::as_str), Some("episodic"));
}

#[tokio::test]
async fn fetch_tiers_nonexistent_ids_omitted() {
    let store = test_store().await;
    let tiers = store.fetch_tiers(&[MessageId(999)]).await.unwrap();
    assert!(tiers.is_empty());
}

#[tokio::test]
async fn fetch_tiers_returns_semantic_after_manual_promote() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store
        .save_message(cid, "user", "remember this")
        .await
        .unwrap();

    store.manual_promote(&[id]).await.unwrap();

    let tiers = store.fetch_tiers(&[id]).await.unwrap();
    assert_eq!(tiers.get(&id).map(String::as_str), Some("semantic"));
}

#[tokio::test]
async fn count_messages_by_tier_empty_db_returns_zeros() {
    let store = test_store().await;
    let (episodic, semantic) = store.count_messages_by_tier().await.unwrap();
    assert_eq!(episodic, 0);
    assert_eq!(semantic, 0);
}

#[tokio::test]
async fn count_messages_by_tier_all_episodic_initially() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    store.save_message(cid, "user", "msg1").await.unwrap();
    store.save_message(cid, "assistant", "msg2").await.unwrap();

    let (episodic, semantic) = store.count_messages_by_tier().await.unwrap();
    assert_eq!(episodic, 2);
    assert_eq!(semantic, 0);
}

#[tokio::test]
async fn count_messages_by_tier_reflects_manual_promotion() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id1 = store.save_message(cid, "user", "fact one").await.unwrap();
    let _id2 = store
        .save_message(cid, "assistant", "response")
        .await
        .unwrap();
    let id3 = store.save_message(cid, "user", "fact two").await.unwrap();

    store.manual_promote(&[id1, id3]).await.unwrap();

    let (episodic, semantic) = store.count_messages_by_tier().await.unwrap();
    assert_eq!(semantic, 2);
    assert_eq!(episodic, 1);
}

#[tokio::test]
async fn count_messages_by_tier_excludes_deleted() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store.save_message(cid, "user", "to delete").await.unwrap();
    store.soft_delete_messages(&[id]).await.unwrap();

    let (episodic, _) = store.count_messages_by_tier().await.unwrap();
    assert_eq!(episodic, 0, "soft-deleted messages must not be counted");
}

#[tokio::test]
async fn find_promotion_candidates_empty_when_no_messages() {
    let store = test_store().await;
    let candidates = store.find_promotion_candidates(1, 100).await.unwrap();
    assert!(candidates.is_empty());
}

#[tokio::test]
async fn find_promotion_candidates_empty_when_session_count_too_low() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    store
        .save_message(cid, "user", "low count msg")
        .await
        .unwrap();
    // session_count defaults to 0; min_sessions=1 → no candidates.
    let candidates = store.find_promotion_candidates(1, 100).await.unwrap();
    assert!(candidates.is_empty());
}

#[tokio::test]
async fn find_promotion_candidates_returns_rows_meeting_threshold() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store
        .save_message(cid, "user", "cross-session fact")
        .await
        .unwrap();

    // Simulate the fact appearing in 2 sessions by incrementing session_count directly.
    sqlx::query("UPDATE messages SET session_count = 2 WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let candidates = store.find_promotion_candidates(2, 100).await.unwrap();
    assert!(candidates.iter().any(|c| c.id == id));
}

#[tokio::test]
async fn find_promotion_candidates_excludes_already_semantic_rows() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store
        .save_message(cid, "user", "already promoted")
        .await
        .unwrap();

    sqlx::query("UPDATE messages SET session_count = 3, tier = 'semantic' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let candidates = store.find_promotion_candidates(1, 100).await.unwrap();
    assert!(
        !candidates.iter().any(|c| c.id == id),
        "semantic rows must not appear as candidates"
    );
}

#[tokio::test]
async fn find_promotion_candidates_respects_batch_size() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    for i in 0..5 {
        let id = store
            .save_message(cid, "user", &format!("fact {i}"))
            .await
            .unwrap();
        sqlx::query("UPDATE messages SET session_count = 5 WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();
    }

    let candidates = store.find_promotion_candidates(1, 3).await.unwrap();
    assert_eq!(candidates.len(), 3, "batch_size must cap the result count");
}

#[tokio::test]
async fn promote_to_semantic_creates_semantic_message_and_deletes_originals() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id1 = store.save_message(cid, "user", "fact a").await.unwrap();
    let id2 = store.save_message(cid, "user", "fact b").await.unwrap();

    let new_id = store
        .promote_to_semantic(cid, "merged: fact a and fact b", &[id1, id2])
        .await
        .unwrap();

    // The new message must be in the semantic tier.
    let tiers = store.fetch_tiers(&[new_id]).await.unwrap();
    assert_eq!(tiers.get(&new_id).map(String::as_str), Some("semantic"));

    // Originals must be soft-deleted (excluded from fetch_tiers).
    let orig_tiers = store.fetch_tiers(&[id1, id2]).await.unwrap();
    assert!(
        orig_tiers.is_empty(),
        "original messages must be soft-deleted after promotion"
    );
}

#[tokio::test]
async fn promote_to_semantic_returns_new_message_id_greater_than_originals() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id1 = store.save_message(cid, "user", "episodic a").await.unwrap();
    let id2 = store.save_message(cid, "user", "episodic b").await.unwrap();

    let new_id = store
        .promote_to_semantic(cid, "semantic merged", &[id1, id2])
        .await
        .unwrap();

    assert!(
        new_id > id2,
        "new semantic message id must be greater than the original ids"
    );
}

#[tokio::test]
async fn promote_to_semantic_empty_ids_returns_error() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let result = store.promote_to_semantic(cid, "should fail", &[]).await;
    assert!(result.is_err(), "empty original_ids must return an error");
}

#[tokio::test]
async fn promote_to_semantic_updates_tier_count() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store.save_message(cid, "user", "promote me").await.unwrap();

    let (before_e, before_s) = store.count_messages_by_tier().await.unwrap();
    assert_eq!(before_e, 1);
    assert_eq!(before_s, 0);

    store
        .promote_to_semantic(cid, "semantic version", &[id])
        .await
        .unwrap();

    let (after_e, after_s) = store.count_messages_by_tier().await.unwrap();
    // Original deleted (not counted), one new semantic inserted.
    assert_eq!(after_e, 0);
    assert_eq!(after_s, 1);
}

#[tokio::test]
async fn manual_promote_empty_input_is_no_op() {
    let store = test_store().await;
    let count = store.manual_promote(&[]).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn manual_promote_sets_tier_to_semantic() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store
        .save_message(cid, "user", "direct promote")
        .await
        .unwrap();

    let count = store.manual_promote(&[id]).await.unwrap();
    assert_eq!(count, 1);

    let tiers = store.fetch_tiers(&[id]).await.unwrap();
    assert_eq!(tiers.get(&id).map(String::as_str), Some("semantic"));
}

#[tokio::test]
async fn manual_promote_does_not_delete_originals() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store.save_message(cid, "user", "keep me").await.unwrap();

    store.manual_promote(&[id]).await.unwrap();

    // Message still present (not soft-deleted) — just tier changed.
    let msg = store.message_by_id(id).await.unwrap();
    assert!(
        msg.is_some(),
        "manual_promote must not soft-delete the original"
    );
}

#[tokio::test]
async fn manual_promote_is_idempotent() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store
        .save_message(cid, "user", "already semantic")
        .await
        .unwrap();

    store.manual_promote(&[id]).await.unwrap();
    // Second call: already semantic, rows_affected = 0 but no error.
    let count = store.manual_promote(&[id]).await.unwrap();
    assert_eq!(
        count, 0,
        "second call on already-semantic row must affect 0 rows"
    );

    let tiers = store.fetch_tiers(&[id]).await.unwrap();
    assert_eq!(tiers.get(&id).map(String::as_str), Some("semantic"));
}

#[tokio::test]
async fn manual_promote_skips_nonexistent_ids() {
    let store = test_store().await;
    let count = store.manual_promote(&[MessageId(9999)]).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn migration_042_default_tier_for_preexisting_rows() {
    // Directly INSERT without the tier column to verify the schema DEFAULT applies.
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    sqlx::query(
        "INSERT INTO messages (conversation_id, role, content, parts, agent_visible, user_visible) \
         VALUES (?, 'user', 'legacy row', '[]', 1, 1)",
    )
    .bind(cid)
    .execute(store.pool())
    .await
    .unwrap();

    let row: (String,) = sqlx::query_as("SELECT tier FROM messages WHERE content = 'legacy row'")
        .fetch_one(store.pool())
        .await
        .unwrap();

    assert_eq!(
        row.0, "episodic",
        "legacy rows must default to 'episodic' tier"
    );
}

#[tokio::test]
async fn migration_042_default_session_count_for_preexisting_rows() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();

    sqlx::query(
        "INSERT INTO messages (conversation_id, role, content, parts, agent_visible, user_visible) \
         VALUES (?, 'user', 'session count row', '[]', 1, 1)",
    )
    .bind(cid)
    .execute(store.pool())
    .await
    .unwrap();

    let row: (i64,) =
        sqlx::query_as("SELECT session_count FROM messages WHERE content = 'session count row'")
            .fetch_one(store.pool())
            .await
            .unwrap();

    assert_eq!(row.0, 0, "legacy rows must default to session_count = 0");
}

#[tokio::test]
async fn promote_to_semantic_with_sentinel_zero_fails() {
    // Regression guard: ConversationId(0) must never be used as the FK value —
    // conversations uses AUTOINCREMENT starting at 1, so id=0 never exists.
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store.save_message(cid, "user", "fact x").await.unwrap();

    let result = store
        .promote_to_semantic(ConversationId(0), "merged", &[id])
        .await;
    assert!(
        result.is_err(),
        "promote_to_semantic with ConversationId(0) must fail FK constraint"
    );
}

#[tokio::test]
async fn find_promotion_candidates_returns_conversation_id() {
    let store = test_store().await;
    let cid = store.create_conversation().await.unwrap();
    let id = store
        .save_message(cid, "user", "cross-session fact")
        .await
        .unwrap();

    sqlx::query("UPDATE messages SET session_count = 3 WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let candidates = store.find_promotion_candidates(2, 100).await.unwrap();
    let candidate = candidates.iter().find(|c| c.id == id).unwrap();
    assert_eq!(
        candidate.conversation_id, cid,
        "find_promotion_candidates must return the source conversation_id"
    );
}
