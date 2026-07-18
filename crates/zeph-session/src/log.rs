// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The append-only JSONL event log: [`SessionEventLog`].
//!
//! Mirrors the append + fsync pattern of `zeph-durable`'s `JournalWriter`
//! (`crates/zeph-durable/src/writer.rs`) at the conversation-semantics level, but persists to a
//! plain JSONL file rather than a `SQLite`-backed journal (spec-068 §3, §14).
//!
//! # Invariants
//!
//! - INV-SP-1 (log-first ordering): callers must append to this log before updating any
//!   downstream projection (`SQLite` `messages`, `acp_sessions.last_seq`).
//! - INV-SP-2 (torn-append truncation): every read validates each line and drops a garbled/
//!   incomplete trailing line from the in-memory result, which can only occur as the very last
//!   line because appends are serialized through a single writer (INV-D2). Only
//!   [`SessionEventLog::open_exclusive`] additionally repairs the torn tail physically on disk —
//!   a lockless [`SessionEventLog::open`]/[`SessionEventLog::read_all`] cannot prove a "torn"
//!   line isn't a live writer's in-flight, not-yet-fsynced append, so it must never mutate the
//!   file (#5487 Finding B).
//! - INV-D2 (single writer): only the session's owning actor/agent process may hold a
//!   `SessionEventLog` for a given session directory at a time. [`SessionEventLog::open`]
//!   does not itself enforce cross-process exclusion — it is also used by read-only
//!   tooling (session export/inspection) that may legitimately run alongside a live
//!   writer. The session's owning actor/agent process should instead use
//!   [`SessionEventLog::open_exclusive`], which takes a non-blocking `flock(2)` advisory
//!   lock (Unix only) and fails with [`SessionError::AlreadyLocked`] if another writer
//!   already holds the session directory.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};

use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use zeph_common::anchor::{Anchor, AnchorStore, AnchorSubsystem};
use zeph_common::hash_chain::{
    ChainError, ChainHash, ChainKeyRing, ChainStreamVerifier, KeyResolution, chain_next, genesis,
};

use crate::error::SessionError;
use crate::event::{SessionEvent, SessionEventEnvelope};

const EVENTS_FILE_NAME: &str = "events.jsonl";
#[cfg(unix)]
const LOCK_FILE_NAME: &str = "events.jsonl.lock";

/// Domain-separation tag for this subsystem's hash chain (issue #6360) — distinct from
/// `zeph-subagent`'s so a chain from one subsystem can never verify against the other.
pub const CHAIN_DOMAIN: &str = "zeph-session log v1";

/// Process-wide history-chain key ring, configured once at bootstrap by resolving
/// `ZEPH_HISTORY_KEY` from the vault (see `zeph_core::history_integrity`).
///
/// See `zeph_subagent::transcript`'s identical registry for the full rationale (a `RwLock`, not
/// `OnceLock`, so tests can reconfigure it, and process-global rather than a constructor
/// parameter because `SessionEventLog::open`/`open_exclusive` have 40+ call sites across crates
/// outside this feature's ownership).
static HISTORY_INTEGRITY: StdRwLock<Option<Arc<ChainKeyRing>>> = StdRwLock::new(None);

/// Configure (or disable, with `None`) history-chain verification for every
/// [`SessionEventLog`] operation in this process from this point forward. See
/// `zeph_subagent::transcript::configure_history_integrity`'s doc for the full contract — this
/// mirrors it exactly.
///
/// # Invariant: single-set-at-startup
///
/// This is `pub` (not `pub(crate)`) specifically so `src/runner.rs` — a different crate from
/// this one — can call it once during CLI bootstrap, before any `SessionEventLog` is opened
/// (see `configure_history_integrity_from_default_vault` in `src/runner.rs`). It is **not**
/// meant to be called again later by production code: reconfiguring mid-process cannot make an
/// already-open handle less safe (each handle captures `ring` at construction and is immune to
/// later reconfiguration, and setting `ring = None` only ever makes *subsequent* opens
/// fail-closed, never trust-bypassing), but a caller reconfiguring after bootstrap without a
/// clear reason is almost certainly a bug, not an intended feature — no production code path
/// does this today, and none should be added without updating this doc. Tests are the one
/// legitimate exception, calling this per-test under `cargo nextest`'s one-process-per-test
/// isolation.
pub fn configure_history_integrity(ring: Option<Arc<ChainKeyRing>>) {
    if let Ok(mut guard) = HISTORY_INTEGRITY.write() {
        *guard = ring;
    }
}

fn history_integrity() -> Option<Arc<ChainKeyRing>> {
    HISTORY_INTEGRITY.read().ok().and_then(|g| g.clone())
}

/// Process-wide vault-anchor store (issue #6449). See
/// `zeph_subagent::transcript::configure_anchor_store`'s identical registry for the full
/// rationale — this mirrors it exactly. `None` (the default) disables anchor writes/checks
/// entirely: sessions behave exactly as they did under #6453.
static ANCHOR_STORE: StdRwLock<Option<Arc<dyn AnchorStore>>> = StdRwLock::new(None);

/// Configure (or disable, with `None`) the vault-anchor store for every [`SessionEventLog`]
/// operation in this process from this point forward.
pub fn configure_anchor_store(store: Option<Arc<dyn AnchorStore>>) {
    if let Ok(mut guard) = ANCHOR_STORE.write() {
        *guard = store;
    }
}

fn anchor_store() -> Option<Arc<dyn AnchorStore>> {
    ANCHOR_STORE.read().ok().and_then(|g| g.clone())
}

/// Chunk size for [`SessionEventLog::read_chunked`] (spec §6.2 step 3: "bounded buffer, ≤ 100
/// events in memory at once").
const REPLAY_CHUNK_SIZE: usize = 100;

/// Bound on the single async vault-anchor `get` performed at open time (issue #6449). A vault
/// stall must fail deterministically rather than hang an unattended caller (durable resume,
/// scheduler restore, ACP resume, fork pre-copy) — this timeout applies uniformly regardless of
/// caller, since `open`/`open_exclusive` cannot distinguish attended from unattended callers
/// itself.
const ANCHOR_GET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Append-only JSONL log for one conversation-session's `events.jsonl`.
///
/// # Examples
///
/// ```
/// use tempfile::tempdir;
/// use zeph_session::event::SessionEvent;
/// use zeph_session::log::SessionEventLog;
///
/// # #[tokio::main]
/// # async fn main() {
/// let dir = tempdir().unwrap();
/// let log = SessionEventLog::open(dir.path()).await.unwrap();
/// log.append(None, None, SessionEvent::SessionEnded { reason: "user_quit".to_owned() })
///     .await
///     .unwrap();
/// assert_eq!(log.last_seq(), Some(0));
/// # }
/// ```
struct SessionWriteState {
    file: File,
    /// Running chain head; `None` until either the first chained append in this handle's
    /// lifetime (fresh chaining start on a legacy or empty log) or seeded from the log's
    /// existing chained tail at open time (M3).
    prev: Option<ChainHash>,
    /// Total on-disk event count (seeded from any pre-existing content at open time,
    /// incremented on every successful append) — the `count` half of the vault anchor written
    /// by [`SessionEventLog::finalize`] (issue #6449).
    count: u64,
}

pub struct SessionEventLog {
    events_path: PathBuf,
    /// `file` and the running chain state share one lock so the chain-link read-modify-write is
    /// always atomic with the physical write and `sync_all` (S2, issue #6360 critic rev2) —
    /// matches `seq` assignment, which already had to be under this same lock for INV-SP-2's
    /// ascending-seq-order guarantee (#5487); folding the chain link in adds no new await inside
    /// the guarded section (BLAKE3 is CPU-only).
    writer: Mutex<SessionWriteState>,
    next_seq: AtomicU64,
    file_identity: Vec<u8>,
    /// Captured once at open time so every `append` on this handle uses one consistent key
    /// ring, even if `configure_history_integrity` is called again concurrently.
    ring: Option<Arc<ChainKeyRing>>,
    /// Set only by [`SessionEventLog::open_exclusive_allow_unverified`] — every subsequent
    /// [`Self::read_all`]/[`Self::read_chunked`] call on this handle also skips chain
    /// verification, not just the initial open, so the deliberate operator override applies
    /// for this handle's whole lifetime rather than just its construction.
    allow_unverified: bool,
    /// Captured once at open time, like `ring` (issue #6449) — `None` if no anchor store is
    /// configured, or none is on file for this session yet.
    anchor: Option<Anchor>,
    #[allow(dead_code)] // held only for its Drop (releases the flock, if taken)
    lock: Option<AdvisoryLock>,
}

/// Derive a session log's chain identity from its directory (the `session_id`) — binds the
/// chain to this one session so a whole-log substitution (swapping in another session's
/// `events.jsonl`) breaks at the genesis hash.
fn file_identity(session_dir: &Path) -> Vec<u8> {
    session_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .into_bytes()
}

impl SessionEventLog {
    /// Open (creating if absent) the `events.jsonl` log under `session_dir`.
    ///
    /// Validates the existing file per INV-SP-2, dropping a torn trailing line from the
    /// in-memory result, then opens the file in append mode for subsequent writes. Sets
    /// file/directory permissions to `0o700`/`0o600` on Unix (spec §4.1); a no-op on other
    /// platforms.
    ///
    /// Does not take the cross-process advisory lock, and never physically truncates the file
    /// (even if a torn tail is found) — safe for read-only tooling that may run alongside a live
    /// writer whose in-flight, not-yet-fsynced line could otherwise be mistaken for "torn" and
    /// destroyed (#5487 Finding B). The session's owning actor/agent process should use
    /// [`Self::open_exclusive`] instead, which does perform the physical repair.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Io`] if the directory or file cannot be created, or
    /// [`SessionError::Serde`] surfaces only via [`Self::read_all`], never here (torn lines are
    /// discarded, not treated as fatal).
    pub async fn open(session_dir: &Path) -> Result<Self, SessionError> {
        Self::open_with_lock(session_dir, None, false).await
    }

    /// Open the `events.jsonl` log under `session_dir` like [`Self::open`], but **skip
    /// hash-chain verification** for this handle's whole lifetime — see
    /// [`Self::open_exclusive_allow_unverified`]'s doc for the full contract (this is its
    /// lockless counterpart, for read-only tooling such as `sessions resume --print
    /// --allow-unverified`).
    ///
    /// # Errors
    ///
    /// Returns any error [`Self::open`] can return other than [`SessionError::Integrity`]
    /// (which this method exists specifically to bypass).
    pub async fn open_allow_unverified(session_dir: &Path) -> Result<Self, SessionError> {
        Self::open_with_lock(session_dir, None, true).await
    }

    /// Open the `events.jsonl` log under `session_dir` like [`Self::open`], but additionally
    /// take a non-blocking, exclusive advisory lock (`flock(2)` on Unix, mirroring
    /// `zeph-scheduler`'s `PidFile`) enforcing INV-D2's single-writer invariant.
    ///
    /// Intended for the session's owning actor/agent process. On non-Unix targets the lock
    /// is a no-op (the workspace has no vetted cross-platform advisory-locking primitive), so
    /// this degrades to [`Self::open`]'s behavior there.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::AlreadyLocked`] if another process already holds the session's
    /// write lock, or any error [`Self::open`] can return.
    pub async fn open_exclusive(session_dir: &Path) -> Result<Self, SessionError> {
        fs::create_dir_all(session_dir).await?;
        let lock = AdvisoryLock::acquire(session_dir)?;
        Self::open_with_lock(session_dir, Some(lock), false).await
    }

    /// Open the `events.jsonl` log under `session_dir` like [`Self::open_exclusive`], but
    /// **skip hash-chain verification** for this one open.
    ///
    /// This is the deliberate, logged override an operator invokes explicitly (e.g. `zeph
    /// sessions resume <id> --allow-unverified`) after being shown a detected chain-integrity
    /// failure — never a silent fallback. Per spec-069 FR-004's fail-closed-by-default posture:
    /// callers on an **unattended** path (durable resume, the crash-orphan sweep, automatic
    /// sub-agent transcript reload) must never call this — only a human-attended path with an
    /// explicit, deliberate opt-in may bypass verification. `read_all`/`read_chunked` on the
    /// returned handle also skip chain verification (a dedicated `allow_unverified` flag carried
    /// on `Self`, threaded through every subsequent read — **not** implemented by nulling the
    /// key ring, which would instead re-trigger the normal "no key configured" fail-closed path
    /// and make this override indistinguishable from a plain hard failure), so the whole session
    /// is treated as best-effort-trusted, matching the legacy posture, for as long as this
    /// handle is held.
    ///
    /// **Scope of the bypass**: this skips cryptographic chain verification only. It does
    /// **not** bypass the structural torn-tail/internal-malformed-line check (S1, the private
    /// `peek_confirms_trailing_torn` helper) — a line that fails to parse as JSON is still a
    /// hard error even with this override, since that is a distinct failure class (structural
    /// corruption, not a cryptographic tamper verdict) that this override was never meant to
    /// paper over. An operator with a genuinely corrupt (non-tamper) internal-malformed-line
    /// session cannot recover it via `--allow-unverified`.
    ///
    /// # Errors
    ///
    /// Returns any error [`Self::open_exclusive`] can return other than
    /// [`SessionError::Integrity`] (which this method exists specifically to bypass).
    pub async fn open_exclusive_allow_unverified(session_dir: &Path) -> Result<Self, SessionError> {
        fs::create_dir_all(session_dir).await?;
        let lock = AdvisoryLock::acquire(session_dir)?;
        Self::open_with_lock(session_dir, Some(lock), true).await
    }

    async fn open_with_lock(
        session_dir: &Path,
        lock: Option<AdvisoryLock>,
        allow_unverified: bool,
    ) -> Result<Self, SessionError> {
        fs::create_dir_all(session_dir).await?;
        set_permissions(session_dir, 0o700).await?;

        let events_path = session_dir.join(EVENTS_FILE_NAME);
        let ring = history_integrity();
        let identity = file_identity(session_dir);

        // Resolve the vault anchor once, bounded by a timeout (issue #6449) so a vault stall
        // fails deterministically rather than hanging an unattended caller (durable resume,
        // scheduler restore, ACP resume, fork pre-copy — none of which can offer an interactive
        // retry).
        let anchor = match anchor_store() {
            Some(store) => tokio::time::timeout(
                ANCHOR_GET_TIMEOUT,
                store.get(AnchorSubsystem::SessionLog, &identity),
            )
            .await
            .map_err(|_| {
                SessionError::Integrity(format!(
                    "vault anchor lookup for session '{}' timed out after {:?} — failing \
                         closed rather than opening unverified",
                    session_dir.display(),
                    ANCHOR_GET_TIMEOUT
                ))
            })?
            .map_err(|e| SessionError::Integrity(format!("anchor lookup failed: {e}")))?,
            None => None,
        };

        // Only the exclusive-lock holder may physically repair a torn tail (see
        // `read_events`'s doc comment) — a lockless `open()` cannot prove the "torn" line
        // isn't a live writer's in-flight, not-yet-fsynced append. Chain verification (S1)
        // always runs regardless of lock status — only the physical *repair* is gated, never
        // the integrity check itself; a failed check here means `open`/`open_exclusive` fails
        // outright rather than opening atop unverified content (M3 open-time tail verify/seed)
        // — unless `allow_unverified` is set (the deliberate `--allow-unverified` operator
        // override), in which case verification is skipped entirely for this open, distinct
        // from `ring = None` (which still fail-closes a chained file per NFR-004).
        let (_, max_seq, chain_head) = read_events(
            &events_path,
            lock.is_some(),
            ring.as_deref(),
            &identity,
            allow_unverified,
            anchor.as_ref(),
        )
        .await?;

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events_path)
            .await?;
        set_permissions(&events_path, 0o600).await?;

        let next_seq = max_seq.map_or(0, |seq| seq + 1);
        let count = max_seq.map_or(0, |seq| seq + 1);
        Ok(Self {
            events_path,
            writer: Mutex::new(SessionWriteState {
                file,
                prev: chain_head,
                count,
            }),
            next_seq: AtomicU64::new(next_seq),
            file_identity: identity,
            ring,
            allow_unverified,
            anchor,
            lock,
        })
    }

    /// The path to this session's `events.jsonl` file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.events_path
    }

    /// The highest `seq` durably appended so far, or `None` if the log is empty.
    #[must_use]
    pub fn last_seq(&self) -> Option<u64> {
        let next = self.next_seq.load(Ordering::SeqCst);
        next.checked_sub(1)
    }

    /// Append one event, assigning it the next monotonic `seq`, and `fsync` before returning.
    ///
    /// The single `write_all` + `sync_all` pair is the atomicity boundary INV-SP-2 relies on: a
    /// crash mid-write can only ever corrupt this one trailing line.
    ///
    /// When history-chain verification is configured, the chain-link read-modify-write
    /// (canonicalize with `chain: None`, hash, then serialize again with the computed hash) is
    /// folded into the same critical section as `seq` assignment and the physical write/fsync
    /// (S2) — on-disk order always matches chain order, exactly as it already had to for `seq`
    /// (#5487).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Serde`] if the event cannot be JSON-encoded, or
    /// [`SessionError::Io`] if the write or fsync fails.
    #[tracing::instrument(name = "session.log.append", skip_all, level = "debug")]
    pub async fn append(
        &self,
        turn_id: Option<u64>,
        parent_seq: Option<u64>,
        kind: SessionEvent,
    ) -> Result<SessionEventEnvelope, SessionError> {
        let mut state = self.writer.lock().await;

        // seq assignment MUST happen while holding the writer lock: two concurrent
        // callers assigned seq N and N+1 before the lock could still race for the
        // lock and land their physical writes in the opposite order, breaking
        // INV-SP-2's ascending-seq-order assumption (#5487).
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let mut envelope = SessionEventEnvelope::new(seq, turn_id, parent_seq, kind);

        let new_head = if let Some(ring) = self.ring.as_deref() {
            let content = serde_json::to_vec(&envelope)?;
            let base = state.prev.unwrap_or_else(|| {
                genesis(
                    &ring.current_key(),
                    CHAIN_DOMAIN,
                    &self.file_identity,
                    ring.current_epoch(),
                )
            });
            let h = chain_next(&ring.current_key(), &base, &content);
            envelope.chain = Some(h.to_hex());
            Some(h)
        } else {
            None
        };

        let mut line = serde_json::to_vec(&envelope)?;
        line.push(b'\n');

        state.file.write_all(&line).await?;
        state.file.sync_all().await?;

        // Only advance the running chain state after the write+fsync succeeded — a failed
        // write must not desynchronize `prev` from what is actually durable on disk.
        if let Some(h) = new_head {
            state.prev = Some(h);
        }
        state.count += 1;

        Ok(envelope)
    }

    /// Finalize this handle: if a vault-anchor store is configured (issue #6449) and this
    /// handle's lifetime saw at least one chained append, persist an [`Anchor`] recording the
    /// current `(epoch, count, head)` — a *prefix commitment* as of this clean close, not a
    /// guarantee against every possible future truncation (see the module docs' session prefix
    /// residual note).
    ///
    /// Written **last**, after every append is durably fsynced, so a crash before this point
    /// leaves the log present with no anchor, which is always benign (never a false tamper
    /// signature).
    ///
    /// A no-op, not an error, when no anchor store is configured or this handle never chained.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Integrity`] if the configured anchor store's `put` fails. Callers
    /// should treat this as best-effort and log rather than fail the whole close/shutdown flow —
    /// the session log itself is already safely written.
    pub async fn finalize(&self) -> Result<(), SessionError> {
        let Some(store) = anchor_store() else {
            return Ok(());
        };
        let (head, count) = {
            let state = self.writer.lock().await;
            let Some(head) = state.prev else {
                return Ok(());
            };
            (head, state.count)
        };
        let epoch = self.ring.as_ref().map_or(0, |r| r.current_epoch());
        let anchor = Anchor::new(epoch, count, head);
        store
            .put(AnchorSubsystem::SessionLog, &self.file_identity, anchor)
            .await
            .map_err(|e| SessionError::Integrity(format!("anchor put failed: {e}")))
    }

    /// Read and validate every event currently in the log, dropping a torn trailing line from
    /// the result (INV-SP-2). Only physically repairs the file if this handle was opened via
    /// [`Self::open_exclusive`] — see that method's doc comment.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Io`] if the file cannot be read, or [`SessionError::Integrity`]
    /// if hash-chain verification fails (S1: this check always runs before any torn-tail
    /// repair).
    #[tracing::instrument(name = "session.log.read_all", skip_all, level = "debug")]
    pub async fn read_all(&self) -> Result<Vec<SessionEventEnvelope>, SessionError> {
        // Same repair gating as `open_with_lock`: only repair the physical file when this
        // handle holds the exclusive lock (i.e. is the session's owning writer). A read-only
        // handle (`open()`) calling `read_all()` — e.g. `sessions show --events`, the ACP HTTP
        // inspection endpoint — must never truncate a live writer's in-flight tail out from
        // under it (#5487 Finding B).
        let (events, _, _) = read_events(
            &self.events_path,
            self.lock.is_some(),
            self.ring.as_deref(),
            &self.file_identity,
            self.allow_unverified,
            self.anchor.as_ref(),
        )
        .await?;
        Ok(events)
    }

    /// Read this log's events in bounded chunks of at most [`REPLAY_CHUNK_SIZE`], invoking
    /// `on_chunk` per chunk instead of materializing the whole file's parsed events into one
    /// `Vec` the way [`Self::read_all`] does (spec §6.2 step 3). Used by
    /// [`crate::replay::ReplayEngine::replay`] to keep peak memory bounded when replaying large
    /// session logs.
    ///
    /// `on_chunk` returns [`ControlFlow::Break`] to stop reading early (e.g. once a replay
    /// `up_to` bound is reached) — remaining lines, including any torn tail beyond the stop
    /// point, are then left uninspected.
    ///
    /// Note the over-read this implies: when `up_to` falls inside a chunk still being
    /// accumulated, that entire chunk (up to [`REPLAY_CHUNK_SIZE`] events) is read and parsed
    /// from disk before `on_chunk` gets a chance to evaluate the break — this never exceeds the
    /// ≤ [`REPLAY_CHUNK_SIZE`]-in-memory bound, but a future refactor must not assume the read
    /// stops the instant the `up_to` seq is reached.
    ///
    /// Same torn-tail detection/repair gating as [`Self::read_all`]: only physically repairs
    /// the file when this handle was opened via [`Self::open_exclusive`]. Chain verification
    /// (S1) runs incrementally as each event is parsed — before it is ever handed to
    /// `on_chunk` — so a tampered event is never exposed to the caller even transiently, and
    /// the bounded-memory guarantee this method exists for is preserved (verification state is
    /// O(1): at most two in-flight [`zeph_common::hash_chain::ChainStreamVerifier`] candidates
    /// until the key epoch resolves, then one).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Io`] if the file cannot be read, or [`SessionError::Integrity`]
    /// if hash-chain verification fails.
    #[tracing::instrument(name = "session.log.read_chunked", skip_all, level = "debug")]
    pub(crate) async fn read_chunked(
        &self,
        on_chunk: impl FnMut(Vec<SessionEventEnvelope>) -> ControlFlow<()>,
    ) -> Result<(), SessionError> {
        read_events_chunked(
            &self.events_path,
            self.lock.is_some(),
            self.ring.as_deref(),
            &self.file_identity,
            self.allow_unverified,
            self.anchor.as_ref(),
            on_chunk,
        )
        .await
    }
}

/// Incremental chain-tracking state shared by [`read_events`]'s whole-file loop and
/// [`read_events_chunked`]'s bounded-chunk loop, so both apply identical legacy-prefix /
/// partial-strip / verification logic to each event as it is parsed (S1: this runs strictly
/// before any torn-tail repair in both callers, and strictly before an event is exposed to a
/// `read_chunked` caller via `on_chunk`).
struct SessionChainTracker<'a> {
    path: &'a Path,
    ring: Option<&'a ChainKeyRing>,
    file_identity: &'a [u8],
    verifier: Option<ChainStreamVerifier>,
    chain_started: bool,
    /// Set only by [`SessionEventLog::open_exclusive_allow_unverified`]'s deliberate operator
    /// override (spec-069 FR-004): when `true`, [`Self::feed`] is a no-op for every event,
    /// chained or not — this is NOT the same as `ring = None` (which still fail-closes a
    /// chained file per NFR-004; the override must actually bypass verification, not just
    /// simulate a missing key, or `--allow-unverified` would be indistinguishable from a
    /// plain hard failure).
    allow_unverified: bool,
    /// Vault anchor for this session, if configured and present (issue #6449). Also bypassed
    /// entirely when `allow_unverified` is set, consistent with that override treating the
    /// whole session as best-effort-trusted.
    anchor: Option<&'a Anchor>,
    /// Total physical event count fed so far (including any legacy prefix) — used to locate the
    /// entry at `anchor.count` and capture the chain head immediately after it.
    physical_index: u64,
    /// The chain head immediately after the `anchor.count`-th event was fed, if reached.
    anchor_checkpoint_head: Option<ChainHash>,
}

impl<'a> SessionChainTracker<'a> {
    fn new(
        path: &'a Path,
        ring: Option<&'a ChainKeyRing>,
        file_identity: &'a [u8],
        allow_unverified: bool,
        anchor: Option<&'a Anchor>,
    ) -> Self {
        Self {
            path,
            ring,
            file_identity,
            verifier: None,
            chain_started: false,
            allow_unverified,
            anchor,
            physical_index: 0,
            anchor_checkpoint_head: None,
        }
    }

    /// Feed one parsed event in on-disk order. Must be called for every event, in order,
    /// including the legacy prefix (a no-op for those).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Integrity`] on a partial strip (a `chain`-less event after
    /// chaining has started), a chained log with no key ring configured (NFR-004), or a
    /// definite/ambiguous chain-verification failure. Always `Ok` when this tracker was
    /// constructed with `allow_unverified = true`.
    fn feed(&mut self, event: &SessionEventEnvelope) -> Result<(), SessionError> {
        if self.allow_unverified {
            return Ok(());
        }
        let Some(hex) = event.chain.as_deref() else {
            return if self.chain_started {
                Err(SessionError::Integrity(format!(
                    "session log '{}' has an event missing its chain field while earlier \
                     events in this log are chained — partial strip detected, TAMPER DETECTED",
                    self.path.display()
                )))
            } else {
                // legacy prefix, no-op — but still advance the physical index (issue #6449:
                // the anchor's `count` is a total physical count including any legacy prefix).
                self.physical_index += 1;
                Ok(())
            };
        };
        self.chain_started = true;

        let stored = ChainHash::from_hex(hex).map_err(|_| {
            SessionError::Integrity(format!(
                "session log '{}' has a malformed chain hash",
                self.path.display()
            ))
        })?;

        if self.verifier.is_none() {
            let ring = self.ring.ok_or_else(|| {
                SessionError::Integrity(format!(
                    "session log '{}' carries chain metadata but no history-integrity key is \
                     configured for this process — refusing to trust it unverified (NFR-004)",
                    self.path.display()
                ))
            })?;
            self.verifier = Some(ChainStreamVerifier::new(
                ring,
                CHAIN_DOMAIN,
                self.file_identity.to_vec(),
            ));
        }

        let mut stripped = event.clone();
        stripped.chain = None;
        let content = serde_json::to_vec(&stripped)?;
        // `verifier` was just ensured `Some` above.
        self.verifier
            .as_mut()
            .expect("verifier initialized above")
            .verify_next(&content, &stored)
            .map_err(|e| describe_chain_error(self.path, &e))?;

        self.physical_index += 1;
        if let Some(anchor) = self.anchor
            && self.physical_index == anchor.count
        {
            self.anchor_checkpoint_head =
                self.verifier.as_ref().and_then(ChainStreamVerifier::head);
        }
        Ok(())
    }

    /// Finalize: logs a re-keyed note if applicable, enforces the anchor decision table (issue
    /// #6449), and returns the verified head hash (`None` if the log was pure legacy — chaining
    /// never started).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Integrity`] if an anchor is configured and: this log is
    /// legacy-looking (no chain field ever fed) despite the anchor existing — a whole-strip
    /// downgrade signature; the on-disk event count is below the anchor's recorded count
    /// (truncation); or the chain head at the anchor's recorded count disagrees with the stored
    /// anchor. A no-op when `allow_unverified` was set (mirrors [`Self::feed`]'s bypass).
    fn finish(self) -> Result<Option<ChainHash>, SessionError> {
        if self.allow_unverified {
            return Ok(None);
        }
        if let Some(KeyResolution::Rekeyed(epoch)) = self
            .verifier
            .as_ref()
            .and_then(ChainStreamVerifier::resolution)
        {
            tracing::info!(
                path = %self.path.display(),
                epoch,
                "session log verified under a previous key epoch (re-keyed, not tampered)"
            );
        }
        // Pure legacy (chaining never started) while a key IS configured is anomalous: every
        // legitimately-written log since this process started should carry a chain field.
        // Auto-trusted per FR-006 (unless an anchor proves otherwise, checked below), but must
        // be observable, not silent (security review B2 condition, NFR-005).
        if !self.chain_started && self.ring.is_some() {
            warn_legacy_under_active_key_once(self.path);
        }

        if let Some(anchor) = self.anchor {
            if !self.chain_started {
                // Legacy-looking (no chain field anywhere), but a vault anchor exists for this
                // session's identity: a file-write-only attacker cannot delete a vault entry, so
                // this can only mean every chain field was deliberately stripped.
                tracing::error!(
                    audit_event = "history_integrity_tamper",
                    subsystem = "session_log",
                    reason = "whole_strip_legacy_with_anchor",
                    path = %self.path.display(),
                    anchored_count = anchor.count,
                    "TAMPER DETECTED: session log is legacy-looking but a vault anchor exists for \
                     it (issue #6449)"
                );
                return Err(SessionError::Integrity(format!(
                    "TAMPER DETECTED in session log '{}': log has no chain metadata \
                     (legacy-looking) but a vault anchor exists for it (anchored at count={}) — \
                     this log was previously chained and its chain fields have been stripped",
                    self.path.display(),
                    anchor.count
                )));
            }
            if self.physical_index < anchor.count {
                tracing::error!(
                    audit_event = "history_integrity_tamper",
                    subsystem = "session_log",
                    reason = "truncated_below_anchor_count",
                    path = %self.path.display(),
                    on_disk_count = self.physical_index,
                    anchored_count = anchor.count,
                    "TAMPER DETECTED: session log truncated below its anchored count (issue #6449)"
                );
                return Err(SessionError::Integrity(format!(
                    "TAMPER DETECTED in session log '{}': on-disk event count ({}) is below the \
                     anchored count ({}) — the log was truncated after being anchored",
                    self.path.display(),
                    self.physical_index,
                    anchor.count
                )));
            }
            let anchor_head = anchor.head().map_err(|e| {
                SessionError::Integrity(format!(
                    "session log '{}' anchor is malformed: {e}",
                    self.path.display()
                ))
            })?;
            match self.anchor_checkpoint_head {
                Some(h) if h == anchor_head => {}
                _ => {
                    tracing::error!(
                        audit_event = "history_integrity_tamper",
                        subsystem = "session_log",
                        reason = "anchor_head_mismatch",
                        path = %self.path.display(),
                        anchored_count = anchor.count,
                        "TAMPER DETECTED: session log chain head at the anchored count does not \
                         match the stored vault anchor (issue #6449)"
                    );
                    return Err(SessionError::Integrity(format!(
                        "TAMPER DETECTED in session log '{}': chain head at the anchored count \
                         ({}) does not match the stored vault anchor",
                        self.path.display(),
                        anchor.count
                    )));
                }
            }
        }

        Ok(self.verifier.and_then(|v| v.head()))
    }
}

/// Paths already warned about via [`warn_legacy_under_active_key_once`] this process — kept
/// small (one entry per distinct session path actually read while chaining-disabled, not
/// per-read) so a session's history isn't re-warned every time it's reloaded.
static WARNED_LEGACY_UNDER_KEY: std::sync::LazyLock<StdRwLock<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| StdRwLock::new(std::collections::HashSet::new()));

/// Log a structured `WARN` the first time a given path is found to be pure-legacy (no `chain`
/// field anywhere) while a history-integrity key ring IS configured (issue #6360, security
/// review B2 condition (c)). See `zeph_subagent::transcript`'s identical helper for the full
/// rationale — this mirrors it exactly.
fn warn_legacy_under_active_key_once(path: &Path) {
    let already_warned = WARNED_LEGACY_UNDER_KEY
        .read()
        .is_ok_and(|set| set.contains(path));
    if already_warned {
        return;
    }
    if let Ok(mut set) = WARNED_LEGACY_UNDER_KEY.write()
        && !set.insert(path.to_path_buf())
    {
        return; // another thread warned first between the read and write locks
    }
    tracing::warn!(
        path = %path.display(),
        "history-chain integrity: session log classifies as legacy (no chain field anywhere) \
         while a history-integrity key IS configured for this process — this is expected for \
         genuine pre-upgrade content, but is also the signature of a full chain-strip downgrade \
         attack (issue #6449, the vault-anchor gap); accepted per FR-006, flagged for operator \
         visibility"
    );
}

/// Render a [`ChainError`] as a [`SessionError::Integrity`] with operator-actionable wording
/// that distinguishes a definite tamper verdict from an ambiguous/possibly-re-keyed one (FR-008
/// — an operator must not be misled into believing a re-keyed log was tampered with).
fn describe_chain_error(path: &Path, err: &ChainError) -> SessionError {
    match err {
        ChainError::Unverifiable => SessionError::Integrity(format!(
            "session log '{}' is unverifiable: no known key epoch (current or previous \
             rotation window) produces a valid chain — possibly re-keyed past the rotation \
             window, or tampered; this is fail-closed by design (NFR-004) and cannot be \
             auto-recovered",
            path.display()
        )),
        ChainError::Mismatch { index } => SessionError::Integrity(format!(
            "TAMPER DETECTED in session log '{}': chain hash mismatch at chained-entry index \
             {index} — content was modified, reordered, or deleted after being written",
            path.display()
        )),
        other => SessionError::Integrity(format!(
            "session log '{}' failed chain verification: {other}",
            path.display()
        )),
    }
}

/// The outcome of parsing one physical line from an `events.jsonl` file.
enum LineOutcome {
    /// End of file reached (0 bytes read).
    Eof,
    /// A blank line (allowed, e.g. trailing newline) — no envelope produced.
    Blank,
    /// A well-formed, newline-terminated envelope. Boxed: adding the `chain` field (issue
    /// #6360) grew `SessionEventEnvelope` past clippy's `large_enum_variant` threshold relative
    /// to this enum's other all-unit variants.
    Event(Box<SessionEventEnvelope>),
    /// A garbled or unterminated line — the torn tail (INV-SP-2). Can only be the final line
    /// because appends are serialized through a single writer (INV-D2).
    Torn,
}

/// Line-oriented cursor over an `events.jsonl` file, shared by [`read_events`] (whole-file,
/// `Vec`-accumulating) and [`read_events_chunked`] (bounded-chunk streaming) so both read paths
/// apply identical per-line validation (INV-SP-2).
struct EventLineReader {
    reader: BufReader<File>,
    line: String,
    offset: u64,
    valid_len: u64,
}

impl EventLineReader {
    /// Opens `path`, returning `None` if the file does not exist (an empty/absent log).
    async fn open(path: &Path) -> Result<Option<Self>, SessionError> {
        let file = match File::open(path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(Self {
            reader: BufReader::new(file),
            line: String::new(),
            offset: 0,
            valid_len: 0,
        }))
    }

    async fn next_line(&mut self) -> Result<LineOutcome, SessionError> {
        self.line.clear();
        let bytes_read = self.reader.read_line(&mut self.line).await? as u64;
        if bytes_read == 0 {
            return Ok(LineOutcome::Eof);
        }

        let is_terminated = self.line.ends_with('\n');
        let trimmed = self.line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            self.offset += bytes_read;
            if is_terminated {
                self.valid_len = self.offset;
            }
            return Ok(LineOutcome::Blank);
        }

        match serde_json::from_str::<SessionEventEnvelope>(trimmed) {
            Ok(envelope) if is_terminated => {
                self.offset += bytes_read;
                self.valid_len = self.offset;
                Ok(LineOutcome::Event(Box::new(envelope)))
            }
            _ => Ok(LineOutcome::Torn),
        }
    }
}

/// Physically truncates `path` to `valid_len` if it is shorter than the file's actual length,
/// repairing a torn tail on disk (INV-SP-2). Only called when `repair` gating (see
/// [`SessionEventLog::open_exclusive`]) has already authorized it.
async fn repair_torn_tail(path: &Path, valid_len: u64) -> Result<(), SessionError> {
    let actual_len = fs::metadata(path).await?.len();
    if valid_len < actual_len {
        let file = OpenOptions::new().write(true).open(path).await?;
        file.set_len(valid_len).await?;
    }
    Ok(())
}

/// Shared epilogue for [`read_events`] and [`read_events_chunked`]: warns once if a torn tail was
/// detected (INV-SP-2), then physically repairs it when `repair` gating authorizes it.
async fn finish_torn_tail(
    path: &Path,
    valid_len: u64,
    repair: bool,
    torn: bool,
) -> Result<(), SessionError> {
    if torn {
        tracing::warn!(
            path = %path.display(),
            valid_len,
            repair,
            "dropped torn tail in session event log (INV-SP-2)"
        );
    }

    if repair {
        repair_torn_tail(path, valid_len).await?;
    }

    Ok(())
}

/// Read every valid line of `path`, dropping a garbled/incomplete trailing line from the
/// in-memory result (INV-SP-2).
///
/// When `repair` is `true`, additionally truncates that torn tail physically on disk. Only the
/// session's exclusive-lock holder (see [`SessionEventLog::open_exclusive`]) may pass `true`: it
/// is the only caller that can prove a "torn" trailing line isn't actually a live writer's
/// in-flight, not-yet-fsynced append (#5487 Finding B) — a lockless reader physically truncating
/// the file could destroy a concurrent writer's tail out from under it.
///
/// Returns the validated events, the maximum `seq` seen (`None` for an empty/absent log), and
/// the verified chain head hash (`None` if the log is pure legacy — no chain metadata anywhere).
///
/// # Errors
///
/// Returns [`SessionError::Integrity`] (S1) if an internal (non-trailing) line is malformed —
/// which must never be treated as a repairable torn tail — or if hash-chain verification fails.
/// This check always completes, and any resulting error is returned, **before** the torn-tail
/// repair below ever runs: a chain-verifiable-but-corrupted internal line must never be silently
/// truncated away as ordinary crash recovery.
async fn read_events(
    path: &Path,
    repair: bool,
    ring: Option<&ChainKeyRing>,
    file_identity: &[u8],
    allow_unverified: bool,
    anchor: Option<&Anchor>,
) -> Result<(Vec<SessionEventEnvelope>, Option<u64>, Option<ChainHash>), SessionError> {
    let Some(mut lines) = EventLineReader::open(path).await? else {
        return Ok((Vec::new(), None, None));
    };

    let mut events = Vec::new();
    let mut max_seq = None;
    let mut torn = false;
    let mut chain = SessionChainTracker::new(path, ring, file_identity, allow_unverified, anchor);

    loop {
        match lines.next_line().await? {
            LineOutcome::Eof => break,
            LineOutcome::Blank => {}
            LineOutcome::Event(envelope) => {
                chain.feed(&envelope)?;
                // Track the true running maximum, not just the last line's value: a
                // file whose physical order doesn't match seq order (e.g. from a
                // pre-fix #5487 race) must still yield the correct next seq.
                max_seq = Some(max_seq.map_or(envelope.seq, |m: u64| m.max(envelope.seq)));
                events.push(*envelope);
            }
            LineOutcome::Torn => {
                torn = peek_confirms_trailing_torn(&mut lines, path).await?;
                break;
            }
        }
    }
    let valid_len = lines.valid_len;
    drop(lines);

    // S1: chain verification has already run above, per event, as it was parsed — any failure
    // already returned via `chain.feed`'s `?` before this point, so `finish_torn_tail`'s
    // physical repair below is only ever reached once the whole read is chain-verified clean.
    let chain_head = chain.finish()?;

    finish_torn_tail(path, valid_len, repair, torn).await?;

    Ok((events, max_seq, chain_head))
}

/// Read `path`'s events in bounded chunks of at most [`REPLAY_CHUNK_SIZE`], invoking `on_chunk`
/// for each chunk instead of materializing the whole file into one `Vec` (spec §6.2 step 3).
/// Torn-tail detection/repair semantics match [`read_events`] exactly — the torn check happens
/// once, when EOF is reached (or not at all, if `on_chunk` breaks early). Chain verification
/// (S1) runs per event as it is parsed, strictly before that event is added to a chunk that
/// might be handed to `on_chunk`, so a tampered event is never exposed to the caller.
async fn read_events_chunked(
    path: &Path,
    repair: bool,
    ring: Option<&ChainKeyRing>,
    file_identity: &[u8],
    allow_unverified: bool,
    anchor: Option<&Anchor>,
    mut on_chunk: impl FnMut(Vec<SessionEventEnvelope>) -> ControlFlow<()>,
) -> Result<(), SessionError> {
    let Some(mut lines) = EventLineReader::open(path).await? else {
        return Ok(());
    };

    let mut chunk = Vec::with_capacity(REPLAY_CHUNK_SIZE);
    let mut torn = false;
    let mut broke_early = false;
    let mut chain = SessionChainTracker::new(path, ring, file_identity, allow_unverified, anchor);

    loop {
        match lines.next_line().await? {
            LineOutcome::Eof => break,
            LineOutcome::Blank => {}
            LineOutcome::Event(envelope) => {
                chain.feed(&envelope)?;
                chunk.push(*envelope);
                if chunk.len() >= REPLAY_CHUNK_SIZE {
                    let flushed =
                        std::mem::replace(&mut chunk, Vec::with_capacity(REPLAY_CHUNK_SIZE));
                    if on_chunk(flushed).is_break() {
                        broke_early = true;
                        break;
                    }
                }
            }
            LineOutcome::Torn => {
                torn = peek_confirms_trailing_torn(&mut lines, path).await?;
                break;
            }
        }
    }

    if !broke_early && !chunk.is_empty() && on_chunk(chunk).is_break() {
        broke_early = true;
    }

    // An early `Break` means the caller (e.g. replay's `up_to` bound) stopped before EOF —
    // whatever lies beyond that point, torn or not, is irrelevant to this read.
    if broke_early {
        return Ok(());
    }

    let valid_len = lines.valid_len;
    drop(lines);
    let _chain_head = chain.finish()?;

    finish_torn_tail(path, valid_len, repair, torn).await?;

    Ok(())
}

/// S1 guard: a genuinely crash-torn tail (INV-D2, single serialized writer) can only be the
/// file's physical last line. Called immediately after [`LineOutcome::Torn`], this peeks one
/// more line to confirm nothing follows — if more content does follow, the "torn" line was not
/// a crash artifact (it is either mid-file corruption or a deliberately tampered line hiding
/// further tampering), and must never be treated as a repairable trailing tail.
///
/// Returns `Ok(true)` (genuine trailing torn tail, eligible for [`repair_torn_tail`]) only when
/// EOF immediately follows.
///
/// # Errors
///
/// Returns [`SessionError::Integrity`] if anything other than EOF follows the torn line.
async fn peek_confirms_trailing_torn(
    lines: &mut EventLineReader,
    path: &Path,
) -> Result<bool, SessionError> {
    match lines.next_line().await? {
        LineOutcome::Eof => Ok(true),
        _ => Err(SessionError::Integrity(format!(
            "internal malformed line in '{}' is not the file's physical last line — refusing \
             to treat it as a torn crash-recovery tail (TAMPER DETECTED or mid-file corruption)",
            path.display()
        ))),
    }
}

/// Sets Unix permission bits on `path` (e.g. `0o700` for a directory, `0o600` for a file); a
/// no-op on non-Unix targets. `pub(crate)` so other modules (e.g. [`crate::fork`]) can apply the
/// same permission convention to directories/files they create outside this module.
#[cfg(unix)]
pub(crate) async fn set_permissions(path: &Path, mode: u32) -> Result<(), SessionError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    Ok(())
}

/// Cross-process advisory lock enforcing INV-D2's single-writer invariant, held for the
/// lifetime of a [`SessionEventLog`] opened via [`SessionEventLog::open_exclusive`].
///
/// Backed by `flock(2)` on a sibling lock file (`events.jsonl.lock`) rather than
/// `events.jsonl` itself, so the lock is independent of the append-mode file handle already
/// held for writing. Mirrors `zeph-scheduler`'s `PidFile`: the holder's PID is written into the
/// file once the lock is acquired, so a contending `acquire` can read it back to tell an
/// operator (or [`SessionError::AlreadyLocked`]'s caller) which process to check — unlike a pid
/// file, though, the lock file is never unlinked on drop: it is a permanent sentinel, not
/// ephemeral process identity, and unlinking it would reopen an unlink/re-create race between
/// the releasing and the next acquiring process.
///
/// **Invariant**: `session_dir` MUST reside on a local filesystem. NFS/network mounts do not
/// guarantee reliable exclusive locking with `flock(2)` (#6378).
#[cfg(unix)]
struct AdvisoryLock(#[allow(dead_code)] rustix::fd::OwnedFd);

#[cfg(unix)]
impl AdvisoryLock {
    fn acquire(session_dir: &Path) -> Result<Self, SessionError> {
        use rustix::fs::{FlockOperation, Mode, OFlags};

        let lock_path = session_dir.join(LOCK_FILE_NAME);
        let fd = rustix::fs::open(
            &lock_path,
            OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(std::io::Error::from)?;

        rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                // The lock file is a permanent sentinel (never unlinked, never cleared on
                // `Drop`), so between a new holder's successful `flock` above and its
                // `ftruncate`+write below, this read can still observe the *previous* holder's
                // PID — a dead PID here is a snapshot, not proof the current holder is gone
                // (#6378, see `describe_already_locked`'s hedged wording).
                let pid = zeph_common::pidfile::read_pid_lenient(&lock_path);
                let pid_alive = pid.map(zeph_common::pidfile::is_process_alive);
                SessionError::AlreadyLocked {
                    path: lock_path.display().to_string(),
                    pid,
                    pid_alive,
                }
            } else {
                SessionError::Io(e.into())
            }
        })?;

        // We hold the lock — record our own PID so a future contending `acquire` can diagnose
        // us (#6378: previously the lock file was permanently empty, giving an operator nothing
        // to verify a contended lock against). Mirrors `PidLockGuard::acquire`.
        rustix::fs::ftruncate(&fd, 0).map_err(std::io::Error::from)?;
        // A `u32` PID renders to at most 10 bytes — a single `write(2)` to a local regular file
        // for a buffer this small does not return a short count in practice, so no `write_all`
        // retry loop is needed here.
        rustix::io::write(&fd, std::process::id().to_string().as_bytes())
            .map_err(std::io::Error::from)?;

        Ok(Self(fd))
    }
}

/// No vetted cross-platform advisory-locking primitive exists in this workspace, so
/// [`SessionEventLog::open_exclusive`] does not enforce INV-D2 on non-Unix targets.
#[cfg(not(unix))]
struct AdvisoryLock;

#[cfg(not(unix))]
impl AdvisoryLock {
    fn acquire(_session_dir: &Path) -> Result<Self, SessionError> {
        Ok(Self)
    }
}

#[cfg(not(unix))]
pub(crate) async fn set_permissions(_path: &Path, _mode: u32) -> Result<(), SessionError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use super::*;

    #[tokio::test]
    async fn test_append_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();

        for i in 0..5u64 {
            log.append(
                Some(i),
                None,
                SessionEvent::UserMessage {
                    text: format!("msg-{i}"),
                    image_refs: vec![],
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(log.last_seq(), Some(4));
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 5);
        for (i, envelope) in events.iter().enumerate() {
            assert_eq!(envelope.seq, i as u64);
        }
    }

    #[tokio::test]
    async fn test_reopen_resumes_seq() {
        let dir = tempfile::tempdir().unwrap();
        {
            let log = SessionEventLog::open(dir.path()).await.unwrap();
            log.append(
                None,
                None,
                SessionEvent::SessionEnded { reason: "x".into() },
            )
            .await
            .unwrap();
        }
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        assert_eq!(log.last_seq(), Some(0));
        let appended = log
            .append(
                None,
                None,
                SessionEvent::SessionEnded { reason: "y".into() },
            )
            .await
            .unwrap();
        assert_eq!(appended.seq, 1);
    }

    #[tokio::test]
    async fn test_torn_write_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path;
        {
            let log = SessionEventLog::open(dir.path()).await.unwrap();
            for i in 0..3u64 {
                log.append(
                    None,
                    None,
                    SessionEvent::UserMessage {
                        text: format!("msg-{i}"),
                        image_refs: vec![],
                    },
                )
                .await
                .unwrap();
            }
            path = log.path().to_path_buf();
        }

        // Simulate a torn write: truncate the file mid-way through the last line.
        let full = tokio::fs::read(&path).await.unwrap();
        let cut = full.len() - 5;
        tokio::fs::write(&path, &full[..cut]).await.unwrap();

        let log = SessionEventLog::open(dir.path()).await.unwrap();
        assert_eq!(
            log.last_seq(),
            Some(1),
            "torn last line must be dropped cleanly"
        );
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 2);
    }

    /// Regression test for #5487 Finding B: a lockless `open()`/`read_all()` must never
    /// physically truncate a torn tail — it cannot distinguish a genuinely torn line from a
    /// live writer's in-flight, not-yet-fsynced append, so mutating the file could destroy that
    /// writer's data out from under it. Only `open_exclusive()` may repair.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_does_not_physically_truncate_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path;
        {
            let log = SessionEventLog::open(dir.path()).await.unwrap();
            for i in 0..3u64 {
                log.append(
                    None,
                    None,
                    SessionEvent::UserMessage {
                        text: format!("msg-{i}"),
                        image_refs: vec![],
                    },
                )
                .await
                .unwrap();
            }
            path = log.path().to_path_buf();
        }

        let full = tokio::fs::read(&path).await.unwrap();
        let cut = full.len() - 5;
        tokio::fs::write(&path, &full[..cut]).await.unwrap();
        let torn_len = tokio::fs::metadata(&path).await.unwrap().len();

        // Lockless open()/read_all(): in-memory result drops the torn line, but the file on
        // disk must be untouched.
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        assert_eq!(log.last_seq(), Some(1));
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            tokio::fs::metadata(&path).await.unwrap().len(),
            torn_len,
            "open()/read_all() must never physically truncate the file"
        );
        drop(log);

        // open_exclusive(): now physically repairs the file.
        let log = SessionEventLog::open_exclusive(dir.path()).await.unwrap();
        assert_eq!(log.last_seq(), Some(1));
        let repaired_len = tokio::fs::metadata(&path).await.unwrap().len();
        assert!(
            repaired_len < torn_len,
            "open_exclusive() must physically truncate the torn tail"
        );
    }

    #[tokio::test]
    async fn test_torn_write_truncation_various_offsets() {
        for cut_from_end in [1usize, 3, 10, 20] {
            let dir = tempfile::tempdir().unwrap();
            let path;
            {
                let log = SessionEventLog::open(dir.path()).await.unwrap();
                for i in 0..4u64 {
                    log.append(
                        None,
                        None,
                        SessionEvent::UserMessage {
                            text: format!("event-number-{i}"),
                            image_refs: vec![],
                        },
                    )
                    .await
                    .unwrap();
                }
                path = log.path().to_path_buf();
            }
            let full = tokio::fs::read(&path).await.unwrap();
            let cut = full.len().saturating_sub(cut_from_end);
            tokio::fs::write(&path, &full[..cut]).await.unwrap();

            // Must not panic and must never see more than the 4 originally-committed events.
            let log = SessionEventLog::open(dir.path()).await.unwrap();
            let events = log.read_all().await.unwrap();
            assert!(events.len() <= 4);
        }
    }

    #[tokio::test]
    async fn test_empty_log_read_all() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        assert_eq!(log.last_seq(), None);
        assert!(log.read_all().await.unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_file_permissions_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        let meta = tokio::fs::metadata(log.path()).await.unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    /// Regression test for #5487 bug B: `read_events` must compute the true running
    /// maximum `seq`, not just take the last physical line's value. Simulates the on-disk
    /// shape a pre-fix concurrent-append race could produce: seq 7 written physically before
    /// seq 6.
    #[tokio::test]
    async fn test_max_seq_survives_out_of_order_physical_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(EVENTS_FILE_NAME);

        let make_line = |seq: u64| {
            let envelope = SessionEventEnvelope::new(
                seq,
                None,
                None,
                SessionEvent::SessionEnded { reason: "x".into() },
            );
            let mut line = serde_json::to_vec(&envelope).unwrap();
            line.push(b'\n');
            line
        };

        // Physical order is seq=7 then seq=6 — out of seq order, as a pre-fix race could
        // produce, but every line individually well-formed and fsynced.
        let mut contents = make_line(7);
        contents.extend(make_line(6));
        tokio::fs::write(&path, &contents).await.unwrap();

        let log = SessionEventLog::open(dir.path()).await.unwrap();
        assert_eq!(
            log.last_seq(),
            Some(7),
            "next_seq must be derived from the true max seq, not the last physical line"
        );
        let appended = log
            .append(
                None,
                None,
                SessionEvent::SessionEnded { reason: "z".into() },
            )
            .await
            .unwrap();
        assert_eq!(
            appended.seq, 8,
            "must not reuse a seq already present earlier in the file"
        );
    }

    /// Regression test for #6378: `AdvisoryLock::acquire` must record the holder's own PID
    /// into the lock file's contents so a contending `acquire` can diagnose who holds it.
    /// Before the fix the lock file was permanently empty.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_exclusive_writes_own_pid_into_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let _log = SessionEventLog::open_exclusive(dir.path()).await.unwrap();

        let lock_path = dir.path().join(LOCK_FILE_NAME);
        let contents = tokio::fs::read_to_string(&lock_path).await.unwrap();
        let pid: u32 = contents.trim().parse().unwrap_or_else(|e| {
            panic!("lock file contents {contents:?} did not parse as a PID: {e}")
        });
        assert_eq!(pid, std::process::id());
    }

    /// Regression test for #6378: on contention, `SessionError::AlreadyLocked` must carry the
    /// holder's PID (read back from the lock file) and a liveness verdict, not just the lock
    /// path. This is a same-process test, so both the holder and the contender are the current
    /// process — a genuinely alive PID is exactly what `pid_alive` must report.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_exclusive_rejects_second_writer() {
        let dir = tempfile::tempdir().unwrap();
        let _first = SessionEventLog::open_exclusive(dir.path()).await.unwrap();
        match SessionEventLog::open_exclusive(dir.path()).await {
            Err(SessionError::AlreadyLocked { pid, pid_alive, .. }) => {
                assert_eq!(pid, Some(std::process::id()));
                assert_eq!(pid_alive, Some(true));
            }
            Err(e) => panic!("expected AlreadyLocked, got different error: {e}"),
            Ok(_) => panic!("expected AlreadyLocked, but second open_exclusive succeeded"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_exclusive_allows_reacquire_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _first = SessionEventLog::open_exclusive(dir.path()).await.unwrap();
        }
        // Lock released when the first handle dropped — must not still be held.
        let _second = SessionEventLog::open_exclusive(dir.path()).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_open_is_not_blocked_by_open_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let _writer = SessionEventLog::open_exclusive(dir.path()).await.unwrap();
        // Read-only `open()` must still succeed while a writer holds the exclusive lock.
        let _reader = SessionEventLog::open(dir.path()).await.unwrap();
    }

    /// Regression test for #5487 bug A: drives genuine concurrent `append()` calls (on real
    /// OS threads, not just cooperative interleaving) against one shared `SessionEventLog` and
    /// asserts seq assignment and physical write order never diverge. Before the fix, `seq`
    /// was assigned via `fetch_add` before acquiring the writer lock, so a task could win a
    /// low seq but lose the race for the lock, landing its line after a higher-seq task's line.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_concurrent_append_preserves_seq_order() {
        const N: u64 = 100;

        let dir = tempfile::tempdir().unwrap();
        let log = std::sync::Arc::new(SessionEventLog::open(dir.path()).await.unwrap());

        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..N {
            let log = log.clone();
            tasks.spawn(async move {
                log.append(
                    None,
                    None,
                    SessionEvent::UserMessage {
                        text: format!("msg-{i}"),
                        image_refs: vec![],
                    },
                )
                .await
                .unwrap()
                .seq
            });
        }

        let mut assigned_seqs: Vec<u64> = tasks.join_all().await;
        assigned_seqs.sort_unstable();
        assert_eq!(
            assigned_seqs,
            (0..N).collect::<Vec<_>>(),
            "every seq in 0..{N} must be assigned exactly once, with no gaps or duplicates"
        );

        // Physical order on disk must match seq order: seq assignment and the write it
        // guards must never diverge under contention (#5487 fix 2).
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), usize::try_from(N).unwrap());
        for (i, envelope) in events.iter().enumerate() {
            assert_eq!(
                envelope.seq, i as u64,
                "physical line {i} must carry seq {i}; seq and write order diverged"
            );
        }
    }

    /// Regression test for #5445 Finding 3: `read_events_chunked` must never hold more than
    /// [`REPLAY_CHUNK_SIZE`] raw envelopes at once, and the concatenation of all chunks must
    /// exactly reproduce what `read_events` (the whole-file `Vec` path) returns, in order.
    #[tokio::test]
    async fn test_read_chunked_bounds_memory_and_matches_whole_file_read() {
        const N: u64 = 733; // comfortably > REPLAY_CHUNK_SIZE, not an exact multiple of it

        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        for i in 0..N {
            log.append(
                None,
                None,
                SessionEvent::UserMessage {
                    text: format!("msg-{i}"),
                    image_refs: vec![],
                },
            )
            .await
            .unwrap();
        }

        let (whole_file_events, _, _) =
            read_events(log.path(), false, None, b"test-session", false, None)
                .await
                .unwrap();
        assert_eq!(whole_file_events.len(), usize::try_from(N).unwrap());

        let mut chunked_events = Vec::new();
        let mut chunk_sizes = Vec::new();
        read_events_chunked(log.path(), false, None, b"test-session", false, None, |chunk| {
            assert!(
                chunk.len() <= REPLAY_CHUNK_SIZE,
                "a single chunk must never exceed REPLAY_CHUNK_SIZE ({REPLAY_CHUNK_SIZE}), got {}",
                chunk.len()
            );
            chunk_sizes.push(chunk.len());
            chunked_events.extend(chunk);
            ControlFlow::Continue(())
        })
        .await
        .unwrap();

        assert_eq!(
            chunked_events.len(),
            whole_file_events.len(),
            "chunked read must yield the same total event count as the whole-file read"
        );
        for (whole, chunked) in whole_file_events.iter().zip(chunked_events.iter()) {
            assert_eq!(whole.seq, chunked.seq);
        }
        assert!(
            chunk_sizes.len() > 1,
            "expected multiple chunks for N={N} events with REPLAY_CHUNK_SIZE={REPLAY_CHUNK_SIZE}"
        );
    }

    // --- Hash-chain integrity tests (issue #6360) ---
    //
    // `configure_history_integrity` mutates process-global state, so these tests rely on
    // `cargo nextest`'s one-process-per-test model for isolation (never run this module with
    // plain `cargo test`, which shares one process across tests in a binary and could race).

    fn test_ring(epoch: u32, byte: u8) -> Arc<ChainKeyRing> {
        Arc::new(ChainKeyRing::new(
            epoch,
            zeph_common::hash_chain::ChainKey::new([byte; 32]),
        ))
    }

    #[tokio::test]
    async fn chained_log_roundtrip() {
        configure_history_integrity(Some(test_ring(0, 20)));
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "hello".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::SessionEnded { reason: "x".into() },
        )
        .await
        .unwrap();
        drop(log);

        let raw = tokio::fs::read_to_string(dir.path().join(EVENTS_FILE_NAME))
            .await
            .unwrap();
        assert!(
            raw.lines().all(|l| l.contains("\"chain\":")),
            "every line must carry a chain field once integrity is configured"
        );

        let log = SessionEventLog::open(dir.path()).await.unwrap();
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 2);

        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn tamper_in_place_edit_is_detected() {
        configure_history_integrity(Some(test_ring(0, 21)));
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        // A first, untouched entry so the key epoch resolves cleanly there; tampering the
        // *second* entry below then produces a definite Mismatch (not an ambiguous
        // Unverifiable, which is what tampering the very first chained entry would produce —
        // that case is covered separately by the epoch-resolution tests).
        log.append(
            None,
            None,
            SessionEvent::SessionEnded {
                reason: "untouched".into(),
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "original".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        drop(log);

        let path = dir.path().join(EVENTS_FILE_NAME);
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let tampered = raw.replace("original", "forged-approval");
        assert_ne!(raw, tampered);
        tokio::fs::write(&path, tampered).await.unwrap();

        let result = SessionEventLog::open(dir.path()).await;
        assert!(matches!(result, Err(SessionError::Integrity(ref m)) if m.contains("TAMPER")));

        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn legacy_log_is_auto_trusted_once_when_integrity_configured_later() {
        configure_history_integrity(None);
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "pre-feature message".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        drop(log);

        let raw = tokio::fs::read_to_string(dir.path().join(EVENTS_FILE_NAME))
            .await
            .unwrap();
        assert!(!raw.contains("\"chain\":"));

        configure_history_integrity(Some(test_ring(0, 22)));
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        let events = log.read_all().await.unwrap();
        assert_eq!(
            events.len(),
            1,
            "legacy content must be auto-trusted, not rejected"
        );

        // A legacy log read while a key IS configured must be flagged exactly once per path
        // (security review B2 condition (c)) — repeat reads must not re-warn.
        let events_path = dir.path().join(EVENTS_FILE_NAME);
        assert!(
            WARNED_LEGACY_UNDER_KEY
                .read()
                .unwrap()
                .contains(&events_path),
            "path must be recorded as warned after the first legacy-under-active-key read"
        );
        let warned_count_before = WARNED_LEGACY_UNDER_KEY.read().unwrap().len();
        let _ = log.read_all().await.unwrap();
        assert_eq!(
            WARNED_LEGACY_UNDER_KEY.read().unwrap().len(),
            warned_count_before,
            "a second read of the same path must not add a second warned-set entry"
        );

        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn partial_strip_of_chain_field_is_detected_as_tamper() {
        configure_history_integrity(Some(test_ring(0, 23)));
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "one".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "two".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        drop(log);

        let path = dir.path().join(EVENTS_FILE_NAME);
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        let mut second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        second.as_object_mut().unwrap().remove("chain");
        let stripped = format!("{}\n{}\n", lines[0], second);
        tokio::fs::write(&path, stripped).await.unwrap();

        let result = SessionEventLog::open(dir.path()).await;
        assert!(
            matches!(result, Err(SessionError::Integrity(ref m)) if m.contains("partial strip"))
        );

        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn key_unavailable_on_chained_log_fails_closed_not_legacy() {
        configure_history_integrity(Some(test_ring(0, 24)));
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::SessionEnded { reason: "x".into() },
        )
        .await
        .unwrap();
        drop(log);

        configure_history_integrity(None);
        let result = SessionEventLog::open(dir.path()).await;
        assert!(matches!(result, Err(SessionError::Integrity(_))));
    }

    /// `--allow-unverified` override: a tampered chained log must still open and read via
    /// `open_exclusive_allow_unverified`, and the bypass must persist across `read_all` calls
    /// on the same handle, not just the initial open.
    #[tokio::test]
    async fn allow_unverified_bypasses_tamper_detection_for_the_whole_handle() {
        configure_history_integrity(Some(test_ring(0, 40)));
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::SessionEnded {
                reason: "untouched".into(),
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "original".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        drop(log);

        let path = dir.path().join(EVENTS_FILE_NAME);
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let tampered = raw.replace("original", "forged-approval");
        assert_ne!(raw, tampered);
        tokio::fs::write(&path, tampered).await.unwrap();

        // The normal path still fails closed.
        let result = SessionEventLog::open_exclusive(dir.path()).await;
        assert!(matches!(result, Err(SessionError::Integrity(_))));

        // The deliberate override succeeds, both at open and on a subsequent read_all.
        let log = SessionEventLog::open_exclusive_allow_unverified(dir.path())
            .await
            .unwrap();
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 2);

        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn rotated_key_epoch_verifies_as_rekeyed_not_tampered() {
        let old_key_byte = 25u8;
        configure_history_integrity(Some(test_ring(0, old_key_byte)));
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::SessionEnded { reason: "x".into() },
        )
        .await
        .unwrap();
        drop(log);

        let ring = Arc::new(
            ChainKeyRing::new(1, zeph_common::hash_chain::ChainKey::new([30u8; 32])).with_previous(
                0,
                zeph_common::hash_chain::ChainKey::new([old_key_byte; 32]),
            ),
        );
        configure_history_integrity(Some(ring));

        let log = SessionEventLog::open(dir.path()).await.unwrap();
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 1);

        configure_history_integrity(None);
    }

    /// S1 regression: a mid-file (non-trailing) malformed line must never be silently
    /// auto-truncated by `open_exclusive`'s torn-tail repair — it must surface as an integrity
    /// error instead. This is a correctness bug independent of chaining (it corrupts crash
    /// recovery itself), reproduced here without configuring any key ring.
    #[tokio::test]
    async fn internal_malformed_line_is_never_treated_as_torn_tail() {
        configure_history_integrity(None);
        let dir = tempfile::tempdir().unwrap();
        let path;
        {
            let log = SessionEventLog::open(dir.path()).await.unwrap();
            for i in 0..3u64 {
                log.append(
                    None,
                    None,
                    SessionEvent::UserMessage {
                        text: format!("msg-{i}"),
                        image_refs: vec![],
                    },
                )
                .await
                .unwrap();
            }
            path = log.path().to_path_buf();
        }

        // Corrupt the *middle* line (not the last) so it fails to parse as JSON, followed by
        // legitimate content — simulates a tamper that overwrites one line in place with
        // garbage, as opposed to a genuine crash mid-append (which can only corrupt the tail).
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        let corrupted = format!("{}\nnot valid json at all\n{}\n", lines[0], lines[2]);
        tokio::fs::write(&path, corrupted).await.unwrap();

        // Under the pre-S1 bug, `open_exclusive` would silently truncate everything from the
        // corrupted line onward, treating it as an ordinary torn crash-recovery tail. It must
        // instead fail closed.
        let result = SessionEventLog::open_exclusive(dir.path()).await;
        assert!(matches!(result, Err(SessionError::Integrity(_))));

        // And the file on disk must be untouched — no silent repair happened.
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            after.lines().count(),
            3,
            "file must not have been truncated"
        );

        configure_history_integrity(None);
    }

    /// S2 regression: concurrent `append` calls must never desynchronize on-disk physical order
    /// from chain-link order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_append_preserves_chain_order() {
        const N: u64 = 60;
        configure_history_integrity(Some(test_ring(0, 26)));
        let dir = tempfile::tempdir().unwrap();
        let log = std::sync::Arc::new(SessionEventLog::open(dir.path()).await.unwrap());

        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..N {
            let log = log.clone();
            tasks.spawn(async move {
                log.append(
                    None,
                    None,
                    SessionEvent::UserMessage {
                        text: format!("msg-{i}"),
                        image_refs: vec![],
                    },
                )
                .await
                .unwrap();
            });
        }
        while tasks.join_next().await.is_some() {}
        drop(log);

        // If chain order had diverged from physical write order, this would fail with a
        // definite Mismatch tamper verdict even though nothing was actually tampered with.
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), usize::try_from(N).unwrap());

        configure_history_integrity(None);
    }

    /// Chunked reads (used by replay) must verify the chain identically to the whole-file read.
    #[tokio::test]
    async fn chunked_read_verifies_chain_and_matches_whole_file_read() {
        const N: u64 = 250; // > REPLAY_CHUNK_SIZE, exercises the chunk-boundary epoch resolution
        configure_history_integrity(Some(test_ring(0, 27)));
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        for i in 0..N {
            log.append(
                None,
                None,
                SessionEvent::UserMessage {
                    text: format!("msg-{i}"),
                    image_refs: vec![],
                },
            )
            .await
            .unwrap();
        }

        let whole = log.read_all().await.unwrap();
        assert_eq!(whole.len(), usize::try_from(N).unwrap());

        let mut chunked = Vec::new();
        log.read_chunked(|chunk| {
            chunked.extend(chunk);
            ControlFlow::Continue(())
        })
        .await
        .unwrap();
        assert_eq!(chunked.len(), whole.len());

        configure_history_integrity(None);
    }

    /// Chunked reads must also detect tamper, not just the whole-file read — a tampered event
    /// deep enough to land in a later chunk must abort before ever reaching `on_chunk`.
    #[tokio::test]
    async fn chunked_read_detects_tamper_in_a_later_chunk() {
        const N: u64 = 150;
        configure_history_integrity(Some(test_ring(0, 28)));
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        for i in 0..N {
            log.append(
                None,
                None,
                SessionEvent::UserMessage {
                    text: format!("msg-{i}"),
                    image_refs: vec![],
                },
            )
            .await
            .unwrap();
        }
        let path = log.path().to_path_buf();
        drop(log);

        // Tamper an event past the first REPLAY_CHUNK_SIZE (100) events.
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let tampered = raw.replacen("msg-120", "forged-120", 1);
        assert_ne!(raw, tampered);
        tokio::fs::write(&path, tampered).await.unwrap();

        configure_history_integrity(Some(test_ring(0, 28)));
        let log = SessionEventLog::open(dir.path()).await;
        // Depending on where tamper lands relative to open-time seeding, this may fail at
        // `open` (M3 tail verify covers the whole file) — assert the failure is an Integrity
        // error either at open or at an explicit chunked read.
        match log {
            Err(SessionError::Integrity(_)) => {}
            Ok(log) => {
                let mut seen = Vec::new();
                let result = log
                    .read_chunked(|chunk| {
                        seen.extend(chunk);
                        ControlFlow::Continue(())
                    })
                    .await;
                assert!(matches!(result, Err(SessionError::Integrity(_))));
            }
            Err(other) => panic!("expected Integrity error, got {other:?}"),
        }

        configure_history_integrity(None);
    }

    // --- Vault-anchor downgrade-resistance tests (issue #6449) ---

    /// In-memory [`AnchorStore`] mock for tests, mirroring the identical mock in
    /// `zeph_subagent::transcript`'s test module.
    #[derive(Default)]
    struct MockAnchorStore {
        map: std::sync::Mutex<std::collections::HashMap<String, Anchor>>,
    }

    impl AnchorStore for MockAnchorStore {
        fn get(
            &self,
            subsystem: AnchorSubsystem,
            file_id: &[u8],
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Option<Anchor>, zeph_common::anchor::AnchorError>>
                    + Send
                    + '_,
            >,
        > {
            let result = self.get_sync(subsystem, file_id);
            Box::pin(async move { result })
        }

        fn get_sync(
            &self,
            subsystem: AnchorSubsystem,
            file_id: &[u8],
        ) -> Result<Option<Anchor>, zeph_common::anchor::AnchorError> {
            let key = zeph_common::anchor::anchor_key(subsystem, file_id);
            Ok(self.map.lock().unwrap().get(&key).cloned())
        }

        fn put(
            &self,
            subsystem: AnchorSubsystem,
            file_id: &[u8],
            anchor: Anchor,
        ) -> Pin<Box<dyn Future<Output = Result<(), zeph_common::anchor::AnchorError>> + Send + '_>>
        {
            let key = zeph_common::anchor::anchor_key(subsystem, file_id);
            self.map.lock().unwrap().insert(key, anchor);
            Box::pin(async { Ok(()) })
        }

        fn delete(
            &self,
            subsystem: AnchorSubsystem,
            file_id: &[u8],
        ) -> Pin<Box<dyn Future<Output = Result<(), zeph_common::anchor::AnchorError>> + Send + '_>>
        {
            let key = zeph_common::anchor::anchor_key(subsystem, file_id);
            self.map.lock().unwrap().remove(&key);
            Box::pin(async { Ok(()) })
        }
    }

    /// FINDING B regression: a session log chained before any anchor store existed must still
    /// open normally once one comes online — an absent anchor is never a tamper signature.
    #[tokio::test]
    async fn pre_anchor_chained_log_still_opens_with_anchor_store_online() {
        configure_history_integrity(Some(test_ring(0, 40)));
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "pre-anchor".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        drop(log);

        configure_anchor_store(Some(Arc::new(MockAnchorStore::default())));
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        let events = log.read_all().await.unwrap();
        assert_eq!(
            events.len(),
            1,
            "absent anchor must never brick a legacy-chained log"
        );

        configure_anchor_store(None);
        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn whole_strip_of_anchored_session_is_tamper() {
        configure_history_integrity(Some(test_ring(0, 41)));
        let store: Arc<dyn AnchorStore> = Arc::new(MockAnchorStore::default());
        configure_anchor_store(Some(Arc::clone(&store)));

        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "one".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::SessionEnded { reason: "x".into() },
        )
        .await
        .unwrap();
        log.finalize().await.unwrap();
        drop(log);

        // Sanity: anchored and untouched, the log still opens.
        assert!(SessionEventLog::open(dir.path()).await.is_ok());

        let path = dir.path().join(EVENTS_FILE_NAME);
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let stripped: String = raw
            .lines()
            .map(|line| {
                let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
                value.as_object_mut().unwrap().remove("chain");
                value.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        tokio::fs::write(&path, stripped).await.unwrap();

        match SessionEventLog::open(dir.path()).await {
            Err(SessionError::Integrity(m)) => {
                assert!(m.contains("TAMPER") && m.contains("vault anchor"), "{m}");
            }
            other => panic!("expected Integrity TAMPER error, got {}", other.is_ok()),
        }

        configure_anchor_store(None);
        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn truncation_below_anchored_session_count_is_tamper() {
        configure_history_integrity(Some(test_ring(0, 42)));
        let store: Arc<dyn AnchorStore> = Arc::new(MockAnchorStore::default());
        configure_anchor_store(Some(Arc::clone(&store)));

        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "one".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::SessionEnded { reason: "x".into() },
        )
        .await
        .unwrap();
        log.finalize().await.unwrap();
        drop(log);

        let path = dir.path().join(EVENTS_FILE_NAME);
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let first_line = raw.lines().next().unwrap();
        tokio::fs::write(&path, format!("{first_line}\n"))
            .await
            .unwrap();

        match SessionEventLog::open(dir.path()).await {
            Err(SessionError::Integrity(m)) => {
                assert!(m.contains("TAMPER") && m.contains("truncated"), "{m}");
            }
            other => panic!("expected Integrity TAMPER error, got {}", other.is_ok()),
        }

        configure_anchor_store(None);
        configure_history_integrity(None);
    }

    /// Legitimate post-close growth (on-disk count > anchor.count, prefix matches) must open OK
    /// — the anchor is a prefix commitment, not an exact-count requirement, for sessions.
    #[tokio::test]
    async fn growth_after_anchor_with_matching_prefix_is_ok() {
        configure_history_integrity(Some(test_ring(0, 43)));
        let store: Arc<dyn AnchorStore> = Arc::new(MockAnchorStore::default());
        configure_anchor_store(Some(Arc::clone(&store)));

        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "one".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        log.finalize().await.unwrap();

        // More appended after the anchor was written (no new finalize) — a legitimate
        // still-open session continuing to grow.
        log.append(
            None,
            None,
            SessionEvent::SessionEnded { reason: "x".into() },
        )
        .await
        .unwrap();
        drop(log);

        let log = SessionEventLog::open(dir.path()).await.unwrap();
        let events = log.read_all().await.unwrap();
        assert_eq!(
            events.len(),
            2,
            "post-anchor growth with a matching prefix must open OK"
        );

        configure_anchor_store(None);
        configure_history_integrity(None);
    }

    #[tokio::test]
    async fn finalize_is_noop_without_anchor_store_or_without_chaining() {
        configure_history_integrity(Some(test_ring(0, 44)));
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::SessionEnded { reason: "x".into() },
        )
        .await
        .unwrap();
        log.finalize().await.unwrap();
        configure_history_integrity(None);

        let store: Arc<dyn AnchorStore> = Arc::new(MockAnchorStore::default());
        configure_anchor_store(Some(Arc::clone(&store)));
        let dir2 = tempfile::tempdir().unwrap();
        let log2 = SessionEventLog::open(dir2.path()).await.unwrap();
        log2.append(
            None,
            None,
            SessionEvent::SessionEnded {
                reason: "legacy".into(),
            },
        )
        .await
        .unwrap();
        log2.finalize().await.unwrap();
        let identity = file_identity(dir2.path());
        assert!(
            store
                .get_sync(AnchorSubsystem::SessionLog, &identity)
                .unwrap()
                .is_none(),
            "no anchor should be written for an unchained handle"
        );

        configure_anchor_store(None);
    }
}
