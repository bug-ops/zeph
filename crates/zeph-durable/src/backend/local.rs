// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The always-compiled local journal backend.
//!
//! [`LocalBackend`] owns a dedicated [`zeph_db::DbPool`] on its own `durable.db` file (INV-14): a
//! separate database keeps the high-write journal off the shared application pool, where
//! `BEGIN IMMEDIATE` contention would otherwise serialize unrelated writers. The schema lives in
//! `zeph-db/migrations/{sqlite,postgres}/` and is applied via [`zeph_db::run_migrations`]; the
//! backend owns no `.sql` files of its own.
//!
//! # Sealing and integrity
//!
//! Payload-bearing entries (currently [`EntryKind::StepResult`]) are AEAD-sealed through the
//! injected [`PayloadCipher`] before they touch the database, with the entry's location bound as
//! associated data so a sealed blob cannot be relocated to another step or execution. Control
//! entries (currently [`EntryKind::EffectIntent`]) carry no payload; when an HMAC key is configured
//! the backend stamps a keyed BLAKE3 row HMAC over their identity for shared-database deployments.
//! When no cipher is injected the payload is stored verbatim — a development-only posture gated by
//! [`encryption_gate`](crate::encryption_gate) at startup.
//!
//! # Scope
//!
//! This revision journals the step-execution entries — [`EntryKind::StepResult`] and
//! [`EntryKind::EffectIntent`] — that the durable step primitive records, plus execution
//! lifecycle (open and [`finalize`](Journal::finalize)), the writer's restart anchor (`max_seq`),
//! and the idempotency-key point lookup
//! ([`lookup_committed_result`](ExecutionBackend::lookup_committed_result)) that lets a guarded step
//! recognize an already-committed effect after a replay divergence (INV-13). Promise, timer, and
//! checkpoint entries are journaled by the promise/timer and retention layers; until then
//! [`append`](Journal::append) of those kinds fails closed with
//! [`DurableError::UnsupportedEntryKind`] rather than dropping their state. The retention sweep
//! ([`prune`](Journal::prune)) is a no-op stub here.

use std::fmt;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use zeph_db::{DbPool, sql};

use crate::backend::{BackendCapabilities, ExecutionBackend, ExecutionSummary, RedactedEntry};
use crate::cipher::{EntryKindTag, PayloadAad, PayloadCipher, ensure_payload_within_limit};
use crate::config::RetentionPolicy;
use crate::error::DurableError;
use crate::ids::{
    ExecutionId, ExecutionKind, IdempotencyKey, JournalSeq, PromiseId, StepId, TimerId,
};
use crate::journal::{EntryKind, ExecutionStatus, Journal, JournalEntry};
use crate::promise::PromiseRecord;
use crate::retention::{CheckpointSnapshot, FoldedStep, decode_checkpoint, encode_checkpoint};
use crate::waiters::NotifyRegistry;
use tracing::Instrument as _;

/// Slack added to `max_payload_bytes` for the read-side size guard.
///
/// The stored blob carries AEAD framing (key-id, extended nonce, tag) on top of the plaintext, so a
/// payload accepted at exactly the limit on write is slightly larger on read. The guard exists only
/// to reject absurdly large rows before allocation/decryption (INV-11), so a small fixed slack
/// above any real AEAD overhead keeps legitimate near-limit entries readable without weakening the
/// denial-of-service protection.
const SEAL_OVERHEAD_SLACK: u64 = 128;

/// Row shape returned by the `list_executions` query.
type ExecutionRow = (String, String, String, i64, i64, Option<i64>, i64);

/// Row shape returned by the `read_execution_redacted` query.
type RedactedRow = (
    i64,
    i64,
    String,
    Option<Vec<u8>>,
    Option<String>,
    Option<i64>,
    i64,
);

/// Render the first 8 bytes of an idempotency key as a lowercase hex prefix (INV-5).
fn idem_key_prefix(bytes: &[u8]) -> String {
    bytes.iter().take(8).fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// The always-compiled durable backend that journals to a dedicated `durable.db`.
///
/// Construct it from a [`zeph_db::DbPool`] (or open one with [`LocalBackend::open`]), then attach an
/// optional [`PayloadCipher`] and HMAC key with the builder methods. Call [`LocalBackend::init`]
/// once before use to apply the schema migrations.
///
/// # Examples
///
/// ```no_run
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// use zeph_durable::LocalBackend;
///
/// // 1 MiB payload ceiling, matching the spec default.
/// let backend = LocalBackend::open("durable.db", 1_048_576).await?;
/// backend.init().await?;
/// # Ok(()) }
/// ```
pub struct LocalBackend {
    pool: DbPool,
    cipher: Option<Arc<dyn PayloadCipher>>,
    hmac_key: Option<[u8; 32]>,
    max_payload_bytes: u64,
    /// In-process wakeup map for parked promise awaits, shared with the resolver path.
    promise_waiters: NotifyRegistry,
    /// In-process wakeup map for parked timers, shared with the timer service.
    timer_waiters: NotifyRegistry,
}

impl fmt::Debug for LocalBackend {
    /// Redacts the cipher and HMAC key — never print key material or a cipher handle.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalBackend")
            .field("cipher", &self.cipher.as_ref().map(|_| "<cipher>"))
            .field("hmac_key", &self.hmac_key.as_ref().map(|_| "<redacted>"))
            .field("max_payload_bytes", &self.max_payload_bytes)
            .finish_non_exhaustive()
    }
}

impl LocalBackend {
    /// Wrap an existing [`zeph_db::DbPool`] as a local backend with the given payload ceiling.
    ///
    /// Call [`LocalBackend::init`] before any journal operation to apply the schema. Attach a
    /// cipher and HMAC key with [`with_cipher`](Self::with_cipher) and
    /// [`with_hmac_key`](Self::with_hmac_key).
    #[must_use]
    pub fn new(pool: DbPool, max_payload_bytes: u64) -> Self {
        Self {
            pool,
            cipher: None,
            hmac_key: None,
            max_payload_bytes,
            promise_waiters: NotifyRegistry::default(),
            timer_waiters: NotifyRegistry::default(),
        }
    }

    /// Open (or create) a backend on a dedicated `durable.db` file (or `:memory:`).
    ///
    /// Connecting also applies the schema migrations, so a freshly opened backend is ready to use;
    /// [`init`](Self::init) may still be called and is idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the pool cannot be opened or migrations fail.
    pub async fn open(path: &str, max_payload_bytes: u64) -> Result<Self, DurableError> {
        let pool = zeph_db::DbConfig {
            url: path.to_string(),
            pool_size: 5,
        }
        .connect()
        .await
        .map_err(|e| DurableError::storage("open", e))?;
        Ok(Self::new(pool, max_payload_bytes))
    }

    /// Inject the AEAD payload cipher used to seal and open payload-bearing entries.
    #[must_use]
    pub fn with_cipher(mut self, cipher: Arc<dyn PayloadCipher>) -> Self {
        self.cipher = Some(cipher);
        self
    }

    /// Configure the keyed-BLAKE3 HMAC key stamped over control entries on shared-database
    /// deployments.
    #[must_use]
    pub fn with_hmac_key(mut self, key: [u8; 32]) -> Self {
        self.hmac_key = Some(key);
        self
    }

    /// Borrow the underlying pool (for tests and adapters that need direct access).
    #[must_use]
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// Apply the durable schema migrations to the backing pool.
    ///
    /// Idempotent: safe to call repeatedly. The schema is owned by `zeph-db`, not this crate.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if a migration fails.
    pub async fn init(&self) -> Result<(), DurableError> {
        zeph_db::run_migrations(&self.pool)
            .await
            .map_err(|e| DurableError::storage("init", e))?;
        Ok(())
    }

    /// List execution summaries for operability surfaces (the `zeph durable` CLI and TUI).
    ///
    /// Returns at most `limit` executions, newest first, optionally filtered by `status` and `kind`
    /// (each is matched against the raw column tag; `None` disables that filter). Only execution-level
    /// metadata is read — never payload bytes or resolver tokens (INV-5). The per-execution step
    /// count is the number of journal entries recorded for it.
    ///
    /// Span: `durable.backend.list`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the query fails, or [`DurableError::Decode`] if a stored
    /// id or status cannot be reconstructed (schema corruption — the `status` column is
    /// `CHECK`-constrained, so this is a fail-closed guard rather than a routine path).
    pub async fn list_executions(
        &self,
        status: Option<&str>,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ExecutionSummary>, DurableError> {
        let span = tracing::info_span!(
            "durable.backend.list",
            status = status.unwrap_or("*"),
            kind = kind.unwrap_or("*"),
            count = tracing::field::Empty,
        );
        async move {
            // `COALESCE(?, col)` keeps a single positional bind per filter and lets the column type
            // drive the bind type, so the same literal works on both SQLite and Postgres without a
            // cast on the `?` placeholder.
            let rows: Vec<ExecutionRow> =
                zeph_db::query_as(sql!(
                    "SELECT
                        e.execution_id,
                        e.kind,
                        e.status,
                        e.created_at,
                        e.updated_at,
                        e.finalized_at,
                        (SELECT COUNT(*) FROM durable_journal j WHERE j.execution_id = e.execution_id)
                     FROM durable_executions e
                     WHERE e.status = COALESCE(?, e.status)
                       AND e.kind = COALESCE(?, e.kind)
                     ORDER BY e.created_at DESC
                     LIMIT ?"
                ))
                .bind(status)
                .bind(kind)
                .bind(limit)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| DurableError::storage("list", e))?;
            tracing::Span::current().record("count", rows.len());
            rows.into_iter()
                .map(|(id, kind, status, created, updated, finalized, steps)| {
                    Ok(ExecutionSummary {
                        execution_id: parse_execution_id(&id)?,
                        kind,
                        status: ExecutionStatus::from_tag(&status).ok_or(DurableError::Decode {
                            context: "execution status is not a recognized CHECK-constrained value",
                        })?,
                        created_at_ms: created,
                        updated_at_ms: updated,
                        finalized_at_ms: finalized,
                        step_count: steps.max(0).cast_unsigned(),
                    })
                })
                .collect()
        }
        .instrument(span)
        .await
    }

    /// Read one execution's journal entries as redaction-safe metadata, without decrypting payloads.
    ///
    /// Unlike [`read_execution`](Journal::read_execution), this never touches the cipher, so it works
    /// against a journal whose AEAD key is unavailable and never exposes plaintext (INV-5). It backs
    /// the default (redacted) `zeph durable show`/`inspect` output. Entries are returned in append
    /// order.
    ///
    /// Span: `durable.backend.read_redacted`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the query fails.
    pub async fn read_execution_redacted(
        &self,
        id: ExecutionId,
    ) -> Result<Vec<RedactedEntry>, DurableError> {
        let exec = id.as_uuid().to_string();
        let rows: Vec<RedactedRow> = zeph_db::query_as(sql!(
            "SELECT seq, step_id, entry_kind, idem_key, effect_class, LENGTH(payload), created_at
                 FROM durable_journal WHERE execution_id = ? ORDER BY seq"
        ))
        .bind(&exec)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DurableError::storage("read_redacted", e))?;
        Ok(rows
            .into_iter()
            .map(
                |(seq, step, entry_kind, idem, effect_class, payload_len, created)| RedactedEntry {
                    seq,
                    step_id: StepId::new(u32::try_from(step).unwrap_or(0)),
                    entry_kind,
                    effect_class,
                    idem_key_prefix: idem.as_deref().map(idem_key_prefix),
                    payload_len: payload_len.unwrap_or(0).max(0).cast_unsigned(),
                    created_at_ms: created,
                },
            )
            .collect())
    }

    /// Count terminal executions a [`prune`](Journal::prune) sweep would delete under `policy`.
    ///
    /// Read-only: backs `zeph durable prune --dry-run`. It applies the same TTL cutoffs as the
    /// delete path, so the count is exactly what a real sweep would remove now.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the query fails.
    pub async fn count_prunable(&self, policy: &RetentionPolicy) -> Result<u64, DurableError> {
        let cutoffs = crate::retention::PruneCutoffs::from_policy(policy, now_unix_millis());
        let (count,): (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM durable_executions
             WHERE finalized_at IS NOT NULL
               AND ( (status = 'completed' AND finalized_at <= ?)
                  OR (status IN ('failed', 'aborted') AND finalized_at <= ?) )"
        ))
        .bind(cutoffs.completed_before_ms)
        .bind(cutoffs.failed_before_ms)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DurableError::storage("count_prunable", e))?;
        Ok(count.max(0).cast_unsigned())
    }

    /// Ensure a `durable_executions` row exists for `id`, returning whether this is a resume.
    ///
    /// Inserts a fresh `running` row for a new execution (returning `false`) or detects an existing
    /// row for a resumed one (returning `true`). The journal's foreign key requires this row before
    /// any entry is appended, so callers open the execution first.
    ///
    /// Span: `durable.backend.open`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the lookup or insert fails.
    pub async fn open_execution(
        &self,
        id: ExecutionId,
        kind: ExecutionKind,
    ) -> Result<bool, DurableError> {
        let span = tracing::info_span!(
            "durable.backend.open",
            execution_id = %id.as_uuid(),
            kind = kind.as_str(),
            is_resume = tracing::field::Empty,
        );
        async move {
            let exec = id.as_uuid().to_string();
            let existing: Option<(String,)> = zeph_db::query_as(sql!(
                "SELECT status FROM durable_executions WHERE execution_id = ?"
            ))
            .bind(&exec)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| DurableError::storage("open", e))?;
            if existing.is_some() {
                tracing::Span::current().record("is_resume", true);
                return Ok(true);
            }
            let now = now_unix_millis();
            zeph_db::query(sql!(
                "INSERT INTO durable_executions
                    (execution_id, kind, status, created_at, updated_at, finalized_at)
                 VALUES (?, ?, 'running', ?, ?, NULL)"
            ))
            .bind(&exec)
            .bind(kind.as_str())
            .bind(now)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(|e| DurableError::storage("open", e))?;
            tracing::Span::current().record("is_resume", false);
            Ok(false)
        }
        .instrument(span)
        .await
    }

    /// Group-commit a batch of buffered entries in a single write transaction.
    ///
    /// Used by the [`JournalWriter`](crate::JournalWriter) to amortize the WAL fsync across all
    /// entries accumulated within a flush interval. Sealing and HMAC computation run before the
    /// transaction opens, keeping CPU work off the write lock. The whole batch commits atomically;
    /// a single malformed entry aborts the batch.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] on a database failure, or a per-entry error
    /// ([`DurableError::PayloadTooLarge`], [`DurableError::UnsupportedEntryKind`], or a cipher
    /// failure) if an entry cannot be prepared.
    pub(crate) async fn append_batch(&self, entries: &[JournalEntry]) -> Result<(), DurableError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut rows = Vec::with_capacity(entries.len());
        for entry in entries {
            rows.push(self.prepare_row(entry)?);
        }
        // `sql!()` caches its postgres rewrite per call site (see #5431), so hoisting
        // this out of the loop below is no longer required to avoid a leak — kept
        // anyway since it reads the intent clearly and costs nothing.
        let insert = sql!(
            "INSERT INTO durable_journal
                (execution_id, step_id, entry_kind, idem_key, effect_class, payload, payload_version, hmac, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        );
        let mut tx = zeph_db::begin_write(&self.pool)
            .await
            .map_err(|e| DurableError::storage("append_batch", e))?;
        for row in rows {
            zeph_db::query(insert)
                .bind(row.execution_id)
                .bind(row.step_id)
                .bind(row.entry_kind)
                .bind(row.idem_key)
                .bind(row.effect_class)
                .bind(row.payload)
                .bind(row.payload_version)
                .bind(row.hmac)
                .bind(row.created_at)
                .execute(&mut *tx)
                .await
                .map_err(|e| DurableError::storage("append_batch", e))?;
        }
        tx.commit()
            .await
            .map_err(|e| DurableError::storage("append_batch", e))?;
        Ok(())
    }

    /// Look up a committed `StepResult` anywhere in an execution by its [`IdempotencyKey`].
    ///
    /// Backs INV-13: a guarded effect that already committed its result must not re-fire after a
    /// replay divergence restarts the execution fresh. Returns the (opened) `StepResult` entry when
    /// one exists, or `None`. The `idx_durable_journal_idem_key` partial index makes this an
    /// `O(log n)` point lookup rather than a scan.
    ///
    /// Span: `durable.journal.lookup_idem`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the query fails, or [`DurableError::Decode`] if the
    /// located row cannot be reconstructed.
    pub(crate) async fn lookup_committed_result(
        &self,
        id: ExecutionId,
        idem_key: IdempotencyKey,
    ) -> Result<Option<JournalEntry>, DurableError> {
        let span = tracing::info_span!(
            "durable.journal.lookup_idem",
            execution_id = %id.as_uuid(),
            found = tracing::field::Empty,
        );
        async move {
            let rows: Vec<JournalRowRead> = zeph_db::query_as(sql!(
                "SELECT seq, step_id, entry_kind, idem_key, effect_class, payload, payload_version, hmac, created_at
                 FROM durable_journal
                 WHERE execution_id = ? AND idem_key = ? AND entry_kind = 'step_result'
                 ORDER BY seq LIMIT 1"
            ))
            .bind(id.as_uuid().to_string())
            .bind(idem_key.as_bytes().to_vec())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DurableError::storage("lookup_idem", e))?;
            let entry = self.rows_to_entries(id, rows).await?.into_iter().next();
            tracing::Span::current().record("found", entry.is_some());
            Ok(entry)
        }
        .instrument(span)
        .await
    }

    /// Read the highest committed [`JournalSeq`], or `None` for an empty journal.
    ///
    /// The [`JournalWriter`](crate::JournalWriter) calls this on (re)start to anchor itself at the
    /// last durably-committed entry (FR-DE-12). Because `seq` is a database-assigned autoincrement,
    /// resumed appends continue from `MAX(seq) + 1` with neither gap nor duplication.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the query fails.
    pub(crate) async fn max_seq(&self) -> Result<Option<JournalSeq>, DurableError> {
        let max: Option<i64> = zeph_db::query_scalar(sql!("SELECT MAX(seq) FROM durable_journal"))
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DurableError::storage("max_seq", e))?;
        Ok(max.map(JournalSeq::new))
    }

    /// The in-process wakeup registry for parked promise awaits, shared with the resolver path.
    pub(crate) fn promise_waiters(&self) -> &NotifyRegistry {
        &self.promise_waiters
    }

    /// The in-process wakeup registry for parked timers, shared with the timer service.
    pub(crate) fn timer_waiters(&self) -> &NotifyRegistry {
        &self.timer_waiters
    }

    /// Insert a freshly-created promise row (INV-9: only the resolver-token hash is stored).
    ///
    /// Called by `promise()` for a brand-new promise; a resumed execution detects the existing row
    /// via [`promise_state`](Self::promise_state) and never re-inserts. Span: `durable.promise.create`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the insert fails.
    pub(crate) async fn insert_promise(
        &self,
        id: PromiseId,
        execution_id: ExecutionId,
        resolver_token_hash: [u8; 32],
        created_at_ms: i64,
    ) -> Result<(), DurableError> {
        let span = tracing::info_span!("durable.promise.create", promise_id = %id.as_uuid());
        async move {
            zeph_db::query(sql!(
                "INSERT INTO durable_promises
                    (promise_id, execution_id, resolver_token_hash, resolved, payload, created_at, resolved_at)
                 VALUES (?, ?, ?, 0, NULL, ?, NULL)"
            ))
            .bind(id.as_uuid().to_string())
            .bind(execution_id.as_uuid().to_string())
            .bind(resolver_token_hash.to_vec())
            .bind(created_at_ms)
            .execute(&self.pool)
            .await
            .map_err(|e| DurableError::storage("insert_promise", e))?;
            Ok(())
        }
        .instrument(span)
        .await
    }

    /// Read a promise's persisted state, or `None` if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the query fails, or [`DurableError::Decode`] if a stored
    /// field cannot be reconstructed.
    pub(crate) async fn promise_state(
        &self,
        id: PromiseId,
    ) -> Result<Option<PromiseRecord>, DurableError> {
        let row: Option<PromiseRowRead> = zeph_db::query_as(sql!(
            "SELECT execution_id, resolver_token_hash, resolved, payload
             FROM durable_promises WHERE promise_id = ?"
        ))
        .bind(id.as_uuid().to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DurableError::storage("promise_state", e))?;
        let Some((exec, hash, resolved, payload)) = row else {
            return Ok(None);
        };
        Ok(Some(PromiseRecord {
            execution_id: parse_execution_id(&exec)?,
            resolver_token_hash: slice_to_array32(&hash, "promise resolver_token_hash")?,
            resolved: resolved != 0,
            payload,
        }))
    }

    /// Commit a resolved value to a pending promise, returning whether it transitioned.
    ///
    /// The conditional `WHERE resolved = 0` makes a double-resolve a no-op (returns `false`); the
    /// caller has already authenticated the resolver token. On a real transition any in-process
    /// waiter is woken. Span: `durable.promise.resolve`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::PayloadTooLarge`] if the value exceeds the limit, a cipher failure if
    /// sealing fails, or [`DurableError::Storage`] on a database error.
    pub(crate) async fn resolve_promise(
        &self,
        id: PromiseId,
        execution_id: ExecutionId,
        value_plaintext: &[u8],
        resolved_at_ms: i64,
    ) -> Result<bool, DurableError> {
        let span = tracing::info_span!("durable.promise.resolve", promise_id = %id.as_uuid());
        async move {
            ensure_payload_within_limit(value_plaintext.len(), self.max_payload_bytes)?;
            let aad = promise_payload_aad(execution_id, id);
            let sealed = self.seal_payload(value_plaintext, &aad)?;
            let affected = zeph_db::query(sql!(
                "UPDATE durable_promises SET resolved = 1, payload = ?, resolved_at = ?
                 WHERE promise_id = ? AND resolved = 0"
            ))
            .bind(sealed)
            .bind(resolved_at_ms)
            .bind(id.as_uuid().to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| DurableError::storage("resolve_promise", e))?
            .rows_affected();
            if affected > 0 {
                self.promise_waiters.wake(id.as_uuid());
            }
            Ok(affected > 0)
        }
        .instrument(span)
        .await
    }

    /// Open a promise's sealed resolved payload back to plaintext.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::ReplayIntegrity`] if the sealed blob does not authenticate, or
    /// [`DurableError::PayloadTooLarge`] if it exceeds the read-side limit.
    pub(crate) fn open_promise_payload(
        &self,
        id: PromiseId,
        execution_id: ExecutionId,
        sealed: &[u8],
    ) -> Result<Bytes, DurableError> {
        ensure_payload_within_limit(
            sealed.len(),
            self.max_payload_bytes.saturating_add(SEAL_OVERHEAD_SLACK),
        )?;
        let aad = promise_payload_aad(execution_id, id);
        self.open_payload(sealed, &aad)
    }

    /// Arm a durable timer to fire at `due_at_ms` (a `durable_timers` row).
    ///
    /// Span: `durable.timer.arm`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the insert fails.
    pub(crate) async fn arm_timer(
        &self,
        id: TimerId,
        execution_id: ExecutionId,
        due_at_ms: i64,
        created_at_ms: i64,
    ) -> Result<(), DurableError> {
        let span = tracing::info_span!("durable.timer.arm", timer_id = %id.as_uuid(), due_at_ms);
        async move {
            zeph_db::query(sql!(
                "INSERT INTO durable_timers (timer_id, execution_id, due_at, fired, created_at)
                 VALUES (?, ?, ?, 0, ?)"
            ))
            .bind(id.as_uuid().to_string())
            .bind(execution_id.as_uuid().to_string())
            .bind(due_at_ms)
            .bind(created_at_ms)
            .execute(&self.pool)
            .await
            .map_err(|e| DurableError::storage("arm_timer", e))?;
            Ok(())
        }
        .instrument(span)
        .await
    }

    /// Read a timer's `(due_at_ms, fired)` state, or `None` if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the query fails.
    pub(crate) async fn timer_state(
        &self,
        id: TimerId,
    ) -> Result<Option<(i64, bool)>, DurableError> {
        let row: Option<(i64, i64)> = zeph_db::query_as(sql!(
            "SELECT due_at, fired FROM durable_timers WHERE timer_id = ?"
        ))
        .bind(id.as_uuid().to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DurableError::storage("timer_state", e))?;
        Ok(row.map(|(due_at, fired)| (due_at, fired != 0)))
    }

    /// List every unfired timer whose instant is at or before `now_ms`.
    ///
    /// The `idx_durable_timers_due(fired, due_at)` index makes this a range scan over due, unfired
    /// timers rather than a full-table scan.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the query fails, or [`DurableError::Decode`] on a
    /// malformed id.
    pub(crate) async fn due_timers(&self, now_ms: i64) -> Result<Vec<TimerId>, DurableError> {
        let rows: Vec<(String,)> = zeph_db::query_as(sql!(
            "SELECT timer_id FROM durable_timers WHERE fired = 0 AND due_at <= ? ORDER BY due_at"
        ))
        .bind(now_ms)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DurableError::storage("due_timers", e))?;
        rows.into_iter().map(|(id,)| parse_timer_id(&id)).collect()
    }

    /// Mark a timer fired, returning whether it transitioned, and wake its parked waiter.
    ///
    /// Span: `durable.timer.fire`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the update fails.
    pub(crate) async fn mark_timer_fired(&self, id: TimerId) -> Result<bool, DurableError> {
        let span = tracing::info_span!("durable.timer.fire", timer_id = %id.as_uuid());
        async move {
            let affected = zeph_db::query(sql!(
                "UPDATE durable_timers SET fired = 1 WHERE timer_id = ? AND fired = 0"
            ))
            .bind(id.as_uuid().to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| DurableError::storage("mark_timer_fired", e))?
            .rows_affected();
            if affected > 0 {
                self.timer_waiters.wake(id.as_uuid());
            }
            Ok(affected > 0)
        }
        .instrument(span)
        .await
    }

    /// Open each foldable step result's sealed payload into a [`FoldedStep`], in step order.
    ///
    /// The per-step AAD is reconstructed from the row so the opened plaintext authenticates exactly
    /// as it did at rest; the idempotency key is preserved so the replayed-from-snapshot step still
    /// satisfies the divergence guard.
    fn open_foldable_steps(
        &self,
        execution_id: ExecutionId,
        rows: Vec<FoldableRowRead>,
    ) -> Result<Vec<FoldedStep>, DurableError> {
        let mut folded = Vec::with_capacity(rows.len());
        for (step_raw, idem, version, payload) in rows {
            let step = u32::try_from(step_raw).map_err(|_| DurableError::Decode {
                context: "checkpoint step_id out of u32 range",
            })?;
            let idem_bytes = idem.ok_or(DurableError::Decode {
                context: "checkpoint step result missing idem_key",
            })?;
            let idem_key =
                IdempotencyKey::from_bytes(slice_to_array32(&idem_bytes, "checkpoint idem_key")?);
            let sealed = payload.ok_or(DurableError::Decode {
                context: "checkpoint step result missing payload",
            })?;
            let aad = PayloadAad::new(
                execution_id,
                StepId::new(step),
                EntryKindTag::StepResult,
                Some(idem_key),
            );
            let plaintext = self.open_payload(&sealed, &aad)?;
            let payload_version =
                u8::try_from(version.unwrap_or(1)).map_err(|_| DurableError::Decode {
                    context: "checkpoint payload_version out of u8 range",
                })?;
            folded.push(FoldedStep {
                step_id: step,
                idem_key: *idem_key.as_bytes(),
                payload_version,
                payload: plaintext,
            });
        }
        Ok(folded)
    }

    /// Fold an execution's committed-idempotent prefix below `up_to_step` into one checkpoint entry.
    ///
    /// Reads the foldable idempotent step results, packs as many as fit the payload budget into a
    /// sealed snapshot, writes a single [`EntryKind::Checkpoint`] entry, and deletes the folded rows
    /// — all in one transaction. A resume replays the folded steps from the snapshot (the snapshot
    /// preserves each step's idempotency key for the divergence guard) instead of re-running them.
    /// Returns the number of steps folded. Runs only on a background task (spec NEVER: not the hot
    /// path). Span: `durable.journal.checkpoint`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] on a database error, or a cipher failure if (re)sealing
    /// fails.
    pub(crate) async fn checkpoint_fold(
        &self,
        execution_id: ExecutionId,
        up_to_step: u32,
    ) -> Result<u64, DurableError> {
        let span = tracing::info_span!(
            "durable.journal.checkpoint",
            execution_id = %execution_id.as_uuid(),
            folded_count = tracing::field::Empty,
        );
        async move {
            let exec = execution_id.as_uuid().to_string();
            let rows: Vec<FoldableRowRead> = zeph_db::query_as(sql!(
                "SELECT step_id, idem_key, payload_version, payload FROM durable_journal
                 WHERE execution_id = ? AND entry_kind = 'step_result'
                   AND effect_class = 'idempotent' AND step_id < ?
                 ORDER BY step_id"
            ))
            .bind(&exec)
            .bind(i64::from(up_to_step))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DurableError::storage("checkpoint", e))?;
            if rows.is_empty() {
                return Ok(0);
            }

            // Open each sealed result, then keep the budget-bounded prefix that fits a checkpoint.
            let mut folded = self.open_foldable_steps(execution_id, rows)?;
            let lens: Vec<usize> = folded.iter().map(|s| s.payload.len()).collect();
            let take = crate::retention::fold_prefix_len(
                &lens,
                crate::retention::checkpoint_budget(self.max_payload_bytes),
            );
            if take == 0 {
                // Not even one result fits the budget; leave the prefix un-folded rather than write
                // an over-limit checkpoint.
                return Ok(0);
            }
            folded.truncate(take);
            let fold_end = folded.last().map_or(up_to_step, |s| s.step_id.saturating_add(1));

            let snapshot = encode_checkpoint(&folded);
            let snap_aad =
                PayloadAad::new(execution_id, StepId::new(fold_end), EntryKindTag::Checkpoint, None);
            let sealed_snapshot = self.seal_payload(&snapshot, &snap_aad)?;

            let mut tx = zeph_db::begin_write(&self.pool)
                .await
                .map_err(|e| DurableError::storage("checkpoint", e))?;
            zeph_db::query(sql!(
                "INSERT INTO durable_journal
                    (execution_id, step_id, entry_kind, idem_key, effect_class, payload, payload_version, hmac, created_at)
                 VALUES (?, ?, 'checkpoint', NULL, NULL, ?, ?, NULL, ?)"
            ))
            .bind(&exec)
            .bind(i64::from(fold_end))
            .bind(sealed_snapshot)
            .bind(i32::from(crate::step::PAYLOAD_VERSION))
            .bind(now_unix_millis())
            .execute(&mut *tx)
            .await
            .map_err(|e| DurableError::storage("checkpoint", e))?;
            zeph_db::query(sql!(
                "DELETE FROM durable_journal
                 WHERE execution_id = ? AND entry_kind = 'step_result'
                   AND effect_class = 'idempotent' AND step_id < ?"
            ))
            .bind(&exec)
            .bind(i64::from(fold_end))
            .execute(&mut *tx)
            .await
            .map_err(|e| DurableError::storage("checkpoint", e))?;
            tx.commit()
                .await
                .map_err(|e| DurableError::storage("checkpoint", e))?;

            let count = folded.len() as u64;
            tracing::Span::current().record("folded_count", count);
            Ok(count)
        }
        .instrument(span)
        .await
    }

    /// Read every checkpoint snapshot for an execution and reconstruct its folded step results.
    ///
    /// The replay cursor calls this once on resume to preload folded results before walking the
    /// surviving journal rows: each returned [`JournalEntry`] is a `StepResult` whose individual row
    /// was deleted by the fold but whose replay value (and idempotency key, for the divergence guard)
    /// lives in the snapshot. Each snapshot is AEAD-opened with its checkpoint-bound AAD. Returns an
    /// empty vector when the execution has never been folded.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] on a database error, or a decode/cipher failure if a
    /// snapshot is corrupt.
    pub(crate) async fn read_checkpoints(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Vec<JournalEntry>, DurableError> {
        let rows: Vec<(i64, Option<Vec<u8>>)> = zeph_db::query_as(sql!(
            "SELECT step_id, payload FROM durable_journal
             WHERE execution_id = ? AND entry_kind = 'checkpoint' ORDER BY step_id"
        ))
        .bind(execution_id.as_uuid().to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DurableError::storage("read_checkpoints", e))?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let mut folded: CheckpointSnapshot = Vec::new();
        for (up_to, payload) in rows {
            let up_to = u32::try_from(up_to).map_err(|_| DurableError::Decode {
                context: "checkpoint up_to_step out of u32 range",
            })?;
            let sealed = payload.ok_or(DurableError::Decode {
                context: "checkpoint entry missing snapshot payload",
            })?;
            ensure_payload_within_limit(
                sealed.len(),
                self.max_payload_bytes.saturating_add(SEAL_OVERHEAD_SLACK),
            )?;
            let aad = PayloadAad::new(
                execution_id,
                StepId::new(up_to),
                EntryKindTag::Checkpoint,
                None,
            );
            let plaintext = self.open_payload(&sealed, &aad)?;
            folded.extend(decode_checkpoint(&plaintext)?);
        }
        // Reconstruct each folded step as a replayable `StepResult` entry under the real execution
        // kind, so the cursor serves it exactly like a surviving row.
        let kind = self.lookup_kind(execution_id).await?;
        let entries = folded
            .into_iter()
            .map(|step| JournalEntry {
                seq: None,
                execution_id,
                kind,
                step_id: StepId::new(step.step_id),
                entry: EntryKind::StepResult {
                    idempotency_key: IdempotencyKey::from_bytes(step.idem_key),
                    payload: step.payload,
                    effect: crate::EffectClass::Idempotent,
                    payload_version: step.payload_version,
                },
                created_at_ms: 0,
            })
            .collect();
        Ok(entries)
    }

    /// Delete one bounded batch of prunable terminal executions and their child rows.
    ///
    /// Selects up to `batch` executions past their TTL, then deletes their journal, promise, timer,
    /// and execution rows in a single transaction (children first, to respect the foreign keys).
    /// Returns the number of executions removed; the retention loop stops once a batch returns fewer
    /// than `batch`.
    async fn delete_prune_batch(
        &self,
        cutoffs: crate::retention::PruneCutoffs,
        batch: u64,
    ) -> Result<u64, DurableError> {
        let ids: Vec<(String,)> = zeph_db::query_as(sql!(
            "SELECT execution_id FROM durable_executions
             WHERE finalized_at IS NOT NULL
               AND ( (status = 'completed' AND finalized_at <= ?)
                  OR (status IN ('failed', 'aborted') AND finalized_at <= ?) )
             ORDER BY finalized_at LIMIT ?"
        ))
        .bind(cutoffs.completed_before_ms)
        .bind(cutoffs.failed_before_ms)
        .bind(i64::try_from(batch).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DurableError::storage("prune", e))?;
        if ids.is_empty() {
            return Ok(0);
        }
        let journal = sql!("DELETE FROM durable_journal WHERE execution_id = ?");
        let promises = sql!("DELETE FROM durable_promises WHERE execution_id = ?");
        let timers = sql!("DELETE FROM durable_timers WHERE execution_id = ?");
        let executions = sql!("DELETE FROM durable_executions WHERE execution_id = ?");
        let mut tx = zeph_db::begin_write(&self.pool)
            .await
            .map_err(|e| DurableError::storage("prune", e))?;
        for (id,) in &ids {
            for stmt in [journal, promises, timers, executions] {
                zeph_db::query(stmt)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| DurableError::storage("prune", e))?;
            }
        }
        tx.commit()
            .await
            .map_err(|e| DurableError::storage("prune", e))?;
        Ok(ids.len() as u64)
    }

    /// Seal a plaintext payload, or pass it through verbatim when no cipher is configured.
    fn seal_payload(&self, plaintext: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, DurableError> {
        match &self.cipher {
            Some(cipher) => Ok(cipher.seal(plaintext, aad)?),
            None => Ok(plaintext.to_vec()),
        }
    }

    /// Open a sealed payload, or copy it through verbatim when no cipher is configured.
    fn open_payload(&self, sealed: &[u8], aad: &PayloadAad) -> Result<Bytes, DurableError> {
        match &self.cipher {
            Some(cipher) => Ok(Bytes::from(cipher.open(sealed, aad)?)),
            None => Ok(Bytes::copy_from_slice(sealed)),
        }
    }

    /// Compute the keyed-BLAKE3 row HMAC over a control entry's identity, when an HMAC key is set.
    ///
    /// Binds `(execution_id, step_id, entry_kind, idem_key?)` so a control row cannot be forged or
    /// relocated on a shared database. Returns `None` when no key is configured (single-user local).
    fn control_hmac(
        &self,
        entry: &JournalEntry,
        idem_key: Option<&IdempotencyKey>,
    ) -> Option<Vec<u8>> {
        let key = self.hmac_key.as_ref()?;
        let mut input = Vec::with_capacity(16 + 4 + 16 + 32);
        input.extend_from_slice(entry.execution_id.as_bytes());
        input.extend_from_slice(&entry.step_id.value().to_le_bytes());
        input.extend_from_slice(entry.entry.tag().as_bytes());
        if let Some(k) = idem_key {
            input.extend_from_slice(k.as_bytes());
        }
        Some(blake3::keyed_hash(key, &input).as_bytes().to_vec())
    }

    /// Derive the persisted column values for an entry, sealing payloads and stamping HMACs.
    fn prepare_row(&self, entry: &JournalEntry) -> Result<JournalRow, DurableError> {
        let execution_id = entry.execution_id.as_uuid().to_string();
        let step_id = i64::from(entry.step_id.value());
        let created_at = entry.created_at_ms;
        let entry_kind = entry.entry.tag();
        match &entry.entry {
            EntryKind::StepResult {
                idempotency_key,
                payload,
                effect,
                payload_version,
            } => {
                ensure_payload_within_limit(payload.len(), self.max_payload_bytes)?;
                let aad = PayloadAad::new(
                    entry.execution_id,
                    entry.step_id,
                    EntryKindTag::StepResult,
                    Some(*idempotency_key),
                );
                let sealed = self.seal_payload(payload.as_ref(), &aad)?;
                Ok(JournalRow {
                    execution_id,
                    step_id,
                    entry_kind,
                    idem_key: Some(idempotency_key.as_bytes().to_vec()),
                    effect_class: Some(effect.as_str()),
                    payload: Some(sealed),
                    payload_version: Some(i32::from(*payload_version)),
                    hmac: None,
                    created_at,
                })
            }
            EntryKind::EffectIntent {
                idempotency_key,
                effect,
                hmac: _,
            } => {
                // The backend is the HMAC keyholder; it stamps the row HMAC itself when configured
                // and ignores any caller-supplied value.
                let hmac = self.control_hmac(entry, Some(idempotency_key));
                Ok(JournalRow {
                    execution_id,
                    step_id,
                    entry_kind,
                    idem_key: Some(idempotency_key.as_bytes().to_vec()),
                    effect_class: Some(effect.as_str()),
                    payload: None,
                    payload_version: None,
                    hmac,
                    created_at,
                })
            }
            EntryKind::PromiseCreated { .. }
            | EntryKind::PromiseResolved { .. }
            | EntryKind::TimerArmed { .. }
            | EntryKind::TimerFired { .. }
            | EntryKind::Checkpoint { .. } => {
                Err(DurableError::UnsupportedEntryKind { kind: entry_kind })
            }
        }
    }

    /// Look up the owning execution's kind for read-time entry reconstruction.
    async fn lookup_kind(&self, id: ExecutionId) -> Result<ExecutionKind, DurableError> {
        let kind: Option<String> = zeph_db::query_scalar(sql!(
            "SELECT kind FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(id.as_uuid().to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DurableError::storage("read", e))?;
        let kind = kind.ok_or(DurableError::Decode {
            context: "journaled entries reference a missing execution row",
        })?;
        ExecutionKind::from_tag(&kind).ok_or(DurableError::Decode {
            context: "execution kind is not reconstructible (custom kind read-back unsupported)",
        })
    }

    /// Reconstruct a [`JournalEntry`] from a stored row, opening sealed payloads.
    fn row_to_entry(
        &self,
        id: ExecutionId,
        kind: ExecutionKind,
        row: JournalRowRead,
    ) -> Result<JournalEntry, DurableError> {
        let (
            seq,
            step_id_raw,
            entry_kind,
            idem_key,
            effect_class,
            payload,
            payload_version,
            hmac,
            created_at,
        ) = row;
        let step_id =
            StepId::new(
                u32::try_from(step_id_raw).map_err(|_| DurableError::Decode {
                    context: "step_id out of u32 range",
                })?,
            );
        let entry = match entry_kind.as_str() {
            "step_result" => {
                let idem_bytes = idem_key.ok_or(DurableError::Decode {
                    context: "step_result idem_key missing",
                })?;
                let idem_key = IdempotencyKey::from_bytes(slice_to_array32(
                    &idem_bytes,
                    "step_result idem_key",
                )?);
                let effect = effect_class
                    .as_deref()
                    .and_then(crate::EffectClass::from_tag)
                    .ok_or(DurableError::Decode {
                        context: "step_result effect_class missing or invalid",
                    })?;
                let sealed = payload.ok_or(DurableError::Decode {
                    context: "step_result payload missing",
                })?;
                ensure_payload_within_limit(
                    sealed.len(),
                    self.max_payload_bytes.saturating_add(SEAL_OVERHEAD_SLACK),
                )?;
                let aad = PayloadAad::new(id, step_id, EntryKindTag::StepResult, Some(idem_key));
                let opened = self.open_payload(&sealed, &aad)?;
                let version = u8::try_from(payload_version.unwrap_or(1)).map_err(|_| {
                    DurableError::Decode {
                        context: "payload_version out of u8 range",
                    }
                })?;
                EntryKind::StepResult {
                    idempotency_key: idem_key,
                    payload: opened,
                    effect,
                    payload_version: version,
                }
            }
            "effect_intent" => {
                let idem_bytes = idem_key.ok_or(DurableError::Decode {
                    context: "effect_intent idem_key missing",
                })?;
                let idem_key = IdempotencyKey::from_bytes(slice_to_array32(
                    &idem_bytes,
                    "effect_intent idem_key",
                )?);
                let effect = effect_class
                    .as_deref()
                    .and_then(crate::EffectClass::from_tag)
                    .ok_or(DurableError::Decode {
                        context: "effect_intent effect_class missing or invalid",
                    })?;
                let hmac = hmac
                    .map(|bytes| slice_to_array32(&bytes, "effect_intent hmac"))
                    .transpose()?;
                EntryKind::EffectIntent {
                    idempotency_key: idem_key,
                    effect,
                    hmac,
                }
            }
            "checkpoint" => self.checkpoint_entry(id, step_id, payload)?,
            other => {
                return Err(DurableError::UnsupportedEntryKind {
                    kind: static_entry_tag(other),
                });
            }
        };
        Ok(JournalEntry {
            seq: Some(JournalSeq::new(seq)),
            execution_id: id,
            kind,
            step_id,
            entry,
            created_at_ms: created_at,
        })
    }

    /// Reconstruct a [`EntryKind::Checkpoint`] from a stored row, opening its sealed snapshot.
    ///
    /// `step_id` carries the checkpoint's `up_to_step` (the fold boundary); the snapshot is bound to
    /// it in the AAD so a checkpoint blob cannot be relocated to a different fold boundary.
    fn checkpoint_entry(
        &self,
        id: ExecutionId,
        step_id: StepId,
        payload: Option<Vec<u8>>,
    ) -> Result<EntryKind, DurableError> {
        let sealed = payload.ok_or(DurableError::Decode {
            context: "checkpoint entry missing snapshot payload",
        })?;
        ensure_payload_within_limit(
            sealed.len(),
            self.max_payload_bytes.saturating_add(SEAL_OVERHEAD_SLACK),
        )?;
        let aad = PayloadAad::new(id, step_id, EntryKindTag::Checkpoint, None);
        let snapshot = self.open_payload(&sealed, &aad)?;
        Ok(EntryKind::Checkpoint {
            up_to_step: step_id.value(),
            snapshot,
        })
    }

    /// Reconstruct every entry from a fetched row set, sharing one kind lookup.
    async fn rows_to_entries(
        &self,
        id: ExecutionId,
        rows: Vec<JournalRowRead>,
    ) -> Result<Vec<JournalEntry>, DurableError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let kind = self.lookup_kind(id).await?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(self.row_to_entry(id, kind, row)?);
        }
        Ok(entries)
    }
}

impl Journal for LocalBackend {
    async fn append(&self, entry: JournalEntry) -> Result<JournalSeq, DurableError> {
        let span = tracing::info_span!(
            "durable.journal.append",
            execution_id = %entry.execution_id.as_uuid(),
            step_id = entry.step_id.value(),
            entry_kind = entry.entry.tag(),
        );
        async move {
            let row = self.prepare_row(&entry)?;
            let (seq,): (i64,) = zeph_db::query_as(sql!(
                "INSERT INTO durable_journal
                    (execution_id, step_id, entry_kind, idem_key, effect_class, payload, payload_version, hmac, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 RETURNING seq"
            ))
                .bind(row.execution_id)
                .bind(row.step_id)
                .bind(row.entry_kind)
                .bind(row.idem_key)
                .bind(row.effect_class)
                .bind(row.payload)
                .bind(row.payload_version)
                .bind(row.hmac)
                .bind(row.created_at)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| DurableError::storage("append", e))?;
            Ok(JournalSeq::new(seq))
        }
        .instrument(span)
        .await
    }

    async fn read_execution(&self, id: ExecutionId) -> Result<Vec<JournalEntry>, DurableError> {
        let span = tracing::info_span!(
            "durable.journal.read",
            execution_id = %id.as_uuid(),
            step_count = tracing::field::Empty,
        );
        async move {
            let rows: Vec<JournalRowRead> = zeph_db::query_as(sql!(
                "SELECT seq, step_id, entry_kind, idem_key, effect_class, payload, payload_version, hmac, created_at
                 FROM durable_journal WHERE execution_id = ? ORDER BY seq"
            ))
            .bind(id.as_uuid().to_string())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DurableError::storage("read", e))?;
            let entries = self.rows_to_entries(id, rows).await?;
            tracing::Span::current().record("step_count", entries.len());
            Ok(entries)
        }
        .instrument(span)
        .await
    }

    async fn read_execution_range(
        &self,
        id: ExecutionId,
        from_step_id: u32,
        limit: usize,
    ) -> Result<Vec<JournalEntry>, DurableError> {
        let span = tracing::info_span!(
            "durable.journal.read_segment",
            execution_id = %id.as_uuid(),
            from_step_id,
            count = tracing::field::Empty,
        );
        async move {
            let rows: Vec<JournalRowRead> = zeph_db::query_as(sql!(
                "SELECT seq, step_id, entry_kind, idem_key, effect_class, payload, payload_version, hmac, created_at
                 FROM durable_journal WHERE execution_id = ? AND step_id >= ? ORDER BY step_id, seq LIMIT ?"
            ))
            .bind(id.as_uuid().to_string())
            .bind(i64::from(from_step_id))
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DurableError::storage("read_segment", e))?;
            let entries = self.rows_to_entries(id, rows).await?;
            tracing::Span::current().record("count", entries.len());
            Ok(entries)
        }
        .instrument(span)
        .await
    }

    async fn finalize(&self, id: ExecutionId, status: ExecutionStatus) -> Result<(), DurableError> {
        let span = tracing::info_span!(
            "durable.journal.finalize",
            execution_id = %id.as_uuid(),
            status = status.as_str(),
        );
        async move {
            let now = now_unix_millis();
            let finalized_at = (!status.is_running()).then_some(now);
            let mut tx = zeph_db::begin_write(&self.pool)
                .await
                .map_err(|e| DurableError::storage("finalize", e))?;
            zeph_db::query(sql!(
                "UPDATE durable_executions SET status = ?, updated_at = ?, finalized_at = ?
                 WHERE execution_id = ?"
            ))
            .bind(status.as_str())
            .bind(now)
            .bind(finalized_at)
            .bind(id.as_uuid().to_string())
            .execute(&mut *tx)
            .await
            .map_err(|e| DurableError::storage("finalize", e))?;
            tx.commit()
                .await
                .map_err(|e| DurableError::storage("finalize", e))?;
            Ok(())
        }
        .instrument(span)
        .await
    }

    async fn prune(&self, policy: &RetentionPolicy) -> Result<u64, DurableError> {
        let now = now_unix_millis();
        crate::retention::prune_in_batches(policy, now, |cutoffs, batch| {
            self.delete_prune_batch(cutoffs, batch)
        })
        .await
    }
}

impl crate::sealed::Sealed for LocalBackend {}

impl ExecutionBackend for LocalBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            parallel_steps: true,
            // The local backend is in-process on SQLite; a Postgres build talks to a shared server.
            cross_process: cfg!(feature = "postgres"),
            max_payload: usize::try_from(self.max_payload_bytes).unwrap_or(usize::MAX),
        }
    }

    async fn lookup_committed_result(
        &self,
        id: ExecutionId,
        idem_key: IdempotencyKey,
    ) -> Result<Option<JournalEntry>, DurableError> {
        LocalBackend::lookup_committed_result(self, id, idem_key).await
    }
}

/// Column values for a single `durable_journal` row, ready to bind.
struct JournalRow {
    execution_id: String,
    step_id: i64,
    entry_kind: &'static str,
    idem_key: Option<Vec<u8>>,
    effect_class: Option<&'static str>,
    payload: Option<Vec<u8>>,
    payload_version: Option<i32>,
    hmac: Option<Vec<u8>>,
    created_at: i64,
}

/// A `durable_journal` row read back from storage, decoded dialect-agnostically.
///
/// Columns are read as a positional tuple (the convention for crates that depend on `zeph-db` but
/// not `sqlx` directly, mirroring `zeph-scheduler`): integers decode as `i64`/`i32` and blobs as
/// `Vec<u8>`, which both backends satisfy through the same `sql!()`-rewritten query. The
/// field order matches the `SELECT` column list:
/// `(seq, step_id, entry_kind, idem_key, effect_class, payload, payload_version, hmac, created_at)`.
type JournalRowRead = (
    i64,
    i64,
    String,
    Option<Vec<u8>>,
    Option<String>,
    Option<Vec<u8>>,
    Option<i32>,
    Option<Vec<u8>>,
    i64,
);

/// A `durable_promises` row read back from storage, in `SELECT` column order:
/// `(execution_id, resolver_token_hash, resolved, payload)`.
type PromiseRowRead = (String, Vec<u8>, i64, Option<Vec<u8>>);

/// A foldable `durable_journal` step-result row, in `SELECT` column order:
/// `(step_id, idem_key, payload_version, payload)`.
type FoldableRowRead = (i64, Option<Vec<u8>>, Option<i32>, Option<Vec<u8>>);

/// Current Unix time in milliseconds, clamped into `i64` and never panicking.
pub(crate) fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Decode a stored blob into a fixed 32-byte array, failing closed on the wrong length.
fn slice_to_array32(bytes: &[u8], field: &'static str) -> Result<[u8; 32], DurableError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| DurableError::Decode { context: field })
}

/// Parse a stored `execution_id` TEXT column back into an [`ExecutionId`], failing closed.
fn parse_execution_id(text: &str) -> Result<ExecutionId, DurableError> {
    uuid::Uuid::parse_str(text)
        .map(ExecutionId::from_uuid)
        .map_err(|_| DurableError::Decode {
            context: "execution_id is not a valid UUID",
        })
}

/// Parse a stored `timer_id` TEXT column back into a [`TimerId`], failing closed.
fn parse_timer_id(text: &str) -> Result<TimerId, DurableError> {
    uuid::Uuid::parse_str(text)
        .map(TimerId::from_uuid)
        .map_err(|_| DurableError::Decode {
            context: "timer_id is not a valid UUID",
        })
}

/// The AAD binding a promise's resolved payload to `(execution_id, promise_id)`.
///
/// A promise has no [`StepId`], so the promise id is folded into the AAD's idempotency-key slot:
/// a payload sealed for one promise cannot be opened as another's (fail-closed on relocation).
fn promise_payload_aad(execution_id: ExecutionId, promise_id: PromiseId) -> PayloadAad {
    let binding = IdempotencyKey::derive(
        execution_id,
        StepId::new(0),
        promise_id.as_uuid().as_bytes(),
    );
    PayloadAad::new(
        execution_id,
        StepId::new(0),
        EntryKindTag::PromiseResolved,
        Some(binding),
    )
}

/// Map a database `entry_kind` string to a `'static` tag for [`DurableError::UnsupportedEntryKind`].
fn static_entry_tag(tag: &str) -> &'static str {
    match tag {
        "promise_created" => "promise_created",
        "promise_resolved" => "promise_resolved",
        "timer_armed" => "timer_armed",
        "timer_fired" => "timer_fired",
        "checkpoint" => "checkpoint",
        _ => "unknown",
    }
}

// Backend tests open a real pool, so they run under the SQLite build (mirroring `zeph-scheduler`,
// whose `:memory:` pool is SQLite-specific). The dialect-agnostic `sql!()` SQL and `i64`/`Vec<u8>`
// column types are verified to compile under the Postgres feature; live Postgres parity is exercised
// by the `#[ignore]`d integration test below.
#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::cipher::CipherError;
    use crate::effect::EffectClass;

    /// An AAD-authenticated test cipher: a BLAKE3 tag over the AAD prefixes an XOR-masked payload,
    /// so opening with a relocated/forged AAD fails authentication exactly like the real cipher.
    struct XorCipher;
    const XOR_MASK: u8 = 0x5A;

    impl PayloadCipher for XorCipher {
        fn seal(&self, plaintext: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, CipherError> {
            let tag = blake3::hash(&aad.canonical_bytes());
            let mut out = tag.as_bytes()[..8].to_vec();
            out.extend(plaintext.iter().map(|b| b ^ XOR_MASK));
            Ok(out)
        }

        fn open(&self, sealed: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, CipherError> {
            if sealed.len() < 8 {
                return Err(CipherError::Malformed {
                    context: "sealed blob shorter than the aad tag",
                });
            }
            let expected = blake3::hash(&aad.canonical_bytes());
            if sealed[..8] != expected.as_bytes()[..8] {
                return Err(CipherError::Authentication);
            }
            Ok(sealed[8..].iter().map(|b| b ^ XOR_MASK).collect())
        }
    }

    async fn mem_backend(max_payload_bytes: u64) -> LocalBackend {
        let backend = LocalBackend::open(":memory:", max_payload_bytes)
            .await
            .expect("open in-memory backend");
        backend.init().await.expect("apply migrations");
        backend
    }

    fn step_result(exec: ExecutionId, step: u32, payload: &[u8]) -> JournalEntry {
        let step_id = StepId::new(step);
        JournalEntry {
            seq: None,
            execution_id: exec,
            kind: ExecutionKind::AgentTurn,
            step_id,
            entry: EntryKind::StepResult {
                idempotency_key: IdempotencyKey::derive(exec, step_id, b"tool:read"),
                payload: Bytes::copy_from_slice(payload),
                effect: EffectClass::Idempotent,
                payload_version: 1,
            },
            created_at_ms: 100,
        }
    }

    fn effect_intent(exec: ExecutionId, step: u32) -> JournalEntry {
        let step_id = StepId::new(step);
        JournalEntry {
            seq: None,
            execution_id: exec,
            kind: ExecutionKind::AgentTurn,
            step_id,
            entry: EntryKind::EffectIntent {
                idempotency_key: IdempotencyKey::derive(exec, step_id, b"transfer"),
                effect: EffectClass::ExactlyOnceGuarded,
                hmac: None,
            },
            created_at_ms: 100,
        }
    }

    #[tokio::test]
    async fn open_execution_is_fresh_then_resume() {
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        assert!(
            !backend
                .open_execution(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap()
        );
        assert!(
            backend
                .open_execution(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn list_executions_summarizes_and_filters() {
        let backend = mem_backend(1_048_576).await;
        let turn = ExecutionId::new();
        let dag = ExecutionId::new();
        backend
            .open_execution(turn, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backend
            .open_execution(dag, ExecutionKind::DagRun)
            .await
            .unwrap();
        backend.append(step_result(turn, 0, b"a")).await.unwrap();
        backend.append(step_result(turn, 1, b"b")).await.unwrap();
        backend.append(step_result(dag, 0, b"c")).await.unwrap();
        backend
            .finalize(turn, ExecutionStatus::Completed)
            .await
            .unwrap();

        // Unfiltered: both executions with their per-execution step counts.
        let all = backend.list_executions(None, None, 10).await.unwrap();
        assert_eq!(all.len(), 2);

        let turn_row = all
            .iter()
            .find(|e| e.execution_id == turn)
            .expect("turn present");
        assert_eq!(turn_row.kind, "agent_turn");
        assert_eq!(turn_row.status, ExecutionStatus::Completed);
        assert_eq!(turn_row.step_count, 2);
        assert!(turn_row.finalized_at_ms.is_some());

        let dag_row = all
            .iter()
            .find(|e| e.execution_id == dag)
            .expect("dag present");
        assert_eq!(dag_row.status, ExecutionStatus::Running);
        assert_eq!(dag_row.step_count, 1);
        assert!(dag_row.finalized_at_ms.is_none());

        // Status filter narrows to the still-running execution.
        let running = backend
            .list_executions(Some("running"), None, 10)
            .await
            .unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].execution_id, dag);

        // Kind filter narrows to the DAG execution.
        let dags = backend
            .list_executions(None, Some("dag_run"), 10)
            .await
            .unwrap();
        assert_eq!(dags.len(), 1);
        assert_eq!(dags[0].execution_id, dag);

        // Limit caps the result set.
        let one = backend.list_executions(None, None, 1).await.unwrap();
        assert_eq!(one.len(), 1);
    }

    #[tokio::test]
    async fn append_and_read_round_trips_step_result() {
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();

        let seq = backend
            .append(step_result(exec, 0, b"hello"))
            .await
            .unwrap();
        assert_eq!(seq.value(), 1, "first append takes seq 1");

        let entries = backend.read_execution(exec).await.unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0].entry {
            EntryKind::StepResult {
                payload, effect, ..
            } => {
                assert_eq!(payload.as_ref(), b"hello");
                assert_eq!(*effect, EffectClass::Idempotent);
            }
            other => panic!("unexpected entry kind: {other:?}"),
        }
        assert_eq!(entries[0].seq, Some(seq));
    }

    #[tokio::test]
    async fn cipher_seals_payload_at_rest_but_round_trips() {
        let backend = mem_backend(1_048_576)
            .await
            .with_cipher(Arc::new(XorCipher));
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backend
            .append(step_result(exec, 0, b"secret-payload"))
            .await
            .unwrap();

        // The stored column is sealed, never the plaintext.
        let (stored,): (Option<Vec<u8>>,) = zeph_db::query_as(sql!(
            "SELECT payload FROM durable_journal WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        let stored = stored.expect("payload present");
        assert_ne!(
            stored.as_slice(),
            b"secret-payload",
            "payload must be sealed at rest"
        );

        // Reading opens it back to the original plaintext.
        let entries = backend.read_execution(exec).await.unwrap();
        match &entries[0].entry {
            EntryKind::StepResult { payload, .. } => {
                assert_eq!(payload.as_ref(), b"secret-payload");
            }
            other => panic!("unexpected entry kind: {other:?}"),
        }
    }

    #[tokio::test]
    async fn control_entry_hmac_is_stamped_only_when_keyed() {
        let exec = ExecutionId::new();

        let unkeyed = mem_backend(1_048_576).await;
        unkeyed
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        unkeyed.append(effect_intent(exec, 0)).await.unwrap();
        match &unkeyed.read_execution(exec).await.unwrap()[0].entry {
            EntryKind::EffectIntent { hmac, .. } => assert!(hmac.is_none()),
            other => panic!("unexpected entry kind: {other:?}"),
        }

        let keyed = mem_backend(1_048_576).await.with_hmac_key([7u8; 32]);
        let exec2 = ExecutionId::new();
        keyed
            .open_execution(exec2, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        keyed.append(effect_intent(exec2, 0)).await.unwrap();
        match &keyed.read_execution(exec2).await.unwrap()[0].entry {
            EntryKind::EffectIntent { hmac, .. } => {
                assert!(
                    hmac.is_some(),
                    "keyed backend stamps a row HMAC over control entries"
                );
            }
            other => panic!("unexpected entry kind: {other:?}"),
        }
    }

    #[tokio::test]
    async fn promise_and_timer_entries_fail_closed() {
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let timer = JournalEntry {
            seq: None,
            execution_id: exec,
            kind: ExecutionKind::AgentTurn,
            step_id: StepId::new(0),
            entry: EntryKind::TimerArmed {
                timer_id: crate::TimerId::new(),
                due_at_ms: 1_000,
                hmac: None,
            },
            created_at_ms: 0,
        };
        assert_matches!(
            backend.append(timer).await,
            Err(DurableError::UnsupportedEntryKind {
                kind: "timer_armed"
            })
        );
    }

    #[tokio::test]
    async fn payload_over_limit_is_rejected_fail_closed() {
        let backend = mem_backend(8).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let big = vec![0u8; 64];
        assert_matches!(
            backend.append(step_result(exec, 0, &big)).await,
            Err(DurableError::PayloadTooLarge { .. })
        );
    }

    #[tokio::test]
    async fn finalize_marks_terminal_status_and_time() {
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backend
            .finalize(exec, ExecutionStatus::Completed)
            .await
            .unwrap();

        let (status, finalized): (String, Option<i64>) = zeph_db::query_as(sql!(
            "SELECT status, finalized_at FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(status, "completed");
        assert!(finalized.is_some(), "a terminal status stamps finalized_at");
    }

    #[tokio::test]
    async fn max_seq_reflects_committed_appends() {
        let backend = mem_backend(1_048_576).await;
        assert_eq!(
            backend.max_seq().await.unwrap(),
            None,
            "empty journal has no max seq"
        );

        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        for step in 0..3 {
            backend.append(step_result(exec, step, b"x")).await.unwrap();
        }
        assert_eq!(backend.max_seq().await.unwrap(), Some(JournalSeq::new(3)));
    }

    #[tokio::test]
    async fn append_batch_group_commits_every_entry() {
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let batch = vec![
            step_result(exec, 0, b"a"),
            step_result(exec, 1, b"b"),
            step_result(exec, 2, b"c"),
        ];
        backend.append_batch(&batch).await.unwrap();
        assert_eq!(backend.read_execution(exec).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn read_execution_range_bounds_the_segment() {
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        for step in 0..5 {
            backend.append(step_result(exec, step, b"x")).await.unwrap();
        }
        let segment = backend.read_execution_range(exec, 2, 2).await.unwrap();
        assert_eq!(segment.len(), 2);
        assert_eq!(segment[0].step_id, StepId::new(2));
        assert_eq!(segment[1].step_id, StepId::new(3));
    }

    #[tokio::test]
    async fn lookup_committed_result_finds_by_idem_key() {
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let entry = step_result(exec, 0, b"committed");
        let idem_key = match &entry.entry {
            EntryKind::StepResult {
                idempotency_key, ..
            } => *idempotency_key,
            other => panic!("unexpected entry kind: {other:?}"),
        };
        backend.append(entry).await.unwrap();

        let found = backend
            .lookup_committed_result(exec, idem_key)
            .await
            .unwrap()
            .expect("committed result is located by its idempotency key");
        match &found.entry {
            EntryKind::StepResult { payload, .. } => assert_eq!(payload.as_ref(), b"committed"),
            other => panic!("unexpected entry kind: {other:?}"),
        }

        // A key that was never committed yields nothing rather than erroring.
        let absent = IdempotencyKey::derive(exec, StepId::new(99), b"never");
        assert!(
            backend
                .lookup_committed_result(exec, absent)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn capabilities_describe_the_local_profile() {
        let backend = mem_backend(4096).await;
        let caps = backend.capabilities();
        assert!(caps.parallel_steps);
        assert!(
            !caps.cross_process,
            "the SQLite local backend is in-process"
        );
        assert_eq!(caps.max_payload, 4096);
    }

    #[tokio::test]
    async fn promise_insert_state_and_resolve_round_trip() {
        let backend = mem_backend(1_048_576)
            .await
            .with_cipher(Arc::new(XorCipher));
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let promise = PromiseId::derive(exec, StepId::new(0));
        backend
            .insert_promise(promise, exec, [9u8; 32], 100)
            .await
            .unwrap();

        let pending = backend.promise_state(promise).await.unwrap().unwrap();
        assert!(!pending.resolved);
        assert_eq!(pending.execution_id, exec);
        assert_eq!(pending.resolver_token_hash, [9u8; 32]);

        // Resolve seals the value at rest; a second resolve is a no-op.
        assert!(
            backend
                .resolve_promise(promise, exec, b"answer", 200)
                .await
                .unwrap()
        );
        assert!(
            !backend
                .resolve_promise(promise, exec, b"again", 300)
                .await
                .unwrap()
        );

        let resolved = backend.promise_state(promise).await.unwrap().unwrap();
        assert!(resolved.resolved);
        let sealed = resolved.payload.expect("resolved payload present");
        assert_ne!(sealed.as_slice(), b"answer", "payload is sealed at rest");
        let opened = backend
            .open_promise_payload(promise, exec, &sealed)
            .unwrap();
        assert_eq!(opened.as_ref(), b"answer");
    }

    #[tokio::test]
    async fn timer_arm_due_and_fire() {
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let past = TimerId::derive(exec, StepId::new(0));
        let future = TimerId::derive(exec, StepId::new(1));
        backend.arm_timer(past, exec, 1_000, 0).await.unwrap();
        backend
            .arm_timer(future, exec, 9_000_000_000_000, 0)
            .await
            .unwrap();

        // Only the past-due timer is returned at now = 5000.
        let due = backend.due_timers(5_000).await.unwrap();
        assert_eq!(due, vec![past]);

        assert!(backend.mark_timer_fired(past).await.unwrap());
        assert!(
            !backend.mark_timer_fired(past).await.unwrap(),
            "second fire is a no-op"
        );
        assert_eq!(
            backend.timer_state(past).await.unwrap(),
            Some((1_000, true))
        );
        // The fired timer no longer appears as due.
        assert!(backend.due_timers(5_000).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prune_deletes_terminal_executions_past_ttl() {
        let backend = mem_backend(1_048_576).await;
        // An old completed execution (finalized long ago) and a fresh running one.
        let old = ExecutionId::new();
        backend
            .open_execution(old, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backend.append(step_result(old, 0, b"x")).await.unwrap();
        // Backdate its finalized_at far into the past.
        zeph_db::query(sql!(
            "UPDATE durable_executions SET status = 'completed', finalized_at = 1000 WHERE execution_id = ?"
        ))
        .bind(old.as_uuid().to_string())
        .execute(backend.pool())
        .await
        .unwrap();

        let live = ExecutionId::new();
        backend
            .open_execution(live, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backend.append(step_result(live, 0, b"y")).await.unwrap();

        let policy = RetentionPolicy {
            ttl_completed_secs: 1,
            prune_batch_size: 10,
            ..RetentionPolicy::default()
        };
        let deleted = backend.prune(&policy).await.unwrap();
        assert_eq!(deleted, 1, "only the aged terminal execution is pruned");

        // The old execution and its journal are gone; the live one survives.
        assert!(backend.read_execution(old).await.unwrap().is_empty());
        assert!(
            backend
                .promise_state(PromiseId::derive(old, StepId::new(0)))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(backend.read_execution(live).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn checkpoint_fold_compacts_idempotent_prefix_and_replays() {
        let backend = mem_backend(1_048_576)
            .await
            .with_cipher(Arc::new(XorCipher));
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        for step in 0..5 {
            backend
                .append(step_result(exec, step, format!("v{step}").as_bytes()))
                .await
                .unwrap();
        }

        // Fold steps 0..3 into a checkpoint.
        let folded = backend.checkpoint_fold(exec, 3).await.unwrap();
        assert_eq!(folded, 3);

        // The individual rows for the folded steps are gone; steps 3 and 4 remain, plus a checkpoint.
        let remaining = backend.read_execution(exec).await.unwrap();
        let step_results: Vec<u32> = remaining
            .iter()
            .filter(|e| matches!(e.entry, EntryKind::StepResult { .. }))
            .map(|e| e.step_id.value())
            .collect();
        assert_eq!(step_results, vec![3, 4], "folded step rows are deleted");
        assert!(
            remaining
                .iter()
                .any(|e| matches!(e.entry, EntryKind::Checkpoint { .. })),
            "a checkpoint entry replaces the folded prefix"
        );

        // The reconstructed folded results carry the original values and idempotency keys.
        let preloaded = backend.read_checkpoints(exec).await.unwrap();
        assert_eq!(preloaded.len(), 3);
        for (i, entry) in preloaded.iter().enumerate() {
            let step = u32::try_from(i).unwrap();
            assert_eq!(entry.step_id, StepId::new(step));
            match &entry.entry {
                EntryKind::StepResult {
                    payload,
                    idempotency_key,
                    ..
                } => {
                    assert_eq!(payload.as_ref(), format!("v{step}").as_bytes());
                    assert_eq!(
                        *idempotency_key,
                        IdempotencyKey::derive(exec, StepId::new(step), b"tool:read")
                    );
                }
                other => panic!("unexpected folded entry: {other:?}"),
            }
        }
    }
}
