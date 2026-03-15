// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SQLite-backed store for ACON compression guidelines and failure pairs.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use crate::error::MemoryError;
use crate::sqlite::SqliteStore;
use crate::types::ConversationId;

static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:sk-|sk_live_|sk_test_|AKIA|ghp_|gho_|-----BEGIN|xoxb-|xoxp-|AIza|ya29\.|glpat-|hf_|npm_|dckr_pat_)[^\s"'`,;\{\}\[\]]*"#,
    )
    .expect("secret regex")
});

static PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:/home/|/Users/|/root/|/tmp/|/var/)[^\s"'`,;\{\}\[\]]*"#).expect("path regex")
});

/// Matches `Authorization: Bearer <token>` headers; captures the token value for redaction.
static BEARER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(Authorization:\s*Bearer\s+)\S+").expect("bearer regex"));

/// Matches standalone JWT tokens (three Base64url-encoded parts separated by dots).
/// The signature segment uses `*` to handle `alg=none` JWTs with an empty signature.
static JWT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*").expect("jwt regex")
});

/// Redact secrets and filesystem paths from text before persistent storage.
///
/// Returns `Cow::Borrowed` when no sensitive content is found (zero-alloc fast path).
fn redact_sensitive(text: &str) -> Cow<'_, str> {
    // Each replace_all may return Cow::Borrowed (no match) or Cow::Owned (replaced).
    // We materialise intermediate Owned values into String so that subsequent steps
    // do not hold a borrow of a local.
    let s0: Cow<'_, str> = SECRET_RE.replace_all(text, "[REDACTED]");
    let s1: Cow<'_, str> = match PATH_RE.replace_all(s0.as_ref(), "[PATH]") {
        Cow::Borrowed(_) => s0,
        Cow::Owned(o) => Cow::Owned(o),
    };
    // Replace only the token value in Bearer headers, keeping the header name intact.
    let s2: Cow<'_, str> = match BEARER_RE.replace_all(s1.as_ref(), "${1}[REDACTED]") {
        Cow::Borrowed(_) => s1,
        Cow::Owned(o) => Cow::Owned(o),
    };
    match JWT_RE.replace_all(s2.as_ref(), "[REDACTED_JWT]") {
        Cow::Borrowed(_) => s2,
        Cow::Owned(o) => Cow::Owned(o),
    }
}

/// A recorded compression failure pair: the compressed context and the response
/// that indicated context was lost.
#[derive(Debug, Clone)]
pub struct CompressionFailurePair {
    pub id: i64,
    pub conversation_id: ConversationId,
    pub compressed_context: String,
    pub failure_reason: String,
    pub created_at: String,
}

/// Maximum characters stored per `compressed_context` or `failure_reason` field.
const MAX_FIELD_CHARS: usize = 4096;

fn truncate_field(s: &str) -> &str {
    let mut idx = MAX_FIELD_CHARS;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx.min(s.len())]
}

impl SqliteStore {
    /// Load the latest active compression guidelines.
    ///
    /// When `conversation_id` is `Some`, returns conversation-specific guidelines
    /// preferred over global (NULL) ones. When `None`, returns only global guidelines.
    ///
    /// Returns `(version, guidelines_text)`. Returns `(0, "")` if no guidelines exist yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn load_compression_guidelines(
        &self,
        conversation_id: Option<ConversationId>,
    ) -> Result<(i64, String), MemoryError> {
        let row = sqlx::query_as::<_, (i64, String)>(
            // When conversation_id is Some(cid): `conversation_id = cid` matches
            // conversation-specific rows; `conversation_id IS NULL` matches global rows.
            // The CASE ensures conversation-specific rows sort before global ones.
            // When conversation_id is None: `conversation_id = NULL` is always false in SQL,
            // so only `conversation_id IS NULL` rows match — correct global-only behavior.
            "SELECT version, guidelines FROM compression_guidelines \
             WHERE conversation_id = ? OR conversation_id IS NULL \
             ORDER BY CASE WHEN conversation_id IS NOT NULL THEN 0 ELSE 1 END, \
                      version DESC \
             LIMIT 1",
        )
        .bind(conversation_id.map(|c| c.0))
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.unwrap_or((0, String::new())))
    }

    /// Load only the version and creation timestamp of the latest active compression guidelines.
    ///
    /// Same scoping rules as [`load_compression_guidelines`]: conversation-specific rows are
    /// preferred over global ones.  Returns `(0, "")` if no guidelines exist yet.
    ///
    /// Use this in hot paths where the full text is not needed (e.g. metrics sync).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn load_compression_guidelines_meta(
        &self,
        conversation_id: Option<ConversationId>,
    ) -> Result<(i64, String), MemoryError> {
        let row = sqlx::query_as::<_, (i64, String)>(
            "SELECT version, created_at FROM compression_guidelines \
             WHERE conversation_id = ? OR conversation_id IS NULL \
             ORDER BY CASE WHEN conversation_id IS NOT NULL THEN 0 ELSE 1 END, \
                      version DESC \
             LIMIT 1",
        )
        .bind(conversation_id.map(|c| c.0))
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.unwrap_or((0, String::new())))
    }

    /// Save a new version of the compression guidelines.
    ///
    /// When `conversation_id` is `Some`, the guidelines are scoped to that conversation.
    /// When `None`, the guidelines are global (apply as fallback for all conversations).
    ///
    /// Inserts a new row; older versions are retained for audit.
    /// Returns the new version number.
    ///
    /// Note: version numbers are globally sequential across all conversation scopes —
    /// they are not per-conversation counters. The UNIQUE(version) constraint from
    /// migration 033 is preserved.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails (including FK violation if
    /// `conversation_id` does not reference a valid conversation row).
    pub async fn save_compression_guidelines(
        &self,
        guidelines: &str,
        token_count: i64,
        conversation_id: Option<ConversationId>,
    ) -> Result<i64, MemoryError> {
        // The INSERT...SELECT computes MAX(version)+1 across all rows (global + per-conversation)
        // and inserts it in a single statement. SQLite's single-writer WAL guarantee makes this
        // atomic — no concurrent writer can observe the same MAX and produce a duplicate version.
        let new_version: i64 = sqlx::query_scalar(
            "INSERT INTO compression_guidelines (version, guidelines, token_count, conversation_id) \
             SELECT COALESCE(MAX(version), 0) + 1, ?, ?, ? \
             FROM compression_guidelines \
             RETURNING version",
        )
        .bind(guidelines)
        .bind(token_count)
        .bind(conversation_id.map(|c| c.0))
        .fetch_one(&self.pool)
        .await?;
        Ok(new_version)
    }

    /// Log a compression failure pair.
    ///
    /// Both `compressed_context` and `failure_reason` are truncated to 4096 chars.
    /// Returns the inserted row id.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn log_compression_failure(
        &self,
        conversation_id: ConversationId,
        compressed_context: &str,
        failure_reason: &str,
    ) -> Result<i64, MemoryError> {
        let ctx = redact_sensitive(compressed_context);
        let ctx = truncate_field(&ctx);
        let reason = redact_sensitive(failure_reason);
        let reason = truncate_field(&reason);
        let id = sqlx::query_scalar(
            "INSERT INTO compression_failure_pairs \
             (conversation_id, compressed_context, failure_reason) \
             VALUES (?, ?, ?) RETURNING id",
        )
        .bind(conversation_id.0)
        .bind(ctx)
        .bind(reason)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Get unused failure pairs (oldest first), up to `limit`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_unused_failure_pairs(
        &self,
        limit: usize,
    ) -> Result<Vec<CompressionFailurePair>, MemoryError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = sqlx::query_as::<_, (i64, i64, String, String, String)>(
            "SELECT id, conversation_id, compressed_context, failure_reason, created_at \
             FROM compression_failure_pairs \
             WHERE used_in_update = 0 \
             ORDER BY created_at ASC \
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, cid, ctx, reason, created_at)| CompressionFailurePair {
                    id,
                    conversation_id: ConversationId(cid),
                    compressed_context: ctx,
                    failure_reason: reason,
                    created_at,
                },
            )
            .collect())
    }

    /// Mark failure pairs as consumed by the updater.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub async fn mark_failure_pairs_used(&self, ids: &[i64]) -> Result<(), MemoryError> {
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "UPDATE compression_failure_pairs SET used_in_update = 1 WHERE id IN ({placeholders})"
        );
        let mut q = sqlx::query(&query);
        for id in ids {
            q = q.bind(id);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    /// Count unused failure pairs.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn count_unused_failure_pairs(&self) -> Result<i64, MemoryError> {
        let count = sqlx::query_scalar(
            "SELECT COUNT(*) FROM compression_failure_pairs WHERE used_in_update = 0",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Delete old used failure pairs, keeping the most recent `keep_recent` unused pairs.
    ///
    /// Removes all rows where `used_in_update = 1`. Unused rows are managed by the
    /// `max_stored_pairs` enforcement below: if there are more than `keep_recent` unused pairs,
    /// the oldest excess rows are deleted.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn cleanup_old_failure_pairs(&self, keep_recent: usize) -> Result<(), MemoryError> {
        // Delete all used pairs (they've already been processed).
        sqlx::query("DELETE FROM compression_failure_pairs WHERE used_in_update = 1")
            .execute(&self.pool)
            .await?;

        // Keep only the most recent `keep_recent` unused pairs.
        let keep = i64::try_from(keep_recent).unwrap_or(i64::MAX);
        sqlx::query(
            "DELETE FROM compression_failure_pairs \
             WHERE used_in_update = 0 \
             AND id NOT IN ( \
                 SELECT id FROM compression_failure_pairs \
                 WHERE used_in_update = 0 \
                 ORDER BY created_at DESC \
                 LIMIT ? \
             )",
        )
        .bind(keep)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // pool_size=1 is required: SQLite :memory: creates an isolated database per
    // connection, so multiple connections would each see an empty schema.
    async fn make_store() -> SqliteStore {
        SqliteStore::with_pool_size(":memory:", 1)
            .await
            .expect("in-memory SqliteStore")
    }

    #[tokio::test]
    async fn load_guidelines_meta_returns_defaults_when_empty() {
        let store = make_store().await;
        let (version, created_at) = store.load_compression_guidelines_meta(None).await.unwrap();
        assert_eq!(version, 0);
        assert!(created_at.is_empty());
    }

    #[tokio::test]
    async fn load_guidelines_meta_returns_version_and_created_at() {
        let store = make_store().await;
        store
            .save_compression_guidelines("keep file paths", 4, None)
            .await
            .unwrap();
        let (version, created_at) = store.load_compression_guidelines_meta(None).await.unwrap();
        assert_eq!(version, 1);
        assert!(!created_at.is_empty(), "created_at should be populated");
    }

    #[tokio::test]
    async fn load_guidelines_returns_defaults_when_empty() {
        let store = make_store().await;
        let (version, text) = store.load_compression_guidelines(None).await.unwrap();
        assert_eq!(version, 0);
        assert!(text.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_guidelines() {
        let store = make_store().await;
        let v1 = store
            .save_compression_guidelines("always preserve file paths", 4, None)
            .await
            .unwrap();
        assert_eq!(v1, 1);
        let v2 = store
            .save_compression_guidelines(
                "always preserve file paths\nalways preserve errors",
                8,
                None,
            )
            .await
            .unwrap();
        assert_eq!(v2, 2);
        // Loading should return the latest version.
        let (v, text) = store.load_compression_guidelines(None).await.unwrap();
        assert_eq!(v, 2);
        assert!(text.contains("errors"));
    }

    #[tokio::test]
    async fn load_guidelines_prefers_conversation_specific() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .save_compression_guidelines("global rule", 2, None)
            .await
            .unwrap();
        store
            .save_compression_guidelines("conversation rule", 2, Some(cid))
            .await
            .unwrap();
        let (_, text) = store.load_compression_guidelines(Some(cid)).await.unwrap();
        assert_eq!(text, "conversation rule");
    }

    #[tokio::test]
    async fn load_guidelines_falls_back_to_global() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .save_compression_guidelines("global rule", 2, None)
            .await
            .unwrap();
        // No conversation-specific guidelines; should fall back to global.
        let (_, text) = store.load_compression_guidelines(Some(cid)).await.unwrap();
        assert_eq!(text, "global rule");
    }

    #[tokio::test]
    async fn load_guidelines_none_returns_global_only() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .save_compression_guidelines("conversation rule", 2, Some(cid))
            .await
            .unwrap();
        // None should not return conversation-scoped guidelines.
        let (version, text) = store.load_compression_guidelines(None).await.unwrap();
        assert_eq!(version, 0);
        assert!(text.is_empty());
    }

    #[tokio::test]
    async fn load_guidelines_scope_isolation() {
        let store = make_store().await;
        let cid_a = ConversationId(store.create_conversation().await.unwrap().0);
        let cid_b = ConversationId(store.create_conversation().await.unwrap().0);

        // Global guideline (conversation_id = None) — visible to all conversations.
        store
            .save_compression_guidelines("Use bullet points", 1, None)
            .await
            .unwrap();
        // Conversation-A-specific guideline — must NOT be visible to B.
        store
            .save_compression_guidelines("Be concise", 2, Some(cid_a))
            .await
            .unwrap();

        // Conversation B: gets only the global guideline, not A's.
        let (_, text_b) = store
            .load_compression_guidelines(Some(cid_b))
            .await
            .unwrap();
        assert_eq!(
            text_b, "Use bullet points",
            "conversation B must see global guideline"
        );

        // Conversation A: gets its own guideline (preferred over global).
        let (_, text_a) = store
            .load_compression_guidelines(Some(cid_a))
            .await
            .unwrap();
        assert_eq!(
            text_a, "Be concise",
            "conversation A must prefer its own guideline over global"
        );

        // None scope: gets only the global guideline.
        let (_, text_global) = store.load_compression_guidelines(None).await.unwrap();
        assert_eq!(
            text_global, "Use bullet points",
            "None scope must see only the global guideline"
        );
    }

    #[tokio::test]
    async fn save_with_nonexistent_conversation_id_fails() {
        let store = make_store().await;
        let nonexistent = ConversationId(99999);
        let result = store
            .save_compression_guidelines("rule", 1, Some(nonexistent))
            .await;
        assert!(
            result.is_err(),
            "FK violation expected for nonexistent conversation_id"
        );
    }

    #[tokio::test]
    async fn cascade_delete_removes_conversation_guidelines() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .save_compression_guidelines("rule", 1, Some(cid))
            .await
            .unwrap();
        // Delete the conversation row directly — should cascade-delete the guideline.
        sqlx::query("DELETE FROM conversations WHERE id = ?")
            .bind(cid.0)
            .execute(store.pool())
            .await
            .unwrap();
        let (version, text) = store.load_compression_guidelines(Some(cid)).await.unwrap();
        assert_eq!(version, 0);
        assert!(text.is_empty());
    }

    #[tokio::test]
    async fn log_and_count_failure_pairs() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .log_compression_failure(cid, "compressed ctx", "i don't recall that")
            .await
            .unwrap();
        let count = store.count_unused_failure_pairs().await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn get_unused_pairs_sorted_oldest_first() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .log_compression_failure(cid, "ctx A", "reason A")
            .await
            .unwrap();
        store
            .log_compression_failure(cid, "ctx B", "reason B")
            .await
            .unwrap();
        let pairs = store.get_unused_failure_pairs(10).await.unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].compressed_context, "ctx A");
    }

    #[tokio::test]
    async fn mark_pairs_used_reduces_count() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        let id = store
            .log_compression_failure(cid, "ctx", "reason")
            .await
            .unwrap();
        store.mark_failure_pairs_used(&[id]).await.unwrap();
        let count = store.count_unused_failure_pairs().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn cleanup_deletes_used_and_trims_unused() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        // Add 3 pairs and mark 1 used.
        let id1 = store
            .log_compression_failure(cid, "ctx1", "r1")
            .await
            .unwrap();
        store
            .log_compression_failure(cid, "ctx2", "r2")
            .await
            .unwrap();
        store
            .log_compression_failure(cid, "ctx3", "r3")
            .await
            .unwrap();
        store.mark_failure_pairs_used(&[id1]).await.unwrap();
        // Cleanup: keep at most 1 unused.
        store.cleanup_old_failure_pairs(1).await.unwrap();
        let count = store.count_unused_failure_pairs().await.unwrap();
        assert_eq!(count, 1, "only 1 unused pair should remain");
    }

    #[test]
    fn redact_sensitive_api_key_is_redacted() {
        let result = redact_sensitive("token sk-abc123def456 used for auth");
        assert!(result.contains("[REDACTED]"), "API key must be redacted");
        assert!(
            !result.contains("sk-abc123"),
            "original key must not appear"
        );
    }

    #[test]
    fn redact_sensitive_plain_text_borrows() {
        let text = "safe text, no secrets here";
        let result = redact_sensitive(text);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "plain text must return Cow::Borrowed (zero-alloc)"
        );
    }

    #[test]
    fn redact_sensitive_filesystem_path_is_redacted() {
        let result = redact_sensitive("config loaded from /Users/dev/project/config.toml");
        assert!(
            result.contains("[PATH]"),
            "filesystem path must be redacted"
        );
        assert!(
            !result.contains("/Users/dev/"),
            "original path must not appear"
        );
    }

    #[test]
    fn redact_sensitive_combined_secret_and_path() {
        let result = redact_sensitive("key sk-abc at /home/user/file");
        assert!(result.contains("[REDACTED]"), "secret must be redacted");
        assert!(result.contains("[PATH]"), "path must be redacted");
    }

    #[tokio::test]
    async fn log_compression_failure_redacts_secrets() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .log_compression_failure(cid, "token sk-abc123def456 used for auth", "context lost")
            .await
            .unwrap();
        let pairs = store.get_unused_failure_pairs(10).await.unwrap();
        assert_eq!(pairs.len(), 1);
        assert!(
            pairs[0].compressed_context.contains("[REDACTED]"),
            "stored context must have redacted secret"
        );
        assert!(
            !pairs[0].compressed_context.contains("sk-abc123"),
            "stored context must not contain raw secret"
        );
    }

    #[tokio::test]
    async fn log_compression_failure_redacts_paths() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .log_compression_failure(cid, "/Users/dev/project/config.toml was loaded", "lost")
            .await
            .unwrap();
        let pairs = store.get_unused_failure_pairs(10).await.unwrap();
        assert!(
            pairs[0].compressed_context.contains("[PATH]"),
            "stored context must have redacted path"
        );
        assert!(
            !pairs[0].compressed_context.contains("/Users/dev/"),
            "stored context must not contain raw path"
        );
    }

    #[tokio::test]
    async fn log_compression_failure_reason_also_redacted() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .log_compression_failure(cid, "some context", "secret ghp_abc123xyz was leaked")
            .await
            .unwrap();
        let pairs = store.get_unused_failure_pairs(10).await.unwrap();
        assert!(
            pairs[0].failure_reason.contains("[REDACTED]"),
            "failure_reason must also be redacted"
        );
        assert!(
            !pairs[0].failure_reason.contains("ghp_abc123xyz"),
            "raw secret must not appear in failure_reason"
        );
    }

    #[tokio::test]
    async fn truncate_field_respects_char_boundary() {
        let s = "а".repeat(5000); // Cyrillic 'а', 2 bytes each
        let truncated = truncate_field(&s);
        assert!(truncated.len() <= MAX_FIELD_CHARS);
        assert!(s.is_char_boundary(truncated.len()));
    }

    #[tokio::test]
    async fn unique_constraint_prevents_duplicate_version() {
        let store = make_store().await;
        // Insert version 1 via the public API.
        store
            .save_compression_guidelines("first", 1, None)
            .await
            .unwrap();
        // store.pool() access is intentional: we need direct pool access to bypass
        // the public API and test the UNIQUE constraint at the SQL level.
        let result = sqlx::query(
            "INSERT INTO compression_guidelines (version, guidelines, token_count) VALUES (1, 'dup', 0)",
        )
        .execute(store.pool())
        .await;
        assert!(
            result.is_err(),
            "duplicate version insert should violate UNIQUE constraint"
        );
    }

    #[test]
    fn redact_sensitive_bearer_token_is_redacted() {
        let result =
            redact_sensitive("Authorization: Bearer eyJhbGciOiJSUzI1NiJ9.payload.signature");
        assert!(
            result.contains("[REDACTED]"),
            "Bearer token must be redacted: {result}"
        );
        assert!(
            !result.contains("eyJhbGciOiJSUzI1NiJ9"),
            "raw JWT header must not appear: {result}"
        );
        assert!(
            result.contains("Authorization:"),
            "header name must be preserved: {result}"
        );
    }

    #[test]
    fn redact_sensitive_bearer_token_case_insensitive() {
        let result =
            redact_sensitive("authorization: bearer eyJhbGciOiJSUzI1NiJ9.payload.signature");
        assert!(
            result.contains("[REDACTED]"),
            "Bearer header match must be case-insensitive: {result}"
        );
    }

    #[test]
    fn redact_sensitive_standalone_jwt_is_redacted() {
        let jwt = "eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyMTIzIn0.SflKxwRJSMeKKF2";
        let input = format!("token value: {jwt} was found in logs");
        let result = redact_sensitive(&input);
        assert!(
            result.contains("[REDACTED_JWT]"),
            "standalone JWT must be replaced with [REDACTED_JWT]: {result}"
        );
        assert!(
            !result.contains("eyJhbGci"),
            "raw JWT must not appear: {result}"
        );
    }

    #[test]
    fn redact_sensitive_mixed_content_all_redacted() {
        let input =
            "key sk-abc123 at /home/user/f with Authorization: Bearer eyJhbG.pay.sig and eyJx.b.c";
        let result = redact_sensitive(input);
        assert!(result.contains("[REDACTED]"), "API key must be redacted");
        assert!(result.contains("[PATH]"), "path must be redacted");
        assert!(!result.contains("sk-abc123"), "raw API key must not appear");
        assert!(!result.contains("eyJhbG"), "raw JWT must not appear");
    }

    #[test]
    fn redact_sensitive_partial_jwt_not_redacted() {
        // A string starting with eyJ but missing the third segment is not a valid JWT.
        let input = "eyJhbGciOiJSUzI1NiJ9.onlytwoparts";
        let result = redact_sensitive(input);
        // Should not be replaced by the JWT regex (only two dot-separated parts).
        assert!(
            !result.contains("[REDACTED_JWT]"),
            "two-part eyJ string must not be treated as JWT: {result}"
        );
        // No substitution occurred — must be zero-alloc Cow::Borrowed.
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "no-match input must return Cow::Borrowed: {result}"
        );
    }

    #[test]
    fn redact_sensitive_alg_none_jwt_empty_signature_redacted() {
        // alg=none JWTs have an empty third segment: <header>.<payload>.
        let input =
            "token: eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1c2VyIn0. was submitted without signature";
        let result = redact_sensitive(input);
        assert!(
            result.contains("[REDACTED_JWT]"),
            "alg=none JWT with empty signature must be redacted: {result}"
        );
        assert!(
            !result.contains("eyJhbGciOiJub25lIn0"),
            "raw alg=none JWT header must not appear: {result}"
        );
    }

    /// Concurrent saves must produce strictly unique versions with no collisions.
    ///
    /// Uses a file-backed database because `SQLite` `:memory:` creates an isolated
    /// database per connection — a multi-connection pool over `:memory:` would give
    /// each writer its own empty schema and cannot test shared-state atomicity.
    #[tokio::test]
    async fn concurrent_saves_produce_unique_versions() {
        use std::collections::HashSet;
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("test.db");
        let store = Arc::new(
            SqliteStore::with_pool_size(db_path.to_str().expect("utf8 path"), 4)
                .await
                .expect("file-backed SqliteStore"),
        );

        let tasks: Vec<_> = (0..8_i64)
            .map(|i| {
                let s = Arc::clone(&store);
                tokio::spawn(async move {
                    s.save_compression_guidelines(&format!("guideline {i}"), i, None)
                        .await
                        .expect("concurrent save must succeed")
                })
            })
            .collect();

        let mut versions = HashSet::new();
        for task in tasks {
            let v = task.await.expect("task must not panic");
            assert!(versions.insert(v), "version {v} appeared more than once");
        }
        assert_eq!(
            versions.len(),
            8,
            "all 8 saves must produce distinct versions"
        );
    }
}
