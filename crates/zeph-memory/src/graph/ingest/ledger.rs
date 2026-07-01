// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Content-hash idempotency ledger for `zeph knowledge ingest`.
//!
//! The ledger records every `(source_uri, content_hash)` pair that has been successfully
//! ingested, so unchanged inputs are skipped on subsequent runs (spec-067 FR-012, INV-5).
//!
//! # Scope and limitations
//!
//! This is a **re-read / cost guard only** — it prevents redundant embedding and LLM extraction
//! for inputs whose content has not changed since the last run. It does **not**:
//!
//! - Reconcile LLM extraction drift across model versions. If the model changes, re-ingest must
//!   be forced by clearing ledger rows for the affected sources.
//! - Delete stale Qdrant points when a file's content changes. When `content_hash` changes for
//!   the same path, the old vectors remain in Qdrant (orphaned). Stale-point cleanup is a Phase-2
//!   concern and is documented as a known MVP limitation in the testing playbook.
//!
//! # Database
//!
//! The ledger shares the agent's existing `SQLite` pool (`SemanticMemory::sqlite().pool()`). It is
//! operator-triggered, one row per ingested file, and is not on the hot path — no dedicated file
//! is needed (mirrors the `skill_trace_sessions` pattern).
//!
//! # Examples
//!
//! ```rust,no_run
//! # async fn example() -> Result<(), zeph_memory::MemoryError> {
//! use zeph_memory::graph::ingest::IngestLedger;
//!
//! // In practice, pool comes from SemanticMemory::sqlite().pool().clone().
//! # let pool: zeph_db::DbPool = unimplemented!();
//! let ledger = IngestLedger::new(pool);
//! let hash = IngestLedger::content_hash(b"hello world");
//! let uri = "specs/README.md@abc123";
//! let batch = "01J0000000000000000000000";
//!
//! ledger.mark_ingested(uri, &hash, batch, 0, 0).await?;
//! assert!(ledger.is_ingested(uri, &hash).await?);
//! # Ok(())
//! # }
//! ```

use zeph_db::{DbPool, query, query_as, query_scalar, sql};

use crate::MemoryError;

/// Outcome of resolving a (possibly abbreviated) batch id prefix against the ledger.
///
/// Returned by [`IngestLedger::resolve_batch_id`] to support git-style unambiguous prefix
/// matching for `zeph knowledge rollback --batch-id`, since `zeph knowledge status` only
/// prints an 8-character prefix of each `import_batch_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchIdResolution {
    /// Exactly one batch id matches the supplied prefix (or matches it exactly).
    Resolved(String),
    /// More than one batch id shares the supplied prefix; lists every match.
    Ambiguous(Vec<String>),
    /// No batch id in the ledger starts with the supplied prefix.
    NotFound,
}

/// A single row from the `knowledge_ingest_ledger` table.
///
/// Returned by [`IngestLedger::summary`] to back `zeph knowledge status`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LedgerEntry {
    /// Repository-relative path qualified by revision, e.g. `specs/README.md@abc123`.
    pub source_uri: String,
    /// BLAKE3 hex digest of the raw file bytes at ingest time.
    pub content_hash: String,
    /// Opaque per-run identifier (`UUIDv4` string) grouping all files ingested in one invocation.
    pub import_batch_id: String,
    /// ISO-8601 timestamp stored as TEXT in both `SQLite` and `PostgreSQL` dialects.
    pub ingested_at: String,
    /// Number of graph entities extracted (0 for the notes sink; non-zero after Phase 2).
    pub entities: i64,
    /// Number of graph edges extracted (0 for the notes sink; non-zero after Phase 2).
    pub edges: i64,
}

/// Content-hash idempotency ledger for `zeph knowledge ingest`.
///
/// Records `(source_uri, content_hash)` pairs of inputs that have been successfully embedded so
/// that unchanged files are skipped on the next run (spec-067 INV-5).
///
/// This is a **re-read / cost guard only** — it does NOT reconcile LLM extraction drift across
/// model versions (INV-5). An unchanged `(source_uri, content_hash)` pair skips re-embedding; a
/// changed hash for the same URI is treated as a new input and produces a new ledger row while
/// leaving any previous Qdrant chunks in place (stale-point cleanup is Phase 2).
#[derive(Clone)]
pub struct IngestLedger {
    pool: DbPool,
}

impl IngestLedger {
    /// Creates a new `IngestLedger` wrapping the given database pool.
    ///
    /// The pool should come from `SemanticMemory::sqlite().pool().clone()` so the ledger shares
    /// the agent's existing connection pool rather than opening a new file.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Computes the BLAKE3 hex digest of `bytes`.
    ///
    /// This is the canonical content-hash format used as the `content_hash` column value.
    /// Callers must use this function (rather than a different hash algorithm or encoding) to
    /// ensure ledger keys are consistent across invocations.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_memory::graph::ingest::IngestLedger;
    ///
    /// let hash = IngestLedger::content_hash(b"hello world");
    /// assert_eq!(hash.len(), 64, "BLAKE3 hex digest is always 64 characters");
    /// ```
    #[must_use]
    pub fn content_hash(bytes: &[u8]) -> String {
        blake3::Hasher::new()
            .update(bytes)
            .finalize()
            .to_hex()
            .to_string()
    }

    /// Returns `true` if the `(source_uri, content_hash)` pair is already recorded in the ledger.
    ///
    /// When this returns `true`, the caller should skip re-embedding the input — its content has
    /// not changed since it was last ingested (spec-067 FR-012).
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Sqlx`] on database failure.
    pub async fn is_ingested(
        &self,
        source_uri: &str,
        content_hash: &str,
    ) -> Result<bool, MemoryError> {
        let exists: Option<i64> = query_scalar(sql!(
            "SELECT 1 FROM knowledge_ingest_ledger \
             WHERE source_uri = ? AND content_hash = ?"
        ))
        .bind(source_uri)
        .bind(content_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(exists.is_some())
    }

    /// Records a successful ingest of `(source_uri, content_hash)`.
    ///
    /// Idempotent: if the pair already exists the row is updated in place with the new
    /// `batch_id`, `ingested_at`, `entities`, and `edges` values
    /// (`ON CONFLICT … DO UPDATE`).
    ///
    /// `entities` and `edges` should be `0` for the notes sink (Phase 1). Non-zero values are
    /// reserved for Phase 2 graph extraction.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Sqlx`] on database failure.
    pub async fn mark_ingested(
        &self,
        source_uri: &str,
        content_hash: &str,
        batch_id: &str,
        entities: i64,
        edges: i64,
    ) -> Result<(), MemoryError> {
        let now = chrono::Utc::now().to_rfc3339();
        query(sql!(
            "INSERT INTO knowledge_ingest_ledger \
             (source_uri, content_hash, import_batch_id, ingested_at, entities, edges) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(source_uri, content_hash) DO UPDATE SET \
             import_batch_id = excluded.import_batch_id, \
             ingested_at = excluded.ingested_at, \
             entities = excluded.entities, \
             edges = excluded.edges"
        ))
        .bind(source_uri)
        .bind(content_hash)
        .bind(batch_id)
        .bind(&now)
        .bind(entities)
        .bind(edges)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Returns `true` if any row in the ledger has the given `import_batch_id`.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Sqlx`] on database failure.
    pub async fn batch_exists(&self, batch_id: &str) -> Result<bool, MemoryError> {
        let exists: Option<i64> = query_scalar(sql!(
            "SELECT 1 FROM knowledge_ingest_ledger WHERE import_batch_id = ?"
        ))
        .bind(batch_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(exists.is_some())
    }

    /// Resolves a (possibly abbreviated) `batch_id` prefix to a full `import_batch_id`.
    ///
    /// Mirrors git's unambiguous short-hash resolution: an exact match on a full id wins
    /// immediately; otherwise a prefix that uniquely identifies one batch resolves to it, a
    /// prefix shared by several batches is reported as [`BatchIdResolution::Ambiguous`], and a
    /// prefix matching no batch is reported as [`BatchIdResolution::NotFound`]. This lets
    /// `zeph knowledge rollback --batch-id` accept the 8-character prefix printed by
    /// `zeph knowledge status` (#5399).
    ///
    /// An empty (or whitespace-only) `prefix` always resolves to [`BatchIdResolution::NotFound`],
    /// even when the ledger is non-empty — every id trivially `starts_with("")`, so without this
    /// guard a blank prefix (e.g. an unset `--batch-id "$VAR"` shell substitution) would silently
    /// resolve to the sole batch in a single-batch ledger instead of being rejected.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Sqlx`] on database failure.
    pub async fn resolve_batch_id(&self, prefix: &str) -> Result<BatchIdResolution, MemoryError> {
        if prefix.trim().is_empty() {
            return Ok(BatchIdResolution::NotFound);
        }

        let ids: Vec<String> = query_scalar(sql!(
            "SELECT DISTINCT import_batch_id FROM knowledge_ingest_ledger"
        ))
        .fetch_all(&self.pool)
        .await?;

        if ids.iter().any(|id| id == prefix) {
            return Ok(BatchIdResolution::Resolved(prefix.to_owned()));
        }

        let mut matches: Vec<String> = ids
            .into_iter()
            .filter(|id| id.starts_with(prefix))
            .collect();
        match matches.len() {
            0 => Ok(BatchIdResolution::NotFound),
            1 => Ok(BatchIdResolution::Resolved(matches.remove(0))),
            _ => Ok(BatchIdResolution::Ambiguous(matches)),
        }
    }

    /// Deletes all ledger rows for the given `import_batch_id`.
    ///
    /// Returns the number of rows removed.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Sqlx`] on database failure.
    pub async fn delete_batch(&self, batch_id: &str) -> Result<u64, MemoryError> {
        let result = query(sql!(
            "DELETE FROM knowledge_ingest_ledger WHERE import_batch_id = ?"
        ))
        .bind(batch_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Deletes all ledger rows for `batch_id` within a caller-provided transaction.
    ///
    /// Executes the same DELETE as [`Self::delete_batch`] but uses `tx` so the caller can
    /// combine it with other writes in a single atomic unit.
    /// Returns the number of rows removed.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Sqlx`] on database failure.
    pub async fn delete_batch_in_tx(
        &self,
        batch_id: &str,
        tx: &mut zeph_db::DbTransaction<'_>,
    ) -> Result<u64, MemoryError> {
        let result = query(sql!(
            "DELETE FROM knowledge_ingest_ledger WHERE import_batch_id = ?"
        ))
        .bind(batch_id)
        .execute(&mut **tx)
        .await?;
        Ok(result.rows_affected())
    }

    /// Returns all ledger rows ordered by `ingested_at` descending, newest first.
    ///
    /// This is the data source for `zeph knowledge status`.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Sqlx`] on database failure.
    pub async fn summary(&self) -> Result<Vec<LedgerEntry>, MemoryError> {
        let rows: Vec<LedgerEntry> = query_as(sql!(
            "SELECT source_uri, content_hash, import_batch_id, ingested_at, entities, edges \
             FROM knowledge_ingest_ledger \
             ORDER BY ingested_at DESC"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

// Hardcodes `sqlx::SqlitePool` and is not portable to PostgreSQL; skipped entirely
// when `postgres` is the active backend (see issue #5364).
#[cfg(all(test, not(feature = "postgres")))]
mod tests {
    use super::*;

    async fn test_ledger() -> IngestLedger {
        let pool = sqlx::SqlitePool::connect(":memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::query(
            "CREATE TABLE knowledge_ingest_ledger (\
             source_uri TEXT NOT NULL, \
             content_hash TEXT NOT NULL, \
             import_batch_id TEXT NOT NULL, \
             ingested_at TEXT NOT NULL DEFAULT (datetime('now')), \
             entities INTEGER NOT NULL DEFAULT 0, \
             edges INTEGER NOT NULL DEFAULT 0, \
             PRIMARY KEY (source_uri, content_hash)\
             )",
        )
        .execute(&pool)
        .await
        .expect("create table");
        IngestLedger::new(pool)
    }

    #[test]
    fn content_hash_is_hex64() {
        let h = IngestLedger::content_hash(b"hello");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn content_hash_deterministic() {
        assert_eq!(
            IngestLedger::content_hash(b"zeph"),
            IngestLedger::content_hash(b"zeph"),
        );
    }

    #[test]
    fn content_hash_differs_for_different_inputs() {
        assert_ne!(
            IngestLedger::content_hash(b"foo"),
            IngestLedger::content_hash(b"bar"),
        );
    }

    #[tokio::test]
    async fn is_ingested_false_when_absent() {
        let ledger = test_ledger().await;
        assert!(!ledger.is_ingested("uri", "hash").await.unwrap());
    }

    #[tokio::test]
    async fn mark_then_is_ingested_round_trip() {
        let ledger = test_ledger().await;
        let uri = "specs/README.md@abc123";
        let hash = IngestLedger::content_hash(b"content");
        ledger
            .mark_ingested(uri, &hash, "batch-1", 0, 0)
            .await
            .unwrap();
        assert!(ledger.is_ingested(uri, &hash).await.unwrap());
    }

    #[tokio::test]
    async fn different_hash_not_ingested() {
        let ledger = test_ledger().await;
        let uri = "specs/README.md@abc123";
        let hash1 = IngestLedger::content_hash(b"v1");
        let hash2 = IngestLedger::content_hash(b"v2");
        ledger
            .mark_ingested(uri, &hash1, "batch-1", 0, 0)
            .await
            .unwrap();
        assert!(!ledger.is_ingested(uri, &hash2).await.unwrap());
    }

    #[tokio::test]
    async fn mark_ingested_idempotent() {
        let ledger = test_ledger().await;
        let uri = "specs/README.md@abc123";
        let hash = IngestLedger::content_hash(b"content");
        ledger
            .mark_ingested(uri, &hash, "batch-1", 0, 0)
            .await
            .unwrap();
        ledger
            .mark_ingested(uri, &hash, "batch-2", 5, 3)
            .await
            .unwrap();
        let rows = ledger.summary().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].import_batch_id, "batch-2");
        assert_eq!(rows[0].entities, 5);
        assert_eq!(rows[0].edges, 3);
    }

    #[tokio::test]
    async fn summary_returns_all_rows() {
        let ledger = test_ledger().await;
        ledger.mark_ingested("a", "h1", "b1", 0, 0).await.unwrap();
        ledger.mark_ingested("b", "h2", "b1", 0, 0).await.unwrap();
        let rows = ledger.summary().await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn summary_empty_when_no_rows() {
        let ledger = test_ledger().await;
        assert!(ledger.summary().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn batch_exists_false_when_no_rows() {
        let ledger = test_ledger().await;
        assert!(!ledger.batch_exists("nonexistent-batch").await.unwrap());
    }

    #[tokio::test]
    async fn batch_exists_true_after_mark_ingested() {
        let ledger = test_ledger().await;
        ledger
            .mark_ingested("uri", "hash", "batch-42", 0, 0)
            .await
            .unwrap();
        assert!(ledger.batch_exists("batch-42").await.unwrap());
        assert!(!ledger.batch_exists("batch-99").await.unwrap());
    }

    #[tokio::test]
    async fn delete_batch_removes_matching_rows_only() {
        let ledger = test_ledger().await;
        ledger
            .mark_ingested("a", "h1", "batch-A", 0, 0)
            .await
            .unwrap();
        ledger
            .mark_ingested("b", "h2", "batch-A", 0, 0)
            .await
            .unwrap();
        ledger
            .mark_ingested("c", "h3", "batch-B", 0, 0)
            .await
            .unwrap();

        let removed = ledger.delete_batch("batch-A").await.unwrap();
        assert_eq!(removed, 2);

        assert!(!ledger.batch_exists("batch-A").await.unwrap());
        assert!(ledger.batch_exists("batch-B").await.unwrap());
    }

    #[tokio::test]
    async fn delete_batch_returns_zero_for_missing_batch() {
        let ledger = test_ledger().await;
        let removed = ledger.delete_batch("ghost-batch").await.unwrap();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn resolve_batch_id_not_found_when_no_match() {
        let ledger = test_ledger().await;
        ledger
            .mark_ingested("a", "h1", "01234567-aaaa", 0, 0)
            .await
            .unwrap();
        assert_eq!(
            ledger.resolve_batch_id("ghost").await.unwrap(),
            BatchIdResolution::NotFound
        );
    }

    #[tokio::test]
    async fn resolve_batch_id_empty_prefix_is_not_found_even_with_a_single_batch() {
        let ledger = test_ledger().await;
        ledger
            .mark_ingested("a", "h1", "01234567-aaaa", 0, 0)
            .await
            .unwrap();
        assert_eq!(
            ledger.resolve_batch_id("").await.unwrap(),
            BatchIdResolution::NotFound
        );
        assert_eq!(
            ledger.resolve_batch_id("   ").await.unwrap(),
            BatchIdResolution::NotFound
        );
    }

    #[tokio::test]
    async fn resolve_batch_id_exact_match_wins_even_if_also_a_prefix() {
        let ledger = test_ledger().await;
        ledger.mark_ingested("a", "h1", "0123", 0, 0).await.unwrap();
        ledger
            .mark_ingested("b", "h2", "01234567", 0, 0)
            .await
            .unwrap();
        assert_eq!(
            ledger.resolve_batch_id("0123").await.unwrap(),
            BatchIdResolution::Resolved("0123".to_owned())
        );
    }

    #[tokio::test]
    async fn resolve_batch_id_unique_prefix_resolves_to_full_id() {
        let ledger = test_ledger().await;
        ledger
            .mark_ingested("a", "h1", "01234567-aaaa-bbbb-cccc", 0, 0)
            .await
            .unwrap();
        ledger
            .mark_ingested("b", "h2", "89abcdef-aaaa-bbbb-cccc", 0, 0)
            .await
            .unwrap();
        assert_eq!(
            ledger.resolve_batch_id("012345").await.unwrap(),
            BatchIdResolution::Resolved("01234567-aaaa-bbbb-cccc".to_owned())
        );
    }

    #[tokio::test]
    async fn resolve_batch_id_ambiguous_prefix_lists_candidates() {
        let ledger = test_ledger().await;
        ledger
            .mark_ingested("a", "h1", "01234567-aaaa", 0, 0)
            .await
            .unwrap();
        ledger
            .mark_ingested("b", "h2", "01234567-bbbb", 0, 0)
            .await
            .unwrap();
        let resolution = ledger.resolve_batch_id("01234567").await.unwrap();
        match resolution {
            BatchIdResolution::Ambiguous(mut candidates) => {
                candidates.sort();
                assert_eq!(candidates, ["01234567-aaaa", "01234567-bbbb"]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }
}
