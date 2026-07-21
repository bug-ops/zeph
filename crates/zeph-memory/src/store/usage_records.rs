// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Durable per-LLM-call usage ledger (`usage_records` table, issue #6549).

use zeph_db::ActiveDialect;
use zeph_db::sql;

use super::SqliteStore;
use crate::error::MemoryError;
use crate::types::{ConversationId, MessageId, UsageRecord, UsageSource};

/// Row shape shared by every `usage_records` SELECT in this module.
type UsageRow = (
    Option<MessageId>,
    Option<ConversationId>,
    String,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
    f64,
    i64,
    Option<i64>,
    Option<f64>,
);

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn row_to_usage_record(row: UsageRow) -> UsageRecord {
    let (
        message_id,
        conversation_id,
        source_str,
        provider_name,
        model_name,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        cost_cents,
        latency_ms,
        ttft_ms,
        tokens_per_sec,
    ) = row;
    let source = source_str.parse().unwrap_or_else(|_| {
        tracing::warn!(value = %source_str, "unrecognized usage_records.source, defaulting to conversation");
        UsageSource::Conversation
    });
    UsageRecord {
        message_id,
        conversation_id,
        source,
        provider_name,
        model_name,
        input_tokens: to_u64(input_tokens),
        output_tokens: to_u64(output_tokens),
        cache_read_tokens: to_u64(cache_read_tokens),
        cache_write_tokens: to_u64(cache_write_tokens),
        reasoning_tokens: reasoning_tokens.map(to_u64),
        cost_cents,
        latency_ms: to_u64(latency_ms),
        ttft_ms: ttft_ms.map(to_u64),
        tokens_per_sec,
    }
}

impl SqliteStore {
    /// Insert one durable usage row.
    ///
    /// Callers write exactly one row per production `CostTracker::record_usage`-feeding
    /// call site — see spec `082-per-message-usage-cost-tracking` §3 for the completeness
    /// invariant. `record.cost_cents` must come from `CostTracker::price_of` so the row's
    /// cost matches the live daily aggregate for the same call.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn record_usage_row(&self, record: &UsageRecord) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "INSERT INTO usage_records \
             (message_id, conversation_id, source, provider_name, model_name, \
              input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
              reasoning_tokens, cost_cents, latency_ms, ttft_ms, tokens_per_sec) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(record.message_id)
        .bind(record.conversation_id)
        .bind(record.source.as_str())
        .bind(&record.provider_name)
        .bind(&record.model_name)
        .bind(to_i64(record.input_tokens))
        .bind(to_i64(record.output_tokens))
        .bind(to_i64(record.cache_read_tokens))
        .bind(to_i64(record.cache_write_tokens))
        .bind(record.reasoning_tokens.map(to_i64))
        .bind(record.cost_cents)
        .bind(to_i64(record.latency_ms))
        .bind(record.ttft_ms.map(to_i64))
        .bind(record.tokens_per_sec)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch the usage row for a single conversational message, if one was recorded.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn message_usage(
        &self,
        message_id: MessageId,
    ) -> Result<Option<UsageRecord>, MemoryError> {
        let row: Option<UsageRow> = zeph_db::query_as(sql!(
            "SELECT message_id, conversation_id, source, provider_name, model_name, \
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
                    reasoning_tokens, cost_cents, latency_ms, ttft_ms, tokens_per_sec \
             FROM usage_records WHERE message_id = ?"
        ))
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_usage_record))
    }

    /// Fetch every conversational usage row for a conversation, ordered by message id.
    ///
    /// Background/orchestration rows (planner, aggregator, ensemble member) carry no
    /// `message_id` and are excluded — use a direct `usage_records` query if those are
    /// needed alongside conversational rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn conversation_usage(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<UsageRecord>, MemoryError> {
        let rows: Vec<UsageRow> = zeph_db::query_as(sql!(
            "SELECT message_id, conversation_id, source, provider_name, model_name, \
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
                    reasoning_tokens, cost_cents, latency_ms, ttft_ms, tokens_per_sec \
             FROM usage_records \
             WHERE conversation_id = ? AND message_id IS NOT NULL \
             ORDER BY message_id ASC"
        ))
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_usage_record).collect())
    }

    /// Sum `cost_cents` across every usage row created at or after `since_epoch_secs`
    /// (Unix epoch seconds, UTC).
    ///
    /// Used for the current-day reconciliation invariant: `usage_cost_since(utc_midnight)`
    /// must equal `CostTracker::current_spend()` (spec `082` US-001 AC).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn usage_cost_since(&self, since_epoch_secs: i64) -> Result<f64, MemoryError> {
        let epoch_expr = <ActiveDialect as zeph_db::dialect::Dialect>::epoch_from_col("created_at");
        let raw = format!(
            "SELECT COALESCE(SUM(cost_cents), 0.0) FROM usage_records WHERE {epoch_expr} >= ?"
        );
        let sql = zeph_db::rewrite_placeholders(&raw);
        let total: f64 = zeph_db::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(since_epoch_secs)
            .fetch_one(&self.pool)
            .await?;
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(source: UsageSource) -> UsageRecord {
        UsageRecord {
            message_id: None,
            conversation_id: None,
            source,
            provider_name: "quality".to_string(),
            model_name: "claude-sonnet-5".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 5,
            reasoning_tokens: Some(20),
            cost_cents: 0.42,
            latency_ms: 800,
            ttft_ms: Some(120),
            tokens_per_sec: Some(63.5),
        }
    }

    #[tokio::test]
    async fn record_and_fetch_conversational_row() {
        let store = SqliteStore::new(":memory:").await.expect("store");
        let cid = store.create_conversation().await.expect("conversation");
        let mid = store
            .save_message(cid, "assistant", "hello")
            .await
            .expect("message");

        let mut record = sample(UsageSource::Conversation);
        record.message_id = Some(mid);
        record.conversation_id = Some(cid);
        store.record_usage_row(&record).await.expect("insert");

        let fetched = store
            .message_usage(mid)
            .await
            .expect("query")
            .expect("row exists");
        assert_eq!(fetched.source, UsageSource::Conversation);
        assert_eq!(fetched.input_tokens, 100);
        assert_eq!(fetched.output_tokens, 50);
        assert_eq!(fetched.cache_read_tokens, 10);
        assert_eq!(fetched.cache_write_tokens, 5);
        assert_eq!(fetched.reasoning_tokens, Some(20));
        assert!((fetched.cost_cents - 0.42).abs() < 1e-9);
        assert_eq!(fetched.ttft_ms, Some(120));

        let conv_rows = store.conversation_usage(cid).await.expect("conv query");
        assert_eq!(conv_rows.len(), 1);
        assert_eq!(conv_rows[0].message_id, Some(mid));
    }

    #[tokio::test]
    async fn background_rows_have_no_message_id_and_are_excluded_from_conversation_usage() {
        let store = SqliteStore::new(":memory:").await.expect("store");
        let cid = store.create_conversation().await.expect("conversation");

        let mut planner_row = sample(UsageSource::Planner);
        planner_row.conversation_id = Some(cid);
        store.record_usage_row(&planner_row).await.expect("insert");

        let conv_rows = store.conversation_usage(cid).await.expect("conv query");
        assert!(conv_rows.is_empty(), "background rows carry no message_id");
    }

    #[tokio::test]
    async fn message_usage_none_when_unrecorded() {
        let store = SqliteStore::new(":memory:").await.expect("store");
        let cid = store.create_conversation().await.expect("conversation");
        let mid = store
            .save_message(cid, "assistant", "no usage row")
            .await
            .expect("message");
        assert!(store.message_usage(mid).await.expect("query").is_none());
    }

    #[tokio::test]
    async fn usage_cost_since_sums_rows_in_window() {
        let store = SqliteStore::new(":memory:").await.expect("store");
        store
            .record_usage_row(&sample(UsageSource::Aggregator))
            .await
            .expect("insert 1");
        store
            .record_usage_row(&sample(UsageSource::EnsembleMember))
            .await
            .expect("insert 2");

        let total = store.usage_cost_since(0).await.expect("sum");
        assert!((total - 0.84).abs() < 1e-6, "total={total}");

        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .cast_signed()
            + 3600;
        let none_yet = store.usage_cost_since(future).await.expect("sum future");
        assert!((none_yet - 0.0).abs() < 1e-9);
    }
}
