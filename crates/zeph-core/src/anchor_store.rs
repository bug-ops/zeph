// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Concrete age-vault-backed [`AnchorStore`] implementation and its growth-bound sweep
//! (issue #6449).
//!
//! `zeph_common::anchor::AnchorStore` is a pure trait consumed by `zeph-subagent` and
//! `zeph-session` (INV-1: those crates never depend on `zeph-vault`). This module is the
//! concrete implementation the binary installs into each adapter's process-global slot at
//! bootstrap, exactly mirroring `history_integrity`'s role for the hash-chain key ring.
//!
//! # Vault growth bound
//!
//! [`AgeVaultProvider::save`] re-encrypts and rewrites the **entire** secrets map on every write
//! — an anchor `put` is `O(total_secrets)`, not unit cost. Since transcript anchors are already
//! bounded (deleted alongside their file by `sweep_old_transcripts`'s companion reconcile pass —
//! see below) but session anchors are deliberately never deleted on `sessions delete` (a session's
//! `events.jsonl` itself survives that command), [`run_anchor_sweep`] bounds total vault growth
//! to `O(max_session_anchors + max_transcript_files)` by:
//!
//! 1. **Reconcile**: dropping any anchor whose on-disk file/session directory no longer exists
//!    (an orphan) — but only after a **grace window** (issue #6462; see `ORPHAN_REAP_GRACE_MS`)
//!    of *sustained* absence, not the instant a single sweep observes the file gone. Removing an
//!    orphan anchor is *never a false TAMPER risk* — an anchor is only ever consulted when opening
//!    a file that exists — but reaping it too eagerly is not an unconditionally benign no-op from
//!    a downgrade-resistance standpoint: under this feature's own threat model (file-write
//!    access), an attacker can delete the real file/session dir for a *specific, even
//!    recently-anchored* identity, then recreate a forged legacy-looking replacement under the
//!    same identity — which the read path trusts once the anchor is gone (an absent anchor is
//!    never a tamper signature, per the module docs on `zeph_common::anchor`). The grace window
//!    closes this: the first sweep that observes the file absent only *stamps*
//!    [`zeph_common::anchor::Anchor::orphaned_since`] with the current wall-clock time (the real
//!    anchor — `count`/`head`/`written_at` — is otherwise untouched, so a forged-but-chained
//!    recreation within the window is still caught by full checkpoint verification, not just the
//!    legacy-strip case); only once the file has stayed absent for at least
//!    `ORPHAN_REAP_GRACE_MS` does a later sweep hard-delete it. If the file reappears before
//!    the grace elapses, `orphaned_since` is cleared (self-heal) and the clock restarts on any
//!    future absence. The grace is keyed on wall-clock time, not a sweep-cycle counter, so it
//!    survives process restarts and stays correct regardless of `SWEEP_INTERVAL`. This remains a
//!    bounded residual, not a closed gap: an attacker who keeps the real file deleted for the
//!    full grace window still eventually gets the anchor reaped (overlapping the already-accepted
//!    "fabricate a brand-new legacy session" no-backfill residual, spec-081 FR-006) — the grace
//!    only raises the cost from "wait out one sweep (≤ 1h, or a restart)" to "sustain the
//!    deletion for the full grace window".
//! 2. **Cap**: evicting the oldest session anchors (by the `written_at` field embedded *inside*
//!    each AEAD-protected [`Anchor`] value — never filesystem mtime, which a file-write-only
//!    attacker can freely rewrite; see the module docs on `zeph_common::anchor`) once the
//!    session-anchor count exceeds `max_session_anchors`.
//!
//! Both steps decide what to remove from a brief read-locked snapshot (filesystem stats and
//! anchor decode run with **no** lock held at all), then mutate the vault's secrets map by
//! targeted key **in place** under a single write-lock scope, followed by exactly one `save()`
//! for the whole sweep — never a snapshot-modify-writeback of the *whole map*, which would
//! silently clobber a concurrent `put` racing the sweep. See [`run_anchor_sweep`] for why the
//! write lock is held only for the removal step, not the filesystem stats.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;

use tracing::Instrument as _;
use zeph_common::anchor::{Anchor, AnchorError, AnchorStore, AnchorSubsystem, parse_anchor_key};
use zeph_common::task_supervisor::{RestartPolicy, TaskDescriptor, TaskSupervisor};

use crate::vault::AgeVaultProvider;

/// Concrete [`AnchorStore`] backed by the process's shared age vault.
///
/// Mirrors `src/bootstrap/oauth.rs`'s `VaultCredentialStore` write pattern, upgraded to route
/// through [`TaskSupervisor::spawn_blocking`] instead of a raw `tokio::task::spawn_blocking` per
/// the CLAUDE.md async-supervision rule: every blocking age-encrypt/write must be a named,
/// observable, abortable task.
pub struct AgeVaultAnchorStore {
    vault: Arc<StdRwLock<AgeVaultProvider>>,
    supervisor: TaskSupervisor,
}

/// Bound on [`AgeVaultAnchorStore::get_sync`]'s blocking lock-acquire (issue #6449 M1). The
/// transcript read path is plain sync (see the `AnchorStore` trait docs for why) and so cannot
/// use `tokio::time::timeout` directly — bounded instead via polling `try_read` against a
/// deadline, so a vault stall fails closed within this bound rather than blocking forever.
const ANCHOR_GET_SYNC_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll interval for the [`AgeVaultAnchorStore::get_sync`] bounded try-read loop.
const ANCHOR_GET_SYNC_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Decode a raw vault value into an `Anchor`, shared by the sync and async `get` paths.
fn decode_anchor(value: Option<&str>) -> Result<Option<Anchor>, AnchorError> {
    match value {
        Some(json) => serde_json::from_str(json)
            .map(Some)
            .map_err(|e| AnchorError::Store(format!("anchor JSON decode failed: {e}"))),
        None => Ok(None),
    }
}

impl AgeVaultAnchorStore {
    /// Construct a store over the process's shared age vault handle.
    #[must_use]
    pub fn new(vault: Arc<StdRwLock<AgeVaultProvider>>, supervisor: TaskSupervisor) -> Self {
        Self { vault, supervisor }
    }

    /// The bounded-try-read implementation behind [`AnchorStore::get_sync`], parameterized on
    /// `timeout` so tests can exercise the fail-closed path without waiting out the real
    /// [`ANCHOR_GET_SYNC_TIMEOUT`].
    ///
    /// Never blocks indefinitely on lock contention: `std::sync::RwLock::try_read` never
    /// suspends the calling thread, so polling it against a deadline (rather than calling the
    /// blocking `read()`) bounds the wait even when a writer (an anchor `put`/`delete`, or
    /// `run_anchor_sweep`'s `save()`) holds the lock for longer than `timeout`.
    fn get_sync_bounded(
        &self,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
        timeout: Duration,
    ) -> Result<Option<Anchor>, AnchorError> {
        let key = zeph_common::anchor::anchor_key(subsystem, file_id);
        let _span = tracing::info_span!(
            "core.anchor.get_sync",
            subsystem = subsystem.key_segment(),
            key = %key
        )
        .entered();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.vault.try_read() {
                Ok(guard) => return decode_anchor(guard.get(&key)),
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    return decode_anchor(poisoned.into_inner().get(&key));
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(AnchorError::Store(format!(
                            "vault read lock timed out after {timeout:?} — failing closed \
                             rather than blocking indefinitely"
                        )));
                    }
                    std::thread::sleep(ANCHOR_GET_SYNC_POLL_INTERVAL);
                }
            }
        }
    }
}

impl AnchorStore for AgeVaultAnchorStore {
    fn get(
        &self,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<Option<Anchor>, AnchorError>> + Send + '_>> {
        // The lock-acquire + decode MUST happen inside the polled future (via spawn_blocking),
        // not synchronously here in the method prologue — otherwise a caller's
        // `tokio::time::timeout(dur, store.get(..))` wraps an already-resolved future with no
        // suspension point to race against, and the timeout can never fire (issue #6449 M1
        // regression: a prior version called `get_sync` here directly).
        let key = zeph_common::anchor::anchor_key(subsystem, file_id);
        let span = tracing::info_span!(
            "core.anchor.get",
            subsystem = subsystem.key_segment(),
            key = %key
        );
        let vault = Arc::clone(&self.vault);
        let supervisor = self.supervisor.clone();
        Box::pin(
            async move {
                let handle = supervisor.spawn_blocking(Arc::from("anchor-get"), move || {
                    let guard = vault
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    decode_anchor(guard.get(&key))
                });
                handle
                    .join()
                    .await
                    .map_err(|e| AnchorError::Store(format!("spawn_blocking: {e}")))?
            }
            .instrument(span),
        )
    }

    fn get_sync(
        &self,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
    ) -> Result<Option<Anchor>, AnchorError> {
        self.get_sync_bounded(subsystem, file_id, ANCHOR_GET_SYNC_TIMEOUT)
    }

    fn put(
        &self,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
        anchor: Anchor,
    ) -> Pin<Box<dyn Future<Output = Result<(), AnchorError>> + Send + '_>> {
        let key = zeph_common::anchor::anchor_key(subsystem, file_id);
        let span = tracing::info_span!(
            "core.anchor.put",
            subsystem = subsystem.key_segment(),
            key = %key
        );
        let vault = Arc::clone(&self.vault);
        let supervisor = self.supervisor.clone();
        Box::pin(
            async move {
                let json = serde_json::to_string(&anchor)
                    .map_err(|e| AnchorError::Store(format!("anchor JSON encode failed: {e}")))?;
                let handle = supervisor.spawn_blocking(Arc::from("anchor-put"), move || {
                    let mut guard = vault
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // An anchor is our own managed entry, always overwritten on re-finalize —
                    // mirrors the OAuth store's `set_secret_mut(.., true)` rationale.
                    guard
                        .set_secret_mut(key, json, true)
                        .map_err(|e| e.to_string())?;
                    guard.save().map_err(|e| e.to_string())
                });
                handle
                    .join()
                    .await
                    .map_err(|e| AnchorError::Store(format!("spawn_blocking: {e}")))?
                    .map_err(AnchorError::Store)
            }
            .instrument(span),
        )
    }

    fn delete(
        &self,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), AnchorError>> + Send + '_>> {
        let key = zeph_common::anchor::anchor_key(subsystem, file_id);
        let span = tracing::info_span!(
            "core.anchor.delete",
            subsystem = subsystem.key_segment(),
            key = %key
        );
        let vault = Arc::clone(&self.vault);
        let supervisor = self.supervisor.clone();
        Box::pin(
            async move {
                let handle = supervisor.spawn_blocking(Arc::from("anchor-delete"), move || {
                    let mut guard = vault
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !guard.remove_secret_mut(&key) {
                        return Ok(()); // absent — a no-op, not an error
                    }
                    guard.save().map_err(|e| e.to_string())
                });
                handle
                    .join()
                    .await
                    .map_err(|e| AnchorError::Store(format!("spawn_blocking: {e}")))?
                    .map_err(AnchorError::Store)
            }
            .instrument(span),
        )
    }
}

/// Install (or uninstall, with `store = None`) the vault-anchor store into both adapter crates'
/// process-global slots (issue #6449). Mirrors
/// `zeph_subagent::transcript::configure_history_integrity`'s single-set-at-startup contract.
pub fn install_anchor_store(store: Option<Arc<dyn AnchorStore>>) {
    zeph_subagent::transcript::configure_anchor_store(store.clone());
    zeph_session::log::configure_anchor_store(store);
}

/// Outcome of one [`run_anchor_sweep`] pass, for logging and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnchorSweepReport {
    /// Orphaned anchors hard-deleted this sweep (backing file/session directory absent for at
    /// least `ORPHAN_REAP_GRACE_MS` since first observed absent).
    pub orphans_reaped: usize,
    /// Anchors newly flagged orphaned this sweep (backing file/session directory absent for the
    /// first time — `orphaned_since` stamped, not yet reaped; issue #6462).
    pub orphans_stamped: usize,
    /// Previously orphan-stamped anchors whose backing file/session directory reappeared this
    /// sweep (`orphaned_since` cleared — self-heal; issue #6462).
    pub orphans_cleared: usize,
    /// Session anchors evicted because the session-anchor count exceeded the cap.
    pub evicted_for_cap: usize,
}

/// Grace window a backing file/session-directory must stay *sustained-absent* before the
/// reconcile sweep hard-deletes its anchor (issue #6462). Wall-clock milliseconds, not a
/// sweep-cycle count, so it is independent of [`SWEEP_INTERVAL`] and survives process restarts —
/// see the module docs for the downgrade-resistance rationale.
const ORPHAN_REAP_GRACE_MS: u64 = 24 * 60 * 60 * 1000;

/// Per-key disposition decided by [`plan_orphan_action`] for a single absent-file anchor (issue
/// #6462). Kept separate from the stamp/clear/keep cases (which also depend on cap-eviction
/// bookkeeping) so the grace-window boundary condition itself is a small, directly unit-testable
/// pure function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrphanAction {
    /// First sweep to observe the file absent — stamp `orphaned_since = now`.
    Stamp,
    /// Already stamped, still within `ORPHAN_REAP_GRACE_MS` — no write this sweep.
    WithinGrace,
    /// Already stamped, sustained absence has reached or exceeded the grace window — hard-delete.
    Reap,
}

/// Decide the orphan disposition for a backing-file-absent anchor, given its current
/// `orphaned_since` stamp (`None` if never before observed absent) and `now`.
///
/// `now.saturating_sub(orphaned_since)` guards against clock skew: a backward clock jump (or a
/// future-dated stamp) yields an elapsed time of `0`, which is always `< ORPHAN_REAP_GRACE_MS`,
/// so skew fails safe toward [`OrphanAction::WithinGrace`], never a premature
/// [`OrphanAction::Reap`].
fn plan_orphan_action(orphaned_since: Option<u64>, now: u64) -> OrphanAction {
    match orphaned_since {
        None => OrphanAction::Stamp,
        Some(since) if now.saturating_sub(since) >= ORPHAN_REAP_GRACE_MS => OrphanAction::Reap,
        Some(_) => OrphanAction::WithinGrace,
    }
}

/// The vault writes/removals decided by the read-only planning pass of [`run_anchor_sweep`],
/// before any lock is taken.
#[derive(Default)]
struct SweepPlan {
    orphans_to_reap: Vec<String>,
    orphans_to_stamp: Vec<(String, String)>,
    orphans_to_clear: Vec<(String, String)>,
    evictions: Vec<String>,
}

/// Filesystem stats + JSON decode over `snapshot` — no vault lock held. Split out of
/// [`run_anchor_sweep`] to keep that function's orchestration (lock/save) readable on its own.
fn plan_sweep(
    snapshot: &[(String, String)],
    transcript_dir: &Path,
    sessions_data_dir: &Path,
    max_session_anchors: usize,
    now: u64,
) -> SweepPlan {
    let mut plan = SweepPlan::default();
    let mut live_session_anchors: Vec<(String, u64)> = Vec::new();

    for (key, json) in snapshot {
        let Some((subsystem, file_id)) = parse_anchor_key(key) else {
            continue; // not a well-formed anchor key — leave it alone
        };
        let file_id_str = String::from_utf8_lossy(&file_id).into_owned();
        let exists = match subsystem {
            AnchorSubsystem::SubagentTranscript => {
                transcript_dir.join(format!("{file_id_str}.jsonl")).exists()
            }
            AnchorSubsystem::SessionLog => {
                zeph_session::session_dir(sessions_data_dir, &file_id_str).exists()
            }
        };
        // A malformed anchor value cannot carry a grace timestamp — reap immediately if absent
        // (matching pre-#6462 behavior for corrupted entries; never attacker-reachable, since
        // only the age-key holder can write a vault value at all), otherwise treat as a live
        // entry with an unknown `written_at`.
        let Some(anchor) = serde_json::from_str::<Anchor>(json).ok() else {
            if exists {
                if subsystem == AnchorSubsystem::SessionLog {
                    live_session_anchors.push((key.clone(), 0));
                }
            } else {
                plan.orphans_to_reap.push(key.clone());
            }
            continue;
        };

        if exists {
            // File present: clear a stale `orphaned_since` stamp (self-heal). A steady-state
            // anchor (never orphaned) is left untouched — no extra write in the common case.
            if anchor.orphaned_since.is_some() {
                let mut cleared = anchor.clone();
                cleared.orphaned_since = None;
                if let Ok(new_json) = serde_json::to_string(&cleared) {
                    plan.orphans_to_clear.push((key.clone(), new_json));
                }
            }
            if subsystem == AnchorSubsystem::SessionLog {
                live_session_anchors.push((key.clone(), anchor.written_at));
            }
            continue;
        }

        match plan_orphan_action(anchor.orphaned_since, now) {
            OrphanAction::Reap => plan.orphans_to_reap.push(key.clone()),
            OrphanAction::WithinGrace => {}
            OrphanAction::Stamp => {
                let mut stamped = anchor;
                stamped.orphaned_since = Some(now);
                if let Ok(new_json) = serde_json::to_string(&stamped) {
                    plan.orphans_to_stamp.push((key.clone(), new_json));
                }
            }
        }
    }

    if live_session_anchors.len() > max_session_anchors {
        live_session_anchors.sort_by_key(|(_, written_at)| *written_at);
        let to_evict = live_session_anchors.len() - max_session_anchors;
        plan.evictions.extend(
            live_session_anchors
                .into_iter()
                .take(to_evict)
                .map(|(key, _)| key),
        );
    }

    plan
}

/// Run one reconcile-and-cap pass over the vault's `ZEPH_HISTORY_ANCHOR_*` keys (issue #6449).
///
/// `now` is the caller-supplied current wall-clock time (Unix milliseconds, matching
/// [`Anchor::written_at`]/[`Anchor::orphaned_since`]) — injected rather than read via
/// `SystemTime::now()` internally so tests can cross the `ORPHAN_REAP_GRACE_MS` boundary
/// deterministically. Real callers ([`spawn_anchor_sweep`]) pass
/// [`zeph_common::anchor::now_unix_millis`].
///
/// Synchronous and blocking (file-existence checks + vault mutation) — callers on an async path
/// must dispatch this through [`TaskSupervisor::spawn_blocking`], never call it inline.
///
/// The vault **write** lock is held only for the final targeted `set_secret_mut`/
/// `remove_secret_mut` calls plus `save()` — every `Path::exists()` stat and anchor JSON decode
/// runs against a snapshot taken under a brief **read** lock, released before any filesystem I/O
/// (see `plan_sweep`). Holding the write lock across `O(total anchors)` stat syscalls would
/// otherwise stall every concurrent anchor `get`/`put` for the duration of the sweep (perf
/// finding, same root cause class as issue #6449 M1). This is still "in place" per
/// M-sweep-inplace: every mutation is always by targeted key (`set_secret_mut`/
/// `remove_secret_mut`), never a whole-map snapshot-modify-writeback, so a concurrent `put` for
/// an unrelated key is never clobbered.
///
/// # Errors
///
/// Returns a description string if the final `save()` (only performed when at least one entry
/// was written or removed) fails.
pub fn run_anchor_sweep(
    vault: &Arc<StdRwLock<AgeVaultProvider>>,
    transcript_dir: &Path,
    sessions_data_dir: &Path,
    max_session_anchors: usize,
    now: u64,
) -> Result<AnchorSweepReport, String> {
    let _span = tracing::info_span!("core.anchor.sweep", max_session_anchors).entered();

    // Step 1: snapshot every anchor key + raw value under a brief READ lock — no I/O here.
    let snapshot: Vec<(String, String)> = {
        let guard = vault
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .list_keys()
            .into_iter()
            .filter(|k| k.starts_with(zeph_common::anchor::ANCHOR_KEY_PREFIX))
            .filter_map(|k| guard.get(k).map(|v| (k.to_owned(), v.to_owned())))
            .collect()
    }; // read guard dropped here, before any filesystem stat

    // Step 2: decide what to write/remove — filesystem stats and JSON decode, no lock held.
    let plan = plan_sweep(
        &snapshot,
        transcript_dir,
        sessions_data_dir,
        max_session_anchors,
        now,
    );

    // Step 3: acquire the WRITE lock only for the targeted writes/removes + one save().
    let mut report = AnchorSweepReport::default();
    let has_writes = !plan.orphans_to_reap.is_empty()
        || !plan.orphans_to_stamp.is_empty()
        || !plan.orphans_to_clear.is_empty()
        || !plan.evictions.is_empty();
    if has_writes {
        let mut guard = vault
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, json) in plan.orphans_to_stamp {
            if guard.set_secret_mut(key, json, true).is_ok() {
                report.orphans_stamped += 1;
            }
        }
        for (key, json) in plan.orphans_to_clear {
            if guard.set_secret_mut(key, json, true).is_ok() {
                report.orphans_cleared += 1;
            }
        }
        for key in &plan.orphans_to_reap {
            if guard.remove_secret_mut(key) {
                report.orphans_reaped += 1;
            }
        }
        for key in &plan.evictions {
            if guard.remove_secret_mut(key) {
                report.evicted_for_cap += 1;
            }
        }
        guard.save().map_err(|e| e.to_string())?;
    }

    Ok(report)
}

/// Low-frequency periodic tick for [`run_anchor_sweep`] (issue #6449 rev2 critic: startup +
/// periodic is required — a long-running server/gateway process would otherwise let the vault
/// map exceed the cap unbounded between restarts).
const SWEEP_INTERVAL: Duration = Duration::from_hours(1);

/// Spawn the reconcile-and-cap sweep as a named, supervised task (issue #6449): runs once
/// immediately, then on an hourly interval thereafter, for the life of the process.
///
/// Each tick's blocking work is dispatched through [`TaskSupervisor::spawn_blocking`] from
/// within the supervised task's own async body — never inline on the tokio worker thread.
pub fn spawn_anchor_sweep(
    supervisor: &TaskSupervisor,
    vault: Arc<StdRwLock<AgeVaultProvider>>,
    transcript_dir: PathBuf,
    sessions_data_dir: PathBuf,
    max_session_anchors: usize,
) {
    let sweep_supervisor = supervisor.clone();
    supervisor.spawn(TaskDescriptor {
        name: "anchor-reconcile-sweep",
        restart: RestartPolicy::RunOnce,
        factory: move || {
            let vault = Arc::clone(&vault);
            let transcript_dir = transcript_dir.clone();
            let sessions_data_dir = sessions_data_dir.clone();
            let blocking = sweep_supervisor.clone();
            async move {
                let tick = move |vault: Arc<StdRwLock<AgeVaultProvider>>,
                                 transcript_dir: PathBuf,
                                 sessions_data_dir: PathBuf,
                                 blocking: TaskSupervisor| async move {
                    let handle =
                        blocking.spawn_blocking(Arc::from("anchor-sweep-tick"), move || {
                            run_anchor_sweep(
                                &vault,
                                &transcript_dir,
                                &sessions_data_dir,
                                max_session_anchors,
                                zeph_common::anchor::now_unix_millis(),
                            )
                        });
                    match handle.join().await {
                        Ok(Ok(report))
                            if report.orphans_reaped > 0
                                || report.orphans_stamped > 0
                                || report.orphans_cleared > 0
                                || report.evicted_for_cap > 0 =>
                        {
                            tracing::info!(
                                orphans_reaped = report.orphans_reaped,
                                orphans_stamped = report.orphans_stamped,
                                orphans_cleared = report.orphans_cleared,
                                evicted_for_cap = report.evicted_for_cap,
                                "anchor reconcile-and-cap sweep completed"
                            );
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!(error = %e, "anchor sweep failed"),
                        Err(e) => tracing::warn!(error = %e, "anchor sweep task failed"),
                    }
                };

                tick(
                    Arc::clone(&vault),
                    transcript_dir.clone(),
                    sessions_data_dir.clone(),
                    blocking.clone(),
                )
                .await;

                let mut interval = tokio::time::interval(SWEEP_INTERVAL);
                interval.tick().await; // first tick fires immediately; already ran once above
                loop {
                    interval.tick().await;
                    tick(
                        Arc::clone(&vault),
                        transcript_dir.clone(),
                        sessions_data_dir.clone(),
                        blocking.clone(),
                    )
                    .await;
                }
            }
        },
    });
}

/// Resolve the vault-stored durable integrity seal marker + grandfather set (issue #6449) for
/// `crate::agent::durable_bootstrap` to attach to a `zeph_durable::backend::LocalBackend`.
///
/// Presence of `ZEPH_DURABLE_INTEGRITY_SEALED` means sealed; its value (if any) is a
/// human-readable timestamp for `doctor` display only, never on the security boundary.
#[must_use]
pub fn load_durable_integrity_seal(
    provider: &AgeVaultProvider,
) -> (bool, HashSet<zeph_durable::ExecutionId>) {
    let sealed = provider.get(DURABLE_INTEGRITY_SEALED_KEY).is_some();
    let grandfather = provider
        .get(DURABLE_INTEGRITY_GRANDFATHER_KEY)
        .map(parse_grandfather_set)
        .unwrap_or_default();
    (sealed, grandfather)
}

/// Vault secret name whose *presence* marks a durable backend sealed against pre-feature
/// integrity-row absence (issue #6449). The value, if any, is a display-only timestamp.
pub const DURABLE_INTEGRITY_SEALED_KEY: &str = "ZEPH_DURABLE_INTEGRITY_SEALED";

/// Vault secret name for the comma-separated set of execution IDs grandfathered past the seal
/// (issue #6449).
pub const DURABLE_INTEGRITY_GRANDFATHER_KEY: &str = "ZEPH_DURABLE_INTEGRITY_GRANDFATHER";

/// Parse a comma-separated grandfather-set vault value into execution IDs, skipping any entry
/// that fails to parse (defensive — a hand-edited vault value should degrade, not panic).
#[must_use]
pub fn parse_grandfather_set(value: &str) -> HashSet<zeph_durable::ExecutionId> {
    value
        .split(',')
        .filter_map(|s| zeph_durable::ExecutionId::parse_str(s.trim()).ok())
        .collect()
}

/// Render a grandfather set back to the vault's comma-separated storage format, merging with
/// any IDs already present in `existing` (each grandfathered id is a permanent addition — see
/// the module docs on `zeph_durable::backend::local::LocalBackend::with_grandfather` for the
/// accepted residual this carries).
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn render_grandfather_set(
    existing: &str,
    new_ids: &HashSet<zeph_durable::ExecutionId>,
) -> String {
    let mut all: HashSet<zeph_durable::ExecutionId> = parse_grandfather_set(existing);
    all.extend(new_ids.iter().copied());
    let mut ids: Vec<String> = all.iter().map(|id| id.as_uuid().to_string()).collect();
    ids.sort_unstable();
    ids.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;
    use zeph_common::anchor::AnchorSubsystem;

    fn test_vault(dir: &Path) -> Arc<StdRwLock<AgeVaultProvider>> {
        AgeVaultProvider::init_vault(dir).unwrap();
        let provider =
            AgeVaultProvider::load(&dir.join("vault-key.txt"), &dir.join("secrets.age")).unwrap();
        Arc::new(StdRwLock::new(provider))
    }

    #[tokio::test]
    async fn put_get_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let vault = test_vault(dir.path());
        let supervisor = TaskSupervisor::new(CancellationToken::new());
        let store = AgeVaultAnchorStore::new(vault, supervisor);

        let head = zeph_common::hash_chain::chain_next(
            &zeph_common::hash_chain::ChainKey::new([1u8; 32]),
            &zeph_common::hash_chain::genesis(
                &zeph_common::hash_chain::ChainKey::new([1u8; 32]),
                "d",
                b"f",
                0,
            ),
            b"content",
        );
        let anchor = Anchor::new(0, 5, head);

        assert!(
            store
                .get(AnchorSubsystem::SubagentTranscript, b"task-1")
                .await
                .unwrap()
                .is_none()
        );

        store
            .put(
                AnchorSubsystem::SubagentTranscript,
                b"task-1",
                anchor.clone(),
            )
            .await
            .unwrap();

        let fetched = store
            .get(AnchorSubsystem::SubagentTranscript, b"task-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.count, 5);
        assert_eq!(fetched.head_hex, anchor.head_hex);

        store
            .delete(AnchorSubsystem::SubagentTranscript, b"task-1")
            .await
            .unwrap();
        assert!(
            store
                .get(AnchorSubsystem::SubagentTranscript, b"task-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Regression test for issue #6449 M1: an external `tokio::time::timeout` wrapping
    /// `AnchorStore::get` must actually fire while the vault write lock is held (e.g. a
    /// concurrent `put`/sweep `save()` in progress), never hang past it. Before the fix,
    /// `get()` called `get_sync` synchronously in its own body before constructing the
    /// returned future, so `store.get(..)` fully resolved (or deadlocked) as a plain
    /// expression *before* `tokio::time::timeout` was ever invoked — the timeout wrapped an
    /// already-resolved future with no suspension point to race against. With the bug present,
    /// this test would hang forever (the writer is never released until *after* the awaited
    /// call returns), not just fail an assertion — the outer `tokio::time::timeout` around the
    /// whole test body is a safety net so a regression fails loudly instead of hanging CI.
    #[tokio::test]
    async fn get_is_a_real_suspension_point_and_honors_an_external_timeout() {
        let outcome = tokio::time::timeout(Duration::from_secs(5), async {
            let dir = tempfile::tempdir().unwrap();
            let vault = test_vault(dir.path());
            let supervisor = TaskSupervisor::new(CancellationToken::new());
            let store = AgeVaultAnchorStore::new(Arc::clone(&vault), supervisor);

            // Hold the write lock on a background OS thread, simulating a slow `save()` (a
            // concurrent `put`/`delete`/sweep) in progress.
            let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let vault_for_holder = Arc::clone(&vault);
            let holder = std::thread::spawn(move || {
                let _guard = vault_for_holder.write().unwrap();
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap(); // block until the test tells us to release
            });
            held_rx.recv().unwrap(); // wait until the writer genuinely holds the lock

            let result = tokio::time::timeout(
                Duration::from_millis(100),
                store.get(AnchorSubsystem::SubagentTranscript, b"whatever"),
            )
            .await;
            assert!(
                result.is_err(),
                "the external 100ms timeout must fire while the write lock is held — a real \
                 suspension point must exist for it to race against"
            );

            release_tx.send(()).unwrap();
            holder.join().unwrap();
        })
        .await;
        assert!(
            outcome.is_ok(),
            "test itself must not hang past its 5s safety-net timeout"
        );
    }

    /// Regression test for issue #6449 M1 on the transcript (sync) read path: `get_sync` must
    /// fail closed under sustained vault write-lock contention rather than blocking forever.
    #[test]
    fn get_sync_bounded_times_out_under_contention_instead_of_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let vault = test_vault(dir.path());
        let supervisor = TaskSupervisor::new(CancellationToken::new());
        let store = AgeVaultAnchorStore::new(Arc::clone(&vault), supervisor);

        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let vault_for_holder = Arc::clone(&vault);
        let holder = std::thread::spawn(move || {
            let _guard = vault_for_holder.write().unwrap();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.recv().unwrap();

        let start = std::time::Instant::now();
        let result = store.get_sync_bounded(
            AnchorSubsystem::SubagentTranscript,
            b"whatever",
            Duration::from_millis(50),
        );
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "must fail closed, not hang, under contention"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must return close to the 50ms bound, not block indefinitely (took {elapsed:?})"
        );

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    /// Once the writer releases, a bounded lookup must still succeed normally (the bound only
    /// gates the contended case, never rejects an uncontended or since-released read).
    #[test]
    fn get_sync_bounded_succeeds_once_contention_clears() {
        let dir = tempfile::tempdir().unwrap();
        let vault = test_vault(dir.path());
        let supervisor = TaskSupervisor::new(CancellationToken::new());
        let store = AgeVaultAnchorStore::new(Arc::clone(&vault), supervisor);

        let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
        let vault_for_holder = Arc::clone(&vault);
        let holder = std::thread::spawn(move || {
            let _guard = vault_for_holder.write().unwrap();
            std::thread::sleep(Duration::from_millis(50));
            held_tx.send(()).unwrap();
        });

        let result = store.get_sync_bounded(
            AnchorSubsystem::SubagentTranscript,
            b"whatever",
            Duration::from_secs(2),
        );
        assert!(
            result.is_ok(),
            "a bound long enough to outlast contention must still succeed"
        );

        held_rx.recv().unwrap();
        holder.join().unwrap();
    }

    fn sample_anchor(count: u64) -> Anchor {
        let key = zeph_common::hash_chain::ChainKey::new([2u8; 32]);
        let base = zeph_common::hash_chain::genesis(&key, "d", b"f", 0);
        let head = zeph_common::hash_chain::chain_next(&key, &base, b"c");
        Anchor::new(0, count, head)
    }

    #[test]
    fn plan_orphan_action_stamps_on_first_observation() {
        assert_eq!(plan_orphan_action(None, 1_000), OrphanAction::Stamp);
    }

    #[test]
    fn plan_orphan_action_boundary_grace_minus_one_within_grace_at_grace_reaps() {
        let since = 1_000u64;
        assert_eq!(
            plan_orphan_action(Some(since), since + ORPHAN_REAP_GRACE_MS - 1),
            OrphanAction::WithinGrace
        );
        assert_eq!(
            plan_orphan_action(Some(since), since + ORPHAN_REAP_GRACE_MS),
            OrphanAction::Reap
        );
    }

    #[test]
    fn plan_orphan_action_saturates_on_clock_skew_instead_of_reaping() {
        let since = 1_000u64;
        assert_eq!(
            plan_orphan_action(Some(since), since - 1),
            OrphanAction::WithinGrace,
            "now < orphaned_since must never underflow or reap"
        );
    }

    /// Write a raw anchor secret directly into the vault, bypassing the async `AnchorStore`
    /// trait (tests operate on the synchronous `AgeVaultProvider` directly).
    fn write_anchor(
        vault: &Arc<StdRwLock<AgeVaultProvider>>,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
        anchor: &Anchor,
    ) {
        let mut guard = vault.write().unwrap();
        let key = zeph_common::anchor::anchor_key(subsystem, file_id);
        let json = serde_json::to_string(anchor).unwrap();
        guard.set_secret_mut(key, json, true).unwrap();
        guard.save().unwrap();
    }

    fn read_anchor(
        vault: &Arc<StdRwLock<AgeVaultProvider>>,
        subsystem: AnchorSubsystem,
        file_id: &[u8],
    ) -> Option<Anchor> {
        let guard = vault.read().unwrap();
        let key = zeph_common::anchor::anchor_key(subsystem, file_id);
        guard.get(&key).map(|v| serde_json::from_str(v).unwrap())
    }

    /// Issue #6462 two-phase regression: an orphan is stamped, not reaped, on the sweep that
    /// first observes its file absent; only a later sweep — after the grace window has fully
    /// elapsed — hard-deletes it.
    #[test]
    fn sweep_stamps_then_reaps_orphan_transcript_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let vault = test_vault(dir.path());
        let transcript_dir = dir.path().join("transcripts");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        write_anchor(
            &vault,
            AnchorSubsystem::SubagentTranscript,
            b"gone",
            &sample_anchor(1),
        );

        let t0 = 1_000_000u64;
        let first = run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, t0).unwrap();
        assert_eq!(first.orphans_stamped, 1, "first sweep must stamp, not reap");
        assert_eq!(first.orphans_reaped, 0);
        assert_eq!(
            vault.read().unwrap().list_keys().len(),
            1,
            "anchor must survive the first sweep"
        );
        let stamped = read_anchor(&vault, AnchorSubsystem::SubagentTranscript, b"gone").unwrap();
        assert_eq!(stamped.orphaned_since, Some(t0));

        let after_grace = t0 + ORPHAN_REAP_GRACE_MS;
        let second =
            run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, after_grace).unwrap();
        assert_eq!(
            second.orphans_reaped, 1,
            "second sweep past grace must reap"
        );
        assert!(vault.read().unwrap().list_keys().is_empty());
    }

    /// #6463: mirrors the transcript two-phase test for `AnchorSubsystem::SessionLog`, and
    /// confirms a sibling *legitimate* session's anchor is untouched throughout — the orphan
    /// reap of one identity must never cause a false read-time tamper signal on another.
    #[test]
    fn sweep_stamps_then_reaps_orphan_session_log_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let vault = test_vault(dir.path());
        let transcript_dir = dir.path().join("transcripts");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // A legitimate, still-live session — must never be affected by the orphan below.
        std::fs::create_dir_all(zeph_session::session_dir(&sessions_dir, "legit")).unwrap();
        write_anchor(
            &vault,
            AnchorSubsystem::SessionLog,
            b"legit",
            &sample_anchor(1),
        );

        // The orphan: no backing session directory at all.
        write_anchor(
            &vault,
            AnchorSubsystem::SessionLog,
            b"gone",
            &sample_anchor(1),
        );

        let t0 = 2_000_000u64;
        let first = run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, t0).unwrap();
        assert_eq!(first.orphans_stamped, 1);
        assert_eq!(first.orphans_reaped, 0);
        assert!(
            read_anchor(&vault, AnchorSubsystem::SessionLog, b"gone")
                .unwrap()
                .orphaned_since
                .is_some()
        );
        assert!(
            read_anchor(&vault, AnchorSubsystem::SessionLog, b"legit")
                .unwrap()
                .orphaned_since
                .is_none(),
            "a live sibling session must never be stamped"
        );

        let after_grace = t0 + ORPHAN_REAP_GRACE_MS;
        let second =
            run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, after_grace).unwrap();
        assert_eq!(second.orphans_reaped, 1);
        assert!(
            read_anchor(&vault, AnchorSubsystem::SessionLog, b"gone").is_none(),
            "the orphan must be gone"
        );
        assert!(
            read_anchor(&vault, AnchorSubsystem::SessionLog, b"legit").is_some(),
            "the legitimate session's anchor must survive, undisturbed"
        );
    }

    /// Self-heal: if the backing file reappears before the grace window elapses,
    /// `orphaned_since` is cleared and the anchor is never reaped.
    #[test]
    fn sweep_self_heals_when_orphaned_file_reappears_before_grace_elapses() {
        let dir = tempfile::tempdir().unwrap();
        let vault = test_vault(dir.path());
        let transcript_dir = dir.path().join("transcripts");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        write_anchor(
            &vault,
            AnchorSubsystem::SubagentTranscript,
            b"flaky",
            &sample_anchor(1),
        );

        let t0 = 1_000_000u64;
        let first = run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, t0).unwrap();
        assert_eq!(first.orphans_stamped, 1);

        // The file reappears (e.g. a concurrent re-finalize) before the grace window elapses.
        std::fs::write(transcript_dir.join("flaky.jsonl"), b"").unwrap();
        let heal_time = t0 + 1;
        let second =
            run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, heal_time).unwrap();
        assert_eq!(second.orphans_cleared, 1);
        assert_eq!(second.orphans_reaped, 0);
        let healed = read_anchor(&vault, AnchorSubsystem::SubagentTranscript, b"flaky").unwrap();
        assert_eq!(
            healed.orphaned_since, None,
            "self-heal must clear the stamp"
        );

        // Even long past what would have been the original grace deadline, the anchor survives
        // while the file stays present — but this alone doesn't prove the clock *reset* (a file
        // that still exists is never reaped regardless of any stamp). Prove the reset directly:
        // delete the file again, at a time already past the ORIGINAL (t0-based) grace deadline,
        // and confirm the sweep stamps a *fresh* `orphaned_since` instead of reaping immediately
        // off the stale t0 clock.
        let far_future = t0 + ORPHAN_REAP_GRACE_MS * 10;
        let third =
            run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, far_future).unwrap();
        assert_eq!(third.orphans_reaped, 0);
        assert!(
            vault
                .read()
                .unwrap()
                .list_keys()
                .contains(&"ZEPH_HISTORY_ANCHOR_SUBAGENT_flaky")
        );

        std::fs::remove_file(transcript_dir.join("flaky.jsonl")).unwrap();
        let re_orphan_time = far_future + 1;
        let fourth =
            run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, re_orphan_time).unwrap();
        assert_eq!(
            fourth.orphans_stamped, 1,
            "a fresh absence must be stamped again, not treated as already-expired leftover \
             from the pre-heal clock"
        );
        assert_eq!(
            fourth.orphans_reaped, 0,
            "must not reap immediately — if the clock had merely paused instead of resetting, \
             this sweep (already well past t0 + GRACE) would incorrectly reap right away"
        );
        let re_stamped =
            read_anchor(&vault, AnchorSubsystem::SubagentTranscript, b"flaky").unwrap();
        assert_eq!(
            re_stamped.orphaned_since,
            Some(re_orphan_time),
            "orphaned_since must restart from the new absence time, not resume counting from t0"
        );

        // The new clock must independently reach its own grace deadline.
        let past_new_grace = re_orphan_time + ORPHAN_REAP_GRACE_MS;
        let fifth =
            run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, past_new_grace).unwrap();
        assert_eq!(fifth.orphans_reaped, 1);
        assert!(vault.read().unwrap().list_keys().is_empty());
    }

    /// N-1 vs N boundary: one millisecond short of the grace window must not reap; reaching (or
    /// exceeding) it must.
    #[test]
    fn sweep_orphan_reap_boundary_grace_minus_one_not_reaped_grace_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let vault = test_vault(dir.path());
        let transcript_dir = dir.path().join("transcripts");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        write_anchor(
            &vault,
            AnchorSubsystem::SubagentTranscript,
            b"gone",
            &sample_anchor(1),
        );

        let t0 = 1_000_000u64;
        run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, t0).unwrap();

        let just_under = run_anchor_sweep(
            &vault,
            &transcript_dir,
            &sessions_dir,
            512,
            t0 + ORPHAN_REAP_GRACE_MS - 1,
        )
        .unwrap();
        assert_eq!(
            just_under.orphans_reaped, 0,
            "must not reap before the grace window elapses"
        );
        assert_eq!(vault.read().unwrap().list_keys().len(), 1);

        let at_grace = run_anchor_sweep(
            &vault,
            &transcript_dir,
            &sessions_dir,
            512,
            t0 + ORPHAN_REAP_GRACE_MS,
        )
        .unwrap();
        assert_eq!(
            at_grace.orphans_reaped, 1,
            "must reap once the grace window is reached"
        );
        assert!(vault.read().unwrap().list_keys().is_empty());
    }

    /// Clock skew: a backward clock jump (`now` older than the recorded `orphaned_since`) must
    /// never panic or underflow, and must fail safe toward keeping the anchor.
    #[test]
    fn sweep_orphan_reap_saturates_instead_of_underflowing_on_clock_skew() {
        let dir = tempfile::tempdir().unwrap();
        let vault = test_vault(dir.path());
        let transcript_dir = dir.path().join("transcripts");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        write_anchor(
            &vault,
            AnchorSubsystem::SubagentTranscript,
            b"gone",
            &sample_anchor(1),
        );

        let t0 = 1_000_000u64;
        run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, t0).unwrap();

        // Clock jumps backward relative to the stamped `orphaned_since`.
        let report = run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, t0 - 1).unwrap();
        assert_eq!(
            report.orphans_reaped, 0,
            "must fail safe, never reap on a clock regression"
        );
        assert_eq!(vault.read().unwrap().list_keys().len(), 1);
    }

    #[test]
    fn sweep_keeps_anchor_whose_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let vault = test_vault(dir.path());
        let transcript_dir = dir.path().join("transcripts");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(transcript_dir.join("alive.jsonl"), b"").unwrap();

        {
            let mut guard = vault.write().unwrap();
            let key =
                zeph_common::anchor::anchor_key(AnchorSubsystem::SubagentTranscript, b"alive");
            let json = serde_json::to_string(&sample_anchor(1)).unwrap();
            guard.set_secret_mut(key, json, true).unwrap();
            guard.save().unwrap();
        }

        let report =
            run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 512, 1_000_000).unwrap();
        assert_eq!(report.orphans_reaped, 0);
        assert_eq!(vault.read().unwrap().list_keys().len(), 1);
    }

    /// S3 regression: eviction must order by the anchor-embedded `written_at`, never by
    /// filesystem mtime (attacker-writable).
    #[test]
    fn sweep_caps_session_anchors_by_embedded_written_at_not_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let vault = test_vault(dir.path());
        let transcript_dir = dir.path().join("transcripts");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Three sessions, each with a session directory on disk (so none are orphans) and an
        // anchor whose `written_at` we control directly (bypassing wall-clock timing).
        // Directories are deliberately created in the OPPOSITE order of `written_at`
        // ("s-new" first, "s-old" last), so filesystem mtime order is inverted relative to
        // anchor-content order — simulating an attacker who manipulates mtime (or simply the
        // natural case of a session directory touched more recently than its anchor's last
        // `written_at`). If eviction used mtime, it would wrongly evict "s-new" or "s-mid"
        // instead of the true oldest, "s-old".
        let mut anchors_by_name = Vec::new();
        for (name, written_at) in [("s-new", 300u64), ("s-mid", 200), ("s-old", 100)] {
            let session_path = zeph_session::session_dir(&sessions_dir, name);
            std::fs::create_dir_all(&session_path).unwrap();
            let mut anchor = sample_anchor(1);
            anchor.written_at = written_at;
            anchors_by_name.push((name, anchor));
        }

        {
            let mut guard = vault.write().unwrap();
            for (name, anchor) in &anchors_by_name {
                let key =
                    zeph_common::anchor::anchor_key(AnchorSubsystem::SessionLog, name.as_bytes());
                let json = serde_json::to_string(anchor).unwrap();
                guard.set_secret_mut(key, json, true).unwrap();
            }
            guard.save().unwrap();
        }

        // Cap at 2: must evict "s-old" (written_at=100), the true oldest by anchor content —
        // NOT whichever mtime manipulation would suggest.
        let report =
            run_anchor_sweep(&vault, &transcript_dir, &sessions_dir, 2, 1_000_000).unwrap();
        assert_eq!(report.evicted_for_cap, 1);

        let remaining_keys: Vec<String> = vault
            .read()
            .unwrap()
            .list_keys()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let old_key = zeph_common::anchor::anchor_key(AnchorSubsystem::SessionLog, b"s-old");
        let mid_key = zeph_common::anchor::anchor_key(AnchorSubsystem::SessionLog, b"s-mid");
        let new_key = zeph_common::anchor::anchor_key(AnchorSubsystem::SessionLog, b"s-new");
        assert!(
            !remaining_keys.contains(&old_key),
            "the true oldest must be evicted"
        );
        assert!(remaining_keys.contains(&mid_key));
        assert!(remaining_keys.contains(&new_key));
    }

    #[test]
    fn grandfather_set_round_trips_and_merges() {
        let a = zeph_durable::ExecutionId::new();
        let b = zeph_durable::ExecutionId::new();
        let rendered = render_grandfather_set("", &HashSet::from([a]));
        let parsed = parse_grandfather_set(&rendered);
        assert!(parsed.contains(&a));

        // Merging must be additive — an existing id is never dropped.
        let rendered2 = render_grandfather_set(&rendered, &HashSet::from([b]));
        let parsed2 = parse_grandfather_set(&rendered2);
        assert!(parsed2.contains(&a));
        assert!(parsed2.contains(&b));
    }
}
