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
//! [`DurableConfig::encryption_gate`](crate::DurableConfig::encryption_gate) at startup.
//!
//! # Scope
//!
//! This revision journals the step-execution entries — [`EntryKind::StepResult`] and
//! [`EntryKind::EffectIntent`] — that the durable step primitive records, plus execution
//! lifecycle (open and [`finalize`](Journal::finalize)) and the writer's restart anchor
//! (`max_seq`). Promise, timer, and checkpoint entries are journaled by the
//! promise/timer and retention layers; until then [`append`](Journal::append) of those kinds fails
//! closed with [`DurableError::UnsupportedEntryKind`] rather than dropping their state. The
//! retention sweep ([`prune`](Journal::prune)) is a no-op stub here.

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use zeph_db::{DbPool, sql};

use crate::backend::{BackendCapabilities, ExecutionBackend};
use crate::cipher::{EntryKindTag, PayloadAad, PayloadCipher, ensure_payload_within_limit};
use crate::config::RetentionPolicy;
use crate::error::DurableError;
use crate::ids::{ExecutionId, ExecutionKind, IdempotencyKey, JournalSeq, StepId};
use crate::journal::{EntryKind, ExecutionStatus, Journal, JournalEntry};
use tracing::Instrument as _;

/// Slack added to `max_payload_bytes` for the read-side size guard.
///
/// The stored blob carries AEAD framing (key-id, extended nonce, tag) on top of the plaintext, so a
/// payload accepted at exactly the limit on write is slightly larger on read. The guard exists only
/// to reject absurdly large rows before allocation/decryption (INV-11), so a small fixed slack
/// above any real AEAD overhead keeps legitimate near-limit entries readable without weakening the
/// denial-of-service protection.
const SEAL_OVERHEAD_SLACK: u64 = 128;

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
            max_connections: 5,
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
        // Evaluate the dialect-rewritten statement once (the postgres `sql!` leaks on each call),
        // then reuse it for every row in the batch.
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

    async fn prune(&self, _policy: &RetentionPolicy) -> Result<u64, DurableError> {
        // The retention sweep lands with the compaction layer; this stub keeps the trait total and
        // the span present for the trace-analysis loop.
        let _span = tracing::info_span!("durable.journal.prune", deleted_count = 0u64).entered();
        Ok(0)
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

/// Current Unix time in milliseconds, clamped into `i64` and never panicking.
fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Decode a stored blob into a fixed 32-byte array, failing closed on the wrong length.
fn slice_to_array32(bytes: &[u8], field: &'static str) -> Result<[u8; 32], DurableError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| DurableError::Decode { context: field })
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
#[cfg(all(test, feature = "sqlite", not(feature = "postgres")))]
mod tests {
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
        assert!(matches!(
            backend.append(timer).await,
            Err(DurableError::UnsupportedEntryKind {
                kind: "timer_armed"
            })
        ));
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
        assert!(matches!(
            backend.append(step_result(exec, 0, &big)).await,
            Err(DurableError::PayloadTooLarge { .. })
        ));
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
}
