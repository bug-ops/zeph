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
//! the backend stamps a keyed BLAKE3 row HMAC over their identity for shared-database deployments,
//! and every read recomputes and constant-time-verifies that HMAC, failing closed with
//! [`DurableError::ControlIntegrity`] on a forged or relocated row. When no cipher is injected the
//! payload is stored verbatim — a development-only posture gated by
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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use zeph_db::{DbPool, sql};

use crate::backend::execution_lock::ExecutionLock;
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
    /// Directory for per-`ExecutionId` advisory lock files (INV-15, #6122), used by
    /// [`open_execution_exclusive`](Self::open_execution_exclusive). `None` when no on-disk path
    /// is known for this backend — a `:memory:` database, a backend built via
    /// [`LocalBackend::new`] from a caller-supplied pool, or a non-SQLite (Postgres) deployment,
    /// where a filesystem lock file cannot express cross-process exclusivity anyway.
    lock_dir: Option<PathBuf>,
    /// Set once [`sweep_orphans`](Self::sweep_orphans) has emitted its warn-once log for a
    /// `lock_dir = None` backend (#6254), so a background retention tick every
    /// `prune_interval_secs` does not spam the log for the lifetime of the process.
    orphan_sweep_warned: std::sync::atomic::AtomicBool,
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
            lock_dir: None,
            orphan_sweep_warned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Open (or create) a backend on a dedicated `durable.db` file (or `:memory:`).
    ///
    /// Connecting also applies the schema migrations, so a freshly opened backend is ready to use;
    /// [`init`](Self::init) may still be called and is idempotent.
    ///
    /// On the `SQLite` backend, also derives the lock directory used by
    /// [`open_execution_exclusive`](Self::open_execution_exclusive) from `path` (a sibling
    /// `<path>.locks/` directory), unless `path` is `:memory:`. The Postgres backend never derives
    /// one — `path` there is a connection URL (which may embed credentials), not a filesystem path.
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
        let mut backend = Self::new(pool, max_payload_bytes);
        backend.lock_dir = lock_dir_for_path(path);
        Ok(backend)
    }

    /// Inject the AEAD payload cipher used to seal and open payload-bearing entries.
    #[must_use]
    pub fn with_cipher(mut self, cipher: Arc<dyn PayloadCipher>) -> Self {
        self.cipher = Some(cipher);
        self
    }

    /// Configure the keyed-BLAKE3 HMAC key stamped over control entries on shared-database
    /// deployments, and used to verify them again on every read (INV-8).
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

    /// Count crash-orphaned executions a [`sweep_orphans`](Journal::sweep_orphans) sweep would
    /// abort under `policy` (#6254).
    ///
    /// Read-only: backs `zeph durable prune --dry-run`. Mirrors the real sweep's staleness scan
    /// and INV-15 flock liveness check (acquiring and immediately releasing each candidate's
    /// `ExecutionLock`, exactly as the real sweep does, so the count reflects genuinely
    /// unowned rows rather than staleness alone) — but never mutates `status`. Returns `0` when
    /// the sweep is disabled (`stale_running_after_secs == 0`) or this backend has no `lock_dir`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the query fails.
    pub async fn count_orphans(&self, policy: &RetentionPolicy) -> Result<u64, DurableError> {
        if policy.stale_running_after_secs == 0 {
            return Ok(0);
        }
        let Some(lock_dir) = self.lock_dir.clone() else {
            return Ok(0);
        };
        let cutoff_ms = orphan_cutoff_ms(policy, now_unix_millis());
        let candidates: Vec<(String,)> = zeph_db::query_as(sql!(
            "SELECT execution_id FROM durable_executions WHERE status = 'running' AND updated_at <= ?"
        ))
        .bind(cutoff_ms)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DurableError::storage("count_orphans", e))?;
        let mut count = 0u64;
        for (exec_str,) in &candidates {
            let Ok(execution_id) = parse_execution_id(exec_str) else {
                continue;
            };
            if ExecutionLock::acquire(&lock_dir, execution_id).is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Ensure a `durable_executions` row exists for `id`, returning whether this is a resume.
    ///
    /// Inserts a fresh `running` row for a new execution (returning `false`) or detects an existing
    /// row for a resumed one (returning `true`). The journal's foreign key requires this row before
    /// any entry is appended, so callers open the execution first.
    ///
    /// Reopening a row previously [`finalize`](Journal::finalize)d as `completed`, `failed`, or
    /// `aborted` un-finalizes it: status resets to `running` and `finalized_at` clears (INV-16,
    /// #6254). A caller reopening an execution is, by definition, still using it, so the retention
    /// sweep (gated on `finalized_at`) must not consider it prunable while it does — without this,
    /// a long-lived execution finalized at one process's graceful shutdown and legitimately resumed
    /// by a later process (e.g. a per-conversation `AgentTurn` execution) would keep a stale
    /// `finalized_at` and could be pruned out from under its still-active journal. `aborted` rows
    /// are included because the crash-orphan sweep (INV-17) makes `aborted` the common outcome of a
    /// resumable crash: a resumed execution whose row keeps `finalized_at` set is prunable out from
    /// under the active resume — the exact hazard this un-finalize prevents for `completed`/`failed`.
    /// This is also strictly safer for the pre-existing divergence-recovery case, which reopens an
    /// `aborted` row on purpose: it now also protects that fresh re-drive from prune.
    ///
    /// The un-finalize is attempted as a single guarded `UPDATE` (no preceding `SELECT`) so there
    /// is no read-then-write window against a concurrent prune sweep (#6251 critic S1): if the row
    /// was deleted by `prune` between an earlier observation and this call, the `UPDATE` simply
    /// matches zero rows rather than silently resurrecting a half-deleted row. A zero-row `UPDATE`
    /// falls back to checking whether the row exists at all (already `running`/`aborted`, or
    /// genuinely gone) before deciding between reporting a resume or inserting a fresh execution —
    /// so this never reports `is_resume = true` for a row that turned out not to exist.
    ///
    /// Span: `durable.backend.open`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] if the lookup, reset, or insert fails.
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

            // Attempt the un-finalize directly, with no preceding SELECT: this is the only write
            // this call needs to make for an existing terminal row, so there is no window between
            // "observe completed/failed" and "reset to running" for a concurrent prune to act in.
            let reopened = zeph_db::query(sql!(
                "UPDATE durable_executions SET status = 'running', updated_at = ?, finalized_at = NULL
                 WHERE execution_id = ? AND status IN ('completed', 'failed', 'aborted')"
            ))
            .bind(now_unix_millis())
            .bind(&exec)
            .execute(&self.pool)
            .await
            .map_err(|e| DurableError::storage("open", e))?;
            if reopened.rows_affected() > 0 {
                tracing::Span::current().record("is_resume", true);
                return Ok(true);
            }

            // Zero rows: either the row doesn't exist, or it exists but wasn't terminal (already
            // `running`, no reset needed — every terminal status is covered by the UPDATE above).
            // Distinguish the two — if a concurrent prune deleted a terminal row between any
            // earlier observation and this check, this SELECT sees the authoritative post-delete
            // state instead of a stale belief that it's there.
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

    /// Like [`open_execution`](Self::open_execution), but additionally takes a non-blocking,
    /// exclusive, process-scoped advisory lock on `id` before touching the row (INV-15, #6122).
    ///
    /// Closes the race two processes deriving the same `ExecutionId` (e.g. two CLI instances
    /// pointed at the same `memory.sqlite_path` and the same `ConversationId`) would otherwise hit
    /// in [`open_execution`](Self::open_execution)'s unsynchronized SELECT-then-INSERT: both could
    /// observe "no existing row", both insert, and both then drive `next_step` from 0 against the
    /// same journal, corrupting it. The lock is acquired first, so the loser never reaches the
    /// row check at all.
    ///
    /// Returns `(is_resume, lock)`. The caller MUST hold `lock` for as long as it drives the
    /// execution — dropping it releases the lock and allows another process to open the same
    /// `id`. `lock` is `None` when this backend has no on-disk lock directory (a `:memory:`
    /// database, a backend built via [`LocalBackend::new`], or a Postgres deployment), in which
    /// case process exclusivity is not enforced — the caller degrades the same way it already does
    /// for `open_execution`'s other failure modes.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::ExecutionLocked`] if another process already holds `id`'s lock, or
    /// any error [`open_execution`](Self::open_execution) can return.
    pub async fn open_execution_exclusive(
        &self,
        id: ExecutionId,
        kind: ExecutionKind,
    ) -> Result<(bool, Option<ExecutionLock>), DurableError> {
        let lock = self
            .lock_dir
            .as_deref()
            .map(|dir| ExecutionLock::acquire(dir, id))
            .transpose()?;
        let is_resume = self.open_execution(id, kind).await?;
        Ok((is_resume, lock))
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

    /// Claim the one-time replay notification for a promise, returning whether this call won.
    ///
    /// The conditional `WHERE notified_at IS NULL` makes the claim single-winner: the first caller
    /// transitions the row (returns `true`); every later caller is a no-op (returns `false`). This
    /// backs #6027 — a resumed foreground sub-agent's replay notice / TUI completion event must fire
    /// at most once across repeated parent restarts. Unlike [`resolve_promise`](Self::resolve_promise)
    /// it touches only the `notified_at` bookkeeping column and carries no payload, so no sealing /
    /// waiter wakeup is involved. Span: `durable.promise.claim_notify`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Storage`] on a database error.
    pub(crate) async fn claim_promise_notification(
        &self,
        id: PromiseId,
        notified_at_ms: i64,
    ) -> Result<bool, DurableError> {
        let span = tracing::info_span!("durable.promise.claim_notify", promise_id = %id.as_uuid());
        async move {
            let affected = zeph_db::query(sql!(
                "UPDATE durable_promises SET notified_at = ?
                 WHERE promise_id = ? AND notified_at IS NULL"
            ))
            .bind(notified_at_ms)
            .bind(id.as_uuid().to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| DurableError::storage("claim_promise_notification", e))?
            .rows_affected();
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
    ///
    /// The candidate-selection `SELECT` runs *inside* the same `begin_write` transaction as the
    /// deletes (not on the autocommit pool beforehand), closing the race where a concurrent
    /// `open_execution` reopen (un-finalize, #6251) lands between "select prunable ids" and
    /// "delete them" — without this, a legitimately-resumed execution could be deleted out from
    /// under its own reopen. `SQLite`'s `BEGIN IMMEDIATE` (via `begin_write`) already serializes
    /// writers at the file level, so the `SELECT` alone is enough there; `PostgreSQL` needs an
    /// explicit `SELECT ... FOR UPDATE` first to take row locks on the same candidates before
    /// they're read, since a plain `BEGIN` does not otherwise block a concurrent `UPDATE` on those
    /// rows (mirrors the `BEGIN IMMEDIATE` / `SELECT FOR UPDATE` split in `goal/store.rs`).
    async fn delete_prune_batch(
        &self,
        cutoffs: crate::retention::PruneCutoffs,
        batch: u64,
    ) -> Result<u64, DurableError> {
        let mut tx = zeph_db::begin_write(&self.pool)
            .await
            .map_err(|e| DurableError::storage("prune", e))?;

        // Postgres only: lock the same candidate rows before reading them, so a concurrent
        // `open_execution` reopen UPDATE on one of these rows blocks until this transaction
        // commits (and then no longer matches, since the SELECT below re-reads post-commit) or
        // this transaction rolls back. Bounded by the same ORDER BY/LIMIT as the real read below
        // so the lock's blast radius matches the batch, not the whole prunable backlog.
        #[cfg(feature = "postgres")]
        zeph_db::query(sql!(
            "SELECT execution_id FROM durable_executions
             WHERE finalized_at IS NOT NULL
               AND ( (status = 'completed' AND finalized_at <= ?)
                  OR (status IN ('failed', 'aborted') AND finalized_at <= ?) )
             ORDER BY finalized_at LIMIT ?
             FOR UPDATE"
        ))
        .bind(cutoffs.completed_before_ms)
        .bind(cutoffs.failed_before_ms)
        .bind(i64::try_from(batch).unwrap_or(i64::MAX))
        .execute(&mut *tx)
        .await
        .map_err(|e| DurableError::storage("prune", e))?;

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
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| DurableError::storage("prune", e))?;
        if ids.is_empty() {
            tx.commit()
                .await
                .map_err(|e| DurableError::storage("prune", e))?;
            return Ok(0);
        }
        let journal = sql!("DELETE FROM durable_journal WHERE execution_id = ?");
        let promises = sql!("DELETE FROM durable_promises WHERE execution_id = ?");
        let timers = sql!("DELETE FROM durable_timers WHERE execution_id = ?");
        // Re-guarded by the same status/finalized_at predicate as the SELECT above (not just
        // `execution_id = ?`) — belt and suspenders alongside the transactional read above.
        let executions = sql!(
            "DELETE FROM durable_executions
             WHERE execution_id = ?
               AND finalized_at IS NOT NULL
               AND ( (status = 'completed' AND finalized_at <= ?)
                  OR (status IN ('failed', 'aborted') AND finalized_at <= ?) )"
        );
        let mut removed = 0u64;
        for (id,) in &ids {
            for stmt in [journal, promises, timers] {
                zeph_db::query(stmt)
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| DurableError::storage("prune", e))?;
            }
            let result = zeph_db::query(executions)
                .bind(id)
                .bind(cutoffs.completed_before_ms)
                .bind(cutoffs.failed_before_ms)
                .execute(&mut *tx)
                .await
                .map_err(|e| DurableError::storage("prune", e))?;
            removed += result.rows_affected();
        }
        tx.commit()
            .await
            .map_err(|e| DurableError::storage("prune", e))?;
        Ok(removed)
    }

    /// One batch of the crash-orphan sweep (INV-17, #6254).
    ///
    /// Selects up to `batch` `status='running'` rows whose `updated_at` is at or before
    /// `cutoff_ms`, then for each candidate non-blockingly try-acquires its INV-15
    /// `ExecutionLock`: `ExecutionLocked` (a live owner holds it) short-circuits to skip —
    /// staleness of `updated_at` alone is never sufficient grounds to abort. Only when the lock is
    /// acquired does the guarded `UPDATE` run, still holding the lock, so the abort is race-free
    /// against a concurrent `open_execution_exclusive` reopen for the same id (both require the
    /// same non-reentrant flock). The lock releases when it drops at the end of each loop
    /// iteration.
    ///
    /// `cursor` is the previous batch's [`SweepCursor`](crate::retention::SweepCursor) (`None` for
    /// the first batch); the candidate scan is keyset-paginated strictly past it so a skipped
    /// (lock-held) row is never re-selected by a later batch — #6254 C1: without this, a batch
    /// consisting entirely of lock-held rows would re-select the identical rows on every
    /// iteration and the caller's batch loop would never terminate. Returns the number of rows
    /// scanned (for the caller's batch-continuation decision), the number actually aborted, and
    /// the cursor to resume from on the next call.
    async fn sweep_orphan_batch(
        &self,
        lock_dir: &std::path::Path,
        cutoff_ms: i64,
        batch: u64,
        cursor: Option<crate::retention::SweepCursor>,
    ) -> Result<crate::retention::SweepBatchOutcome, DurableError> {
        // Sentinel "no lower bound" cursor: every real `updated_at` (Unix ms) is > i64::MIN, so
        // this keyset predicate is a no-op on the first batch while still using one static,
        // sql!()-cacheable query for both the first and subsequent calls.
        let (after_updated_at, after_exec) = cursor.map_or((i64::MIN, String::new()), |c| {
            (c.updated_at_ms, c.execution_id)
        });

        let candidates: Vec<(String, i64)> = zeph_db::query_as(sql!(
            "SELECT execution_id, updated_at FROM durable_executions
             WHERE status = 'running' AND updated_at <= ?
               AND (updated_at > ? OR (updated_at = ? AND execution_id > ?))
             ORDER BY updated_at, execution_id LIMIT ?"
        ))
        .bind(cutoff_ms)
        .bind(after_updated_at)
        .bind(after_updated_at)
        .bind(&after_exec)
        .bind(i64::try_from(batch).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DurableError::storage("sweep_orphans", e))?;

        let scanned = u64::try_from(candidates.len()).unwrap_or(u64::MAX);
        let next_cursor = candidates
            .last()
            .map(|(id, updated_at)| crate::retention::SweepCursor {
                updated_at_ms: *updated_at,
                execution_id: id.clone(),
            });

        let now = now_unix_millis();
        let abort = sql!(
            "UPDATE durable_executions SET status = 'aborted', finalized_at = ?, updated_at = ?
             WHERE execution_id = ? AND status = 'running' AND finalized_at IS NULL"
        );
        let mut aborted = 0u64;
        for (exec_str, _updated_at) in &candidates {
            let Ok(execution_id) = parse_execution_id(exec_str) else {
                continue;
            };
            match ExecutionLock::acquire(lock_dir, execution_id) {
                Ok(_lock) => {
                    let result = zeph_db::query(abort)
                        .bind(now)
                        .bind(now)
                        .bind(exec_str)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| DurableError::storage("sweep_orphans", e))?;
                    aborted += result.rows_affected();
                    // `_lock` drops here, releasing the flock for the next holder.
                }
                Err(DurableError::ExecutionLocked { .. }) => {
                    // A live owner holds this execution — never abort on staleness alone (INV-17).
                }
                Err(e) => return Err(e),
            }
        }
        Ok(crate::retention::SweepBatchOutcome {
            scanned,
            aborted,
            next_cursor,
        })
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
        self.compute_control_hmac(
            entry.execution_id,
            entry.step_id,
            entry.entry.tag(),
            idem_key,
        )
        .map(|h| h.to_vec())
    }

    /// Core keyed-BLAKE3 computation shared by [`control_hmac`](Self::control_hmac) (write path,
    /// takes a full [`JournalEntry`]) and [`verify_control_hmac`](Self::verify_control_hmac) (read
    /// path, which has the row's identity fields but not yet a reconstructed entry). Returns `None`
    /// when no HMAC key is configured (single-user local).
    fn compute_control_hmac(
        &self,
        execution_id: ExecutionId,
        step_id: StepId,
        tag: &'static str,
        idem_key: Option<&IdempotencyKey>,
    ) -> Option<[u8; 32]> {
        let key = self.hmac_key.as_ref()?;
        let mut input = Vec::with_capacity(16 + 4 + 16 + 32);
        input.extend_from_slice(execution_id.as_bytes());
        input.extend_from_slice(&step_id.value().to_le_bytes());
        input.extend_from_slice(tag.as_bytes());
        if let Some(k) = idem_key {
            input.extend_from_slice(k.as_bytes());
        }
        Some(*blake3::keyed_hash(key, &input).as_bytes())
    }

    /// Recompute and constant-time-verify a control entry's row HMAC read back from storage
    /// (INV-8).
    ///
    /// A no-op only when no HMAC key is configured **and** the row carries no stored HMAC — the
    /// documented single-user local stance where control entries carry no HMAC and none is
    /// enforced. If this backend is unkeyed but the row *does* carry a stamped HMAC, that is
    /// config drift between the writer and this reader (e.g. `shared_db` toggled, or a reader
    /// whose config disagrees with the writer's over the same physical file) and is rejected
    /// fail-closed rather than silently trusted, since an `EffectIntent`'s fields are plaintext
    /// and an unkeyed reader has no way to tell a genuine stamped row from a forged one. When a
    /// key *is* configured, every control row this backend reads must carry a matching HMAC: a
    /// missing HMAC or a mismatch both indicate the row was forged, relocated, or written without
    /// the configured key, and both fail closed with [`DurableError::ControlIntegrity`].
    ///
    /// The comparison uses [`blake3::Hash`] equality, which compares in constant time (the same
    /// idiom used for the promise resolver-token check in `promise.rs`), so a forged HMAC reveals
    /// no timing signal.
    fn verify_control_hmac(
        &self,
        execution_id: ExecutionId,
        step_id: StepId,
        tag: &'static str,
        idem_key: Option<&IdempotencyKey>,
        stored: Option<[u8; 32]>,
    ) -> Result<(), DurableError> {
        let Some(expected) = self.compute_control_hmac(execution_id, step_id, tag, idem_key) else {
            return if stored.is_some() {
                Err(DurableError::ControlIntegrity)
            } else {
                Ok(())
            };
        };
        match stored {
            Some(stored) if blake3::Hash::from(expected) == blake3::Hash::from(stored) => Ok(()),
            _ => Err(DurableError::ControlIntegrity),
        }
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
                self.verify_control_hmac(
                    id,
                    step_id,
                    EntryKindTag::EffectIntent.as_str(),
                    Some(&idem_key),
                    hmac,
                )?;
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
            // `AND status = 'running'` makes this a one-shot transition: whichever of a concurrent
            // divergence-triggered `Aborted` or a caller's `Completed`/`Failed` commits first wins,
            // and the loser's UPDATE affects zero rows instead of clobbering the winner's terminal
            // status (finalize is otherwise safe to call more than once per execution).
            zeph_db::query(sql!(
                "UPDATE durable_executions SET status = ?, updated_at = ?, finalized_at = ?
                 WHERE execution_id = ? AND status = 'running'"
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

    /// Crash-orphan reclamation (INV-17, #6254). See [`Journal::sweep_orphans`] for the contract.
    async fn sweep_orphans(&self, policy: &RetentionPolicy) -> Result<u64, DurableError> {
        if policy.stale_running_after_secs == 0 {
            return Ok(0);
        }
        let Some(lock_dir) = self.lock_dir.clone() else {
            if !self
                .orphan_sweep_warned
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                tracing::warn!(
                    "durable: crash-orphan sweep requires an on-disk advisory-lock dir; orphan \
                     reclamation disabled for this backend (Postgres/:memory:/non-Unix)"
                );
            }
            return Ok(0);
        };
        let cutoff_ms = orphan_cutoff_ms(policy, now_unix_millis());
        crate::retention::sweep_orphans_in_batches(
            policy.prune_batch_size,
            cutoff_ms,
            |cutoff, batch, cursor| self.sweep_orphan_batch(&lock_dir, cutoff, batch, cursor),
        )
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

/// Derive the per-execution lock directory sibling to a `path` passed to
/// [`LocalBackend::open`], `None` for `:memory:`.
///
/// Feature-gated to the `SQLite` backend only (INV-15): under `postgres`, `path` is a connection
/// URL that may embed credentials, and appending a suffix to mint a directory name would risk
/// creating a secret-bearing path component on disk.
#[cfg(feature = "sqlite")]
fn lock_dir_for_path(path: &str) -> Option<std::path::PathBuf> {
    (path != ":memory:").then(|| std::path::PathBuf::from(format!("{path}.locks")))
}

#[cfg(not(feature = "sqlite"))]
fn lock_dir_for_path(_path: &str) -> Option<std::path::PathBuf> {
    None
}

/// Regression coverage for the `postgres`-only branch of [`lock_dir_for_path`] (INV-15): a
/// connection URL — which may embed credentials — must never be used to mint an on-disk lock
/// directory name. The main `mod tests` block below is gated on `feature = "sqlite"` and so never
/// exercises this branch; run with `cargo nextest run -p zeph-durable --no-default-features
/// --features postgres`.
#[cfg(all(test, not(feature = "sqlite")))]
mod postgres_lock_dir_tests {
    use super::lock_dir_for_path;

    #[test]
    fn postgres_url_never_derives_a_lock_dir() {
        assert_eq!(
            lock_dir_for_path("postgres://user:secret@host/db"),
            None,
            "a Postgres connection URL (which may embed credentials) must never be used to mint \
             an on-disk lock directory name"
        );
        assert_eq!(lock_dir_for_path(":memory:"), None);
    }
}

/// Current Unix time in milliseconds, clamped into `i64` and never panicking.
pub(crate) fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The absolute `updated_at` cutoff (Unix ms) at or before which a `status='running'` row becomes
/// a crash-orphan sweep candidate (INV-17, #6254).
fn orphan_cutoff_ms(policy: &RetentionPolicy, now_ms: i64) -> i64 {
    let threshold =
        i64::try_from(policy.stale_running_after_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    now_ms.saturating_sub(threshold)
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
    async fn open_execution_exclusive_is_fresh_then_resume() {
        // A file-backed (not `:memory:`) backend is required: only `LocalBackend::open` with a
        // real on-disk path derives a `lock_dir` (#6122).
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("durable.db");
        let backend = LocalBackend::open(&db_path.to_string_lossy(), 1_048_576)
            .await
            .unwrap();
        backend.init().await.unwrap();

        let exec = ExecutionId::new();
        let (is_resume, lock) = backend
            .open_execution_exclusive(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        assert!(!is_resume);
        assert!(lock.is_some(), "a file-backed backend must derive a lock");
        drop(lock);

        let (is_resume, _lock) = backend
            .open_execution_exclusive(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        assert!(is_resume);
    }

    /// Regression test for #6122: two `LocalBackend` handles onto the same on-disk journal (as
    /// two independent CLI processes sharing `memory.sqlite_path` would each construct) must not
    /// both be able to hold `open_execution_exclusive` for the same colliding `ExecutionId`
    /// concurrently.
    #[tokio::test]
    async fn open_execution_exclusive_rejects_concurrent_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("durable.db");
        let url = db_path.to_string_lossy().into_owned();

        let backend_a = LocalBackend::open(&url, 1_048_576).await.unwrap();
        backend_a.init().await.unwrap();
        let backend_b = LocalBackend::open(&url, 1_048_576).await.unwrap();

        let exec = ExecutionId::new();
        let (_, _lock_a) = backend_a
            .open_execution_exclusive(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();

        let err = backend_b
            .open_execution_exclusive(exec, ExecutionKind::AgentTurn)
            .await
            .expect_err("a second concurrent holder must be rejected");
        assert!(
            matches!(err, DurableError::ExecutionLocked { execution_id, .. } if execution_id == exec),
            "expected ExecutionLocked, got {err:?}"
        );
    }

    #[tokio::test]
    async fn open_execution_exclusive_on_memory_backend_returns_no_lock() {
        // `:memory:` has no on-disk directory to lock, so it degrades to unenforced exclusivity —
        // consistent with `SessionEventLog::open_exclusive`'s non-Unix degrade.
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        let (is_resume, lock) = backend
            .open_execution_exclusive(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        assert!(!is_resume);
        assert!(lock.is_none());
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

    /// Regression for #6043/#6044: a control entry written under one HMAC key must fail closed
    /// with [`DurableError::ControlIntegrity`] when read back under a *different* key — the
    /// forged/relocated-row rejection the row HMAC exists to provide. Both backends share the
    /// same underlying pool (a second `LocalBackend` handle over the same connection), so this
    /// exercises the read path's recompute-and-compare, not just a difference in whether a key is
    /// configured at all.
    #[tokio::test]
    async fn read_execution_rejects_control_hmac_under_wrong_key() {
        let writer = mem_backend(1_048_576).await.with_hmac_key([1u8; 32]);
        let exec = ExecutionId::new();
        writer
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        writer.append(effect_intent(exec, 0)).await.unwrap();

        let wrong_key_reader =
            LocalBackend::new(writer.pool().clone(), 1_048_576).with_hmac_key([2u8; 32]);
        assert_matches!(
            wrong_key_reader.read_execution(exec).await,
            Err(DurableError::ControlIntegrity)
        );

        // Reading under the correct key still succeeds.
        let right_key_reader =
            LocalBackend::new(writer.pool().clone(), 1_048_576).with_hmac_key([1u8; 32]);
        assert!(right_key_reader.read_execution(exec).await.is_ok());
    }

    /// Regression for #6043/#6044: a control entry written by an *unkeyed* backend (`hmac =
    /// NULL`) must fail closed when later read by a keyed backend — a keyed backend enforces that
    /// every control row it reads carries a matching HMAC, so a missing HMAC is treated the same
    /// as a mismatched one rather than silently passing through unverified.
    #[tokio::test]
    async fn read_execution_rejects_missing_hmac_on_keyed_backend() {
        let writer = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        writer
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        writer.append(effect_intent(exec, 0)).await.unwrap();

        let keyed_reader =
            LocalBackend::new(writer.pool().clone(), 1_048_576).with_hmac_key([3u8; 32]);
        assert_matches!(
            keyed_reader.read_execution(exec).await,
            Err(DurableError::ControlIntegrity)
        );
    }

    /// Regression for #6043/#6044 (review S1): a control entry written by a *keyed* backend must
    /// fail closed when later read by an *unkeyed* backend, rather than silently trusting the
    /// stamped HMAC as an ordinary (unverified) plaintext field. Before this fix,
    /// `verify_control_hmac` returned `Ok(())` unconditionally whenever the reader had no HMAC
    /// key, regardless of whether the stored row carried one — so config drift between a keyed
    /// writer and an unkeyed reader over the same physical file (e.g. `shared_db` toggled, or a
    /// reader whose config disagrees with the writer's) let a stamped row through unverified,
    /// which is exactly the forgery-acceptance gap #6043 says the row HMAC closes.
    #[tokio::test]
    async fn read_execution_rejects_stamped_hmac_on_unkeyed_backend() {
        let writer = mem_backend(1_048_576).await.with_hmac_key([4u8; 32]);
        let exec = ExecutionId::new();
        writer
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        writer.append(effect_intent(exec, 0)).await.unwrap();

        let unkeyed_reader = LocalBackend::new(writer.pool().clone(), 1_048_576);
        assert_matches!(
            unkeyed_reader.read_execution(exec).await,
            Err(DurableError::ControlIntegrity)
        );
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
    async fn finalize_is_a_noop_once_already_terminal() {
        // #6251: finalize must be safe to call more than once (e.g. a caller's own `Completed`
        // racing the divergence guard's `Aborted`) — whichever status lands first wins.
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

        // A later call with a different terminal status must not overwrite the first.
        backend
            .finalize(exec, ExecutionStatus::Failed)
            .await
            .unwrap();

        let (status,): (String,) = zeph_db::query_as(sql!(
            "SELECT status FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(
            status, "completed",
            "the first terminal status must stick; a later finalize call is a no-op"
        );
    }

    #[tokio::test]
    async fn finalize_after_abort_is_a_noop() {
        // #6251: the reverse direction of the divergence race — the internal `Aborted` transition
        // (replay-divergence guard) commits first, so a consumer's later own `Completed`/`Failed`
        // call must be a no-op rather than resurrecting the row out of its aborted state.
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backend
            .finalize(exec, ExecutionStatus::Aborted)
            .await
            .unwrap();

        backend
            .finalize(exec, ExecutionStatus::Completed)
            .await
            .unwrap();

        let (status,): (String,) = zeph_db::query_as(sql!(
            "SELECT status FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(
            status, "aborted",
            "an aborted execution must not be overwritten by a later Completed/Failed call"
        );
    }

    #[tokio::test]
    async fn reopening_a_finalized_execution_resets_it_to_running() {
        // #6251: a finalized execution that is legitimately reopened (e.g. a resumed conversation)
        // must not keep a stale `finalized_at` — otherwise the retention sweep could prune a row
        // that is still receiving new journal writes.
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

        let is_resume = backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        assert!(is_resume, "the row already existed, so this is a resume");

        let (status, finalized): (String, Option<i64>) = zeph_db::query_as(sql!(
            "SELECT status, finalized_at FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(
            status, "running",
            "reopening a completed execution must un-finalize it"
        );
        assert!(
            finalized.is_none(),
            "reopening must clear the stale finalized_at"
        );
    }

    #[tokio::test]
    async fn reopening_a_failed_execution_resets_it_to_running() {
        // #6251: same guarantee as `reopening_a_finalized_execution_resets_it_to_running`, but for
        // the `Failed` terminal status — e.g. a scheduler retry of the same (job_name, slot_ms)
        // after the previous fire failed must not orphan a `Failed` row.
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backend
            .finalize(exec, ExecutionStatus::Failed)
            .await
            .unwrap();

        let is_resume = backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        assert!(is_resume, "the row already existed, so this is a resume");

        let (status, finalized): (String, Option<i64>) = zeph_db::query_as(sql!(
            "SELECT status, finalized_at FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(
            status, "running",
            "reopening a failed execution must un-finalize it"
        );
        assert!(
            finalized.is_none(),
            "reopening must clear the stale finalized_at"
        );
    }

    #[tokio::test]
    async fn reopening_an_aborted_execution_un_finalizes_it() {
        // INV-16 (#6254): reopening a row in ANY terminal status — including `aborted` — must
        // un-finalize it back to `running` with `finalized_at` cleared. This covers both the
        // pre-existing divergence-recovery reopen (which starts a fresh replay cursor on
        // purpose) and the new crash-orphan sweep (INV-17), which makes `aborted` the common
        // outcome of a resumable crash: a resumed execution whose row keeps `finalized_at` set
        // would otherwise be prunable out from under the active resume.
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backend
            .finalize(exec, ExecutionStatus::Aborted)
            .await
            .unwrap();

        let is_resume = backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        assert!(is_resume, "the row already existed, so this is a resume");

        let (status, finalized): (String, Option<i64>) = zeph_db::query_as(sql!(
            "SELECT status, finalized_at FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(
            status, "running",
            "reopening an aborted execution must un-finalize it (INV-16)"
        );
        assert!(
            finalized.is_none(),
            "reopening must clear the stale finalized_at"
        );
    }

    #[tokio::test]
    async fn reopen_of_a_row_deleted_out_from_under_it_starts_fresh() {
        // #6251 critic S1: simulates the tail of the prune-vs-reopen race — a concurrent prune
        // sweep deletes the row entirely before the reopen's guarded UPDATE runs. The guarded
        // UPDATE must match zero rows (not resurrect a half-deleted row), and the existence
        // fallback must see the row is genuinely gone and start a fresh execution rather than
        // falsely reporting `is_resume = true` for a row that no longer exists.
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

        // Simulate the prune sweep's delete completing before the reopen runs.
        zeph_db::query(sql!(
            "DELETE FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .execute(backend.pool())
        .await
        .unwrap();

        let is_resume = backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        assert!(
            !is_resume,
            "a row deleted by a concurrent prune must be reported as a fresh execution, not a resume"
        );

        let (status,): (String,) = zeph_db::query_as(sql!(
            "SELECT status FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(status, "running", "the fresh row starts running");
    }

    #[tokio::test]
    async fn prune_does_not_delete_a_row_reopened_since_it_was_finalized() {
        // #6251 critic S1: a row finalized, then legitimately reopened (un-finalized back to
        // running) before prune runs, must not be deleted even though prune's cutoff would have
        // matched its now-stale-if-it-were-still-finalized state.
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backend.append(step_result(exec, 0, b"x")).await.unwrap();
        zeph_db::query(sql!(
            "UPDATE durable_executions SET status = 'completed', finalized_at = 1000 WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .execute(backend.pool())
        .await
        .unwrap();

        // A legitimate resume reopens and un-finalizes it before the prune sweep runs.
        let is_resume = backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        assert!(is_resume);

        let policy = RetentionPolicy {
            ttl_completed_secs: 1,
            prune_batch_size: 10,
            ..RetentionPolicy::default()
        };
        let deleted = backend.prune(&policy).await.unwrap();
        assert_eq!(
            deleted, 0,
            "a reopened (un-finalized) execution must not be pruned"
        );
        assert_eq!(
            backend.read_execution(exec).await.unwrap().len(),
            1,
            "the execution's journal must survive"
        );
    }

    #[tokio::test]
    async fn concurrent_prune_and_reopen_never_lose_or_corrupt_the_row() {
        // #6251 critic S1: the deterministic tests above exercise each ordering of the prune-vs-
        // reopen race one step at a time; this test drives the two operations as genuinely
        // concurrent tasks against a real multi-connection pool (file-backed — `:memory:` forces
        // a single connection, per `zeph-db/src/pool.rs`'s `connect_sqlite`, which would serialize
        // the two calls trivially and prove nothing about the locking fix). Runs many trials with
        // fresh executions so the two tasks' actual scheduling order varies across iterations,
        // covering both "prune's tx starts first" and "reopen's UPDATE starts first" without
        // needing artificial delay injection into the DB layer.
        //
        // Invariant checked every trial, regardless of which task wins: neither operation errors,
        // and the row is never lost — it either stays `running` (reopen won, or ran after prune's
        // read already excluded it) or is deleted and then reinserted fresh by reopen's
        // does-not-exist fallback (prune won). It must never end up half-deleted (FK violation on
        // a later journal append) or stuck `completed` with a live journal.
        let dir = tempfile::tempdir().unwrap();
        let db_url = dir.path().join("durable.db").to_string_lossy().into_owned();
        let backend = Arc::new(LocalBackend::open(&db_url, 1_048_576).await.unwrap());
        backend.init().await.unwrap();

        let policy = RetentionPolicy {
            ttl_completed_secs: 1,
            prune_batch_size: 10,
            ..RetentionPolicy::default()
        };

        for _ in 0..20 {
            let exec = ExecutionId::new();
            backend
                .open_execution(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap();
            backend.append(step_result(exec, 0, b"x")).await.unwrap();
            // Backdate finalized_at so this row is immediately prune-eligible.
            zeph_db::query(sql!(
                "UPDATE durable_executions SET status = 'completed', finalized_at = 1000 WHERE execution_id = ?"
            ))
            .bind(exec.as_uuid().to_string())
            .execute(backend.pool())
            .await
            .unwrap();

            let reopen_backend = backend.clone();
            let reopen = tokio::spawn(async move {
                reopen_backend
                    .open_execution(exec, ExecutionKind::AgentTurn)
                    .await
            });
            let prune_backend = backend.clone();
            let policy_for_task = policy.clone();
            let prune = tokio::spawn(async move { prune_backend.prune(&policy_for_task).await });

            let (reopen_result, prune_result) = tokio::join!(reopen, prune);
            reopen_result
                .expect("reopen task must not panic")
                .expect("reopen must not error under concurrent prune");
            prune_result
                .expect("prune task must not panic")
                .expect("prune must not error under a concurrent reopen");

            let (status,): (String,) = zeph_db::query_as(sql!(
                "SELECT status FROM durable_executions WHERE execution_id = ?"
            ))
            .bind(exec.as_uuid().to_string())
            .fetch_one(backend.pool())
            .await
            .expect(
                "the row must exist under either race outcome — reopened-running, or \
                 deleted-then-reinserted-fresh-running by reopen's fallback",
            );
            assert_eq!(
                status, "running",
                "whichever task wins, the row must end up running — never left completed \
                 (orphaned from a live journal) or absent"
            );
        }
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
    async fn claim_promise_notification_is_single_winner() {
        let backend = mem_backend(1_048_576).await;
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

        // First claim wins (transitions notified_at from NULL).
        assert!(
            backend
                .claim_promise_notification(promise, 200)
                .await
                .unwrap()
        );
        // Every later claim on the same promise is a no-op.
        assert!(
            !backend
                .claim_promise_notification(promise, 300)
                .await
                .unwrap()
        );
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

    /// Backdate a `durable_executions` row's `updated_at` so it becomes a sweep candidate.
    async fn backdate_updated_at(backend: &LocalBackend, id: ExecutionId, updated_at_ms: i64) {
        zeph_db::query(sql!(
            "UPDATE durable_executions SET updated_at = ? WHERE execution_id = ?"
        ))
        .bind(updated_at_ms)
        .bind(id.as_uuid().to_string())
        .execute(backend.pool())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sweep_orphans_disabled_when_threshold_is_zero() {
        // A file-backed backend so the sweep would otherwise have a lock_dir to work with;
        // stale_running_after_secs = 0 must short-circuit before any scan.
        let dir = tempfile::tempdir().unwrap();
        let backend =
            LocalBackend::open(&dir.path().join("durable.db").to_string_lossy(), 1_048_576)
                .await
                .unwrap();
        backend.init().await.unwrap();

        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backdate_updated_at(&backend, exec, 0).await;

        let policy = RetentionPolicy {
            stale_running_after_secs: 0,
            ..RetentionPolicy::default()
        };
        let aborted = backend.sweep_orphans(&policy).await.unwrap();
        assert_eq!(
            aborted, 0,
            "stale_running_after_secs = 0 disables the sweep"
        );

        let (status,): (String,) = zeph_db::query_as(sql!(
            "SELECT status FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(status, "running");
    }

    #[tokio::test]
    async fn sweep_orphans_is_a_documented_no_op_on_memory_backend() {
        // `:memory:` has no on-disk lock_dir (INV-15 degrade), so the sweep must never abort on
        // staleness alone — FR-DE-19.
        let backend = mem_backend(1_048_576).await;
        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backdate_updated_at(&backend, exec, 0).await;

        let policy = RetentionPolicy {
            stale_running_after_secs: 1,
            ..RetentionPolicy::default()
        };
        let aborted = backend.sweep_orphans(&policy).await.unwrap();
        assert_eq!(
            aborted, 0,
            "a lock_dir=None backend must never abort on staleness alone"
        );

        let (status,): (String,) = zeph_db::query_as(sql!(
            "SELECT status FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(status, "running");
    }

    #[tokio::test]
    async fn sweep_orphans_aborts_a_stale_running_execution_with_no_live_owner() {
        // FR-DE-16/17: a stale `running` row whose lock is free (no live owner) is hard-aborted.
        let dir = tempfile::tempdir().unwrap();
        let backend =
            LocalBackend::open(&dir.path().join("durable.db").to_string_lossy(), 1_048_576)
                .await
                .unwrap();
        backend.init().await.unwrap();

        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        // Nothing holds this execution's ExecutionLock — `open_execution` (not `_exclusive`)
        // never acquires one, simulating a crashed owner whose flock released on process exit.
        backdate_updated_at(&backend, exec, 0).await;

        let policy = RetentionPolicy {
            stale_running_after_secs: 1,
            ..RetentionPolicy::default()
        };
        let aborted = backend.sweep_orphans(&policy).await.unwrap();
        assert_eq!(aborted, 1);

        let (status, finalized): (String, Option<i64>) = zeph_db::query_as(sql!(
            "SELECT status, finalized_at FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(status, "aborted");
        assert!(finalized.is_some());
    }

    #[tokio::test]
    async fn sweep_orphans_skips_an_execution_whose_lock_is_held_by_a_live_owner() {
        // INV-17: staleness of `updated_at` alone is never sufficient — a stale-but-alive
        // execution (long single step, parked HITL promise, multi-hour job) must survive the
        // sweep as long as its owner still holds the INV-15 flock.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("durable.db");
        let url = db_path.to_string_lossy().into_owned();

        let owner = LocalBackend::open(&url, 1_048_576).await.unwrap();
        owner.init().await.unwrap();
        let sweeper = LocalBackend::open(&url, 1_048_576).await.unwrap();

        let exec = ExecutionId::new();
        let (_, _lock) = owner
            .open_execution_exclusive(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backdate_updated_at(&owner, exec, 0).await;

        let policy = RetentionPolicy {
            stale_running_after_secs: 1,
            ..RetentionPolicy::default()
        };
        let aborted = sweeper.sweep_orphans(&policy).await.unwrap();
        assert_eq!(aborted, 0, "a live-held lock must never be swept");

        let (status,): (String,) = zeph_db::query_as(sql!(
            "SELECT status FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(owner.pool())
        .await
        .unwrap();
        assert_eq!(status, "running");
    }

    #[tokio::test]
    async fn sweep_orphans_leaves_a_fresh_running_execution_untouched() {
        // A recently-updated `running` row is not yet a sweep candidate at all.
        let dir = tempfile::tempdir().unwrap();
        let backend =
            LocalBackend::open(&dir.path().join("durable.db").to_string_lossy(), 1_048_576)
                .await
                .unwrap();
        backend.init().await.unwrap();

        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();

        let policy = RetentionPolicy {
            stale_running_after_secs: 3600,
            ..RetentionPolicy::default()
        };
        let aborted = backend.sweep_orphans(&policy).await.unwrap();
        assert_eq!(aborted, 0);
    }

    #[tokio::test]
    async fn count_orphans_matches_sweep_without_mutating() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            LocalBackend::open(&dir.path().join("durable.db").to_string_lossy(), 1_048_576)
                .await
                .unwrap();
        backend.init().await.unwrap();

        let exec = ExecutionId::new();
        backend
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        backdate_updated_at(&backend, exec, 0).await;

        let policy = RetentionPolicy {
            stale_running_after_secs: 1,
            ..RetentionPolicy::default()
        };
        let counted = backend.count_orphans(&policy).await.unwrap();
        assert_eq!(counted, 1);

        // count_orphans must not have mutated the row.
        let (status,): (String,) = zeph_db::query_as(sql!(
            "SELECT status FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(backend.pool())
        .await
        .unwrap();
        assert_eq!(status, "running");

        let aborted = backend.sweep_orphans(&policy).await.unwrap();
        assert_eq!(
            aborted, counted,
            "sweep must abort exactly what count_orphans counted"
        );
    }

    /// Batching-boundary regression: a candidate set straddling `prune_batch_size` (one more row
    /// than a single batch) must be fully processed across multiple batches, not just the first
    /// one. Exercises the real `sweep_orphan_batch`/`sweep_orphans_in_batches` composition end to
    /// end (not the pure-logic unit test in `retention.rs`), so the SQL `LIMIT` and the
    /// `scanned`-driven continuation check are both proven against a real DB.
    #[tokio::test]
    async fn sweep_orphans_processes_every_batch_when_candidates_straddle_the_batch_size() {
        let dir = tempfile::tempdir().unwrap();
        let backend =
            LocalBackend::open(&dir.path().join("durable.db").to_string_lossy(), 1_048_576)
                .await
                .unwrap();
        backend.init().await.unwrap();

        let batch_size = 2u64;
        let candidate_count = batch_size + 1; // straddles the batch boundary
        let mut execs = Vec::new();
        for _ in 0..candidate_count {
            let exec = ExecutionId::new();
            backend
                .open_execution(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap();
            backdate_updated_at(&backend, exec, 0).await;
            execs.push(exec);
        }

        let policy = RetentionPolicy {
            stale_running_after_secs: 1,
            prune_batch_size: batch_size,
            ..RetentionPolicy::default()
        };
        let aborted = backend.sweep_orphans(&policy).await.unwrap();
        assert_eq!(
            aborted, candidate_count,
            "every candidate must be aborted, including the one past the first batch"
        );

        for exec in execs {
            let (status,): (String,) = zeph_db::query_as(sql!(
                "SELECT status FROM durable_executions WHERE execution_id = ?"
            ))
            .bind(exec.as_uuid().to_string())
            .fetch_one(backend.pool())
            .await
            .unwrap();
            assert_eq!(status, "aborted");
        }
    }

    /// #6254 C1 regression: when the count of stale-but-live (lock-held) candidates is `>=
    /// prune_batch_size`, the sweep must still terminate rather than looping forever re-selecting
    /// the same lock-held rows. Before the keyset-pagination fix, `sweep_orphan_batch`'s candidate
    /// `SELECT` had no offset/cursor, so a batch consisting entirely of lock-held rows (which the
    /// sweep never deletes, mutates, or otherwise removes from the `status='running'` candidate
    /// set) would re-select the identical rows on every iteration: `scanned` would stay `==
    /// batch` and `aborted` would stay `0` forever, so `sweep_orphans_in_batches`'s `scanned <
    /// batch` continuation check would never trip. Exercises the real DB-backed
    /// `sweep_orphan_batch`/`sweep_orphans_in_batches` composition (not the pure-logic
    /// simulation in `retention.rs`) with more lock-held candidates than `prune_batch_size`, so a
    /// naive single-batch-worth-of-locks reproduction would not have caught a bug that only
    /// manifests once the candidate set spans multiple batches.
    #[tokio::test]
    async fn sweep_orphans_terminates_when_lock_held_candidates_exceed_batch_size() {
        let dir = tempfile::tempdir().unwrap();
        let db_url = dir.path().join("durable.db").to_string_lossy().into_owned();

        let owner = LocalBackend::open(&db_url, 1_048_576).await.unwrap();
        owner.init().await.unwrap();
        let sweeper = LocalBackend::open(&db_url, 1_048_576).await.unwrap();

        let batch_size = 2u64;
        let candidate_count = batch_size * 2 + 1; // spans at least three batches, all lock-held
        let mut locks = Vec::new();
        for _ in 0..candidate_count {
            let exec = ExecutionId::new();
            let (_, lock) = owner
                .open_execution_exclusive(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap();
            backdate_updated_at(&owner, exec, 0).await;
            locks.push(lock); // held for the whole test — every candidate stays lock-held
        }

        let policy = RetentionPolicy {
            stale_running_after_secs: 1,
            prune_batch_size: batch_size,
            ..RetentionPolicy::default()
        };

        let aborted = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            sweeper.sweep_orphans(&policy),
        )
        .await
        .expect(
            "sweep_orphans must terminate even when lock-held candidates exceed prune_batch_size \
             (#6254 C1) — it hung instead of returning",
        )
        .unwrap();

        assert_eq!(aborted, 0, "every candidate's lock is held by a live owner");
        drop(locks);
    }

    /// INV-17: the sweep's guarded abort `UPDATE` runs only while holding the same non-reentrant
    /// flock a concurrent `open_execution_exclusive` reopen for the same execution id requires, so
    /// the two can never both mutate the row at once. Drives them as genuinely concurrent tasks
    /// against a real multi-connection pool (file-backed — `:memory:` forces a single connection,
    /// which would serialize the two calls trivially and prove nothing) across many trials so both
    /// orderings ("sweep acquires the lock first" and "reopen acquires the lock first") are
    /// exercised without artificial delay injection, mirroring the #6251
    /// `concurrent_prune_and_reopen_never_lose_or_corrupt_the_row` pattern above.
    #[tokio::test]
    async fn concurrent_sweep_and_reopen_race_never_corrupts_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let db_url = dir.path().join("durable.db").to_string_lossy().into_owned();
        let backend = Arc::new(LocalBackend::open(&db_url, 1_048_576).await.unwrap());
        backend.init().await.unwrap();

        let policy = RetentionPolicy {
            stale_running_after_secs: 1,
            prune_batch_size: 10,
            ..RetentionPolicy::default()
        };

        for _ in 0..20 {
            let exec = ExecutionId::new();
            backend
                .open_execution(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap();
            backdate_updated_at(&backend, exec, 0).await;

            let sweep_backend = backend.clone();
            let policy_for_task = policy.clone();
            let sweep =
                tokio::spawn(async move { sweep_backend.sweep_orphans(&policy_for_task).await });

            let reopen_backend = backend.clone();
            let reopen = tokio::spawn(async move {
                reopen_backend
                    .open_execution_exclusive(exec, ExecutionKind::AgentTurn)
                    .await
            });

            let (sweep_result, reopen_result) = tokio::join!(sweep, reopen);
            let aborted = sweep_result
                .expect("sweep task must not panic")
                .expect("sweep must not error under a concurrent reopen");
            assert!(aborted <= 1, "at most one candidate row exists per trial");

            match reopen_result.expect("reopen task must not panic") {
                Ok((_is_resume, _lock)) => {
                    // reopen won the race for the lock (either before the sweep even tried, or
                    // after the sweep aborted the row and released) — the row must be `running`
                    // with `finalized_at` cleared either way (INV-16 un-finalizes `aborted` too).
                    let (status, finalized): (String, Option<i64>) = zeph_db::query_as(sql!(
                        "SELECT status, finalized_at FROM durable_executions WHERE execution_id = ?"
                    ))
                    .bind(exec.as_uuid().to_string())
                    .fetch_one(backend.pool())
                    .await
                    .unwrap();
                    assert_eq!(status, "running");
                    assert!(finalized.is_none());
                    // Finalize before the next trial: when reopen wins because the row was
                    // already `running` (not terminal) at the time it checked, `open_execution`'s
                    // existing-row branch never bumps `updated_at` — left alone, this row would
                    // stay a stale `running` candidate forever and pollute a later trial's
                    // `aborted` count (the `assert!(aborted <= 1, ...)` above would then see more
                    // than this trial's own row). Each trial must start with a clean slate of
                    // exactly its own candidate.
                    backend
                        .finalize(exec, ExecutionStatus::Completed)
                        .await
                        .unwrap();
                }
                Err(DurableError::ExecutionLocked { .. }) => {
                    // The sweep held the lock at the moment reopen tried — expected under the race.
                }
                Err(e) => panic!(
                    "reopen must only ever fail with ExecutionLocked under this race, got {e:?}"
                ),
            }
        }
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
