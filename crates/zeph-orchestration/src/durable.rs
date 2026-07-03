// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Durable P2 adapter: journaling and restoring the replan budget across `/plan resume` cycles.
//!
//! Each time a DAG is paused the live replan counters (`task_replan_counts`,
//! `global_replan_count`, `predicate_replans_used`, `predicate_reasons`, `lineage_chains`) are
//! serialized into a [`ReplanBudgetSnapshot`] and written as an `EffectClass::Idempotent`
//! step to a dedicated [`ExecutionId`] derived from `(graph_id, save_generation)`.  A fresh
//! execution is opened for *each* save, so a plan that pauses → resumes → pauses again does not
//! replay the stale first snapshot (the generation counter differentiates the two executions).
//!
//! On `/plan resume`, [`restore_budget`] reads back the highest-generation snapshot and hands it
//! to [`crate::DagScheduler::resume_from_durable`], which populates the counters before the scheduler
//! starts dispatching tasks again.  When the flag is off (or no snapshot exists), the scheduler
//! falls back to zeroing all counters — identical to the pre-durable behaviour.
//!
//! # Layer boundary
//!
//! This module imports only `zeph_durable` and `zeph_config` from infrastructure crates.
//! `zeph-core` types are never imported here; the call sites in `plan.rs` supply the
//! cipher and config.  (INV-1: no business-logic in L0 infrastructure.)

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::instrument;
use zeph_durable::DurableError;
use zeph_durable::{
    DurableBackendEnum, DurableConfig, DurableContext, ExecutionId, ExecutionKind, Journal as _,
    JournalWriterHandle, StepDescriptor,
};

use super::graph::{GraphId, TaskId};
use super::lineage::{ErrorLineage, LineageEntry, LineageKind};

// ─── domain-separation label used for all P2 budget executions ────────────────
const BUDGET_DOMAIN: &[u8] = b"zeph.orch.budget.v1";

// ─── fixed step name, constant so idem_key is stable per-generation ───────────
const BUDGET_STEP_NAME: &str = "replan_budget";
const BUDGET_STEP_FP: &[u8] = b"orch:replan_budget:v1";

// ─── DTO types ────────────────────────────────────────────────────────────────

/// Serde-able projection of a single [`LineageEntry`].
///
/// [`LineageKind`] has no serde derives; this DTO flattens the single v1 variant to
/// `error_class: String`.  If a second `LineageKind` variant is added post-v1 the
/// exhaustive match in `From<&LineageEntry>` becomes a compile error — the intended tripwire.
///
/// # TODO(post-v1)
///
/// When `LineageKind` gains additional variants, replace `error_class` with a serde-tagged
/// enum so no information is silently lost during round-trips.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineageEntryDto {
    /// The task whose failure or weak output triggered this entry.
    pub task_id: TaskId,
    /// Short error class label (e.g. `"timeout"`, `"llm_error"`).
    pub error_class: String,
    /// Milliseconds since UNIX epoch when the entry was recorded.
    pub ts_ms: u64,
}

impl From<&LineageEntry> for LineageEntryDto {
    fn from(e: &LineageEntry) -> Self {
        // Single v1 variant — match is exhaustive; adding WeakOutput here is the tripwire.
        let error_class = match &e.kind {
            LineageKind::Failed { error_class } => error_class.clone(),
        };
        Self {
            task_id: e.task_id,
            error_class,
            ts_ms: e.ts_ms,
        }
    }
}

/// Serde-able projection of an [`ErrorLineage`] chain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ErrorLineageDto {
    /// Ordered entries (earliest first).
    pub entries: Vec<LineageEntryDto>,
}

impl From<&ErrorLineage> for ErrorLineageDto {
    fn from(src: &ErrorLineage) -> Self {
        Self {
            entries: src.entries().iter().map(LineageEntryDto::from).collect(),
        }
    }
}

impl ErrorLineage {
    /// Reconstruct an [`ErrorLineage`] from a persisted [`ErrorLineageDto`].
    ///
    /// Used by the durable resume path to rebuild the lineage chains that were journaled at
    /// the last pause so the cascade detector resumes with accurate history.
    #[must_use]
    pub fn from_dto(dto: ErrorLineageDto) -> Self {
        let mut chain = ErrorLineage::default();
        for d in dto.entries {
            chain.push(LineageEntry {
                task_id: d.task_id,
                kind: LineageKind::Failed {
                    error_class: d.error_class,
                },
                ts_ms: d.ts_ms,
            });
        }
        chain
    }
}

/// Serializable snapshot of the five replan/lineage/predicate counters tracked by [`DagScheduler`].
///
/// Written to the durable journal at each DAG pause so a subsequent `/plan resume` can restore
/// the counters rather than zeroing them, preventing the budget overrun that FR-DE-13 exists to
/// block.
///
/// [`DagScheduler`]: crate::scheduler::DagScheduler
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplanBudgetSnapshot {
    /// Per-task replan count.
    pub task_replan_counts: std::collections::HashMap<TaskId, u32>,
    /// Whole-plan replan counter.
    pub global_replan_count: u32,
    /// Number of predicate-driven re-runs consumed so far.
    pub predicate_replans_used: u32,
    /// Per-task accumulated predicate failure reasons.
    pub predicate_reasons: std::collections::HashMap<TaskId, String>,
    /// Per-task error lineage chains, serialised as DTOs.
    pub lineage_chains: std::collections::HashMap<TaskId, ErrorLineageDto>,
}

// ─── id derivation ────────────────────────────────────────────────────────────

/// Derive the [`ExecutionId`] for a specific `(graph_id, generation)` budget save.
///
/// Each generation gets a unique id so a plan that pauses, resumes and pauses again writes
/// to a fresh execution rather than replaying the stale snapshot from the first pause.
fn budget_exec_id(graph_id: &GraphId, generation: u32) -> ExecutionId {
    let mut payload = graph_id.to_string().into_bytes();
    payload.push(b'\0');
    payload.extend_from_slice(&generation.to_le_bytes());
    ExecutionId::derive(BUDGET_DOMAIN, &payload)
}

// ─── context builder ──────────────────────────────────────────────────────────

/// Open a [`DurableContext`] for the given budget execution id.
///
/// `is_resume` is passed through from [`DurableBackendEnum::Local`]'s `open_execution` result so
/// the context knows whether to replay or start fresh.
fn build_budget_ctx(
    exec_id: ExecutionId,
    is_resume: bool,
    backend: Arc<DurableBackendEnum>,
    writer: JournalWriterHandle,
    config: &DurableConfig,
) -> DurableContext {
    DurableContext::new(
        exec_id,
        ExecutionKind::DagRun,
        is_resume,
        backend,
        writer,
        config,
    )
}

// ─── public API ───────────────────────────────────────────────────────────────

/// Journal the current replan budget for a DAG execution.
///
/// Opens a fresh execution keyed to `(graph_id, generation)` and writes the snapshot as a
/// single `EffectClass::Idempotent` step.  The generation counter in the persisted
/// [`crate::TaskGraph`] must be incremented by the caller *before* this call so successive pauses
/// produce different execution ids.
///
/// The `backend` must already have the cipher injected (via `LocalBackend::with_cipher`) before
/// being wrapped into a `DurableBackendEnum`.  When encryption is disabled, pass a backend
/// without a cipher; the journal will store plaintext payloads (development mode only).
///
/// # Errors
///
/// Returns [`DurableError`] if the backend cannot be opened or the step write fails.
#[instrument(
    level = "info",
    name = "orch.durable.budget_journal",
    skip(backend, writer, config, snapshot),
    fields(graph_id = %graph_id, generation)
)]
pub async fn journal_budget(
    graph_id: &GraphId,
    generation: u32,
    backend: Arc<DurableBackendEnum>,
    writer: JournalWriterHandle,
    config: &DurableConfig,
    snapshot: ReplanBudgetSnapshot,
) -> Result<(), DurableError> {
    tracing::Span::current().record("generation", generation);

    let exec_id = budget_exec_id(graph_id, generation);
    let DurableBackendEnum::Local(local_backend) = &*backend else {
        return Err(DurableError::Storage {
            op: "open",
            source: "journal_budget: only Local backend is supported"
                .to_string()
                .into(),
        });
    };
    let local_backend = local_backend.clone();
    // Open a fresh execution (never a resume) for this generation.
    local_backend
        .open_execution(exec_id, ExecutionKind::DagRun)
        .await?;

    let ctx = build_budget_ctx(exec_id, false, backend, writer, config);

    let desc = StepDescriptor::idempotent(BUDGET_STEP_NAME, BUDGET_STEP_FP.to_vec());
    ctx.step(desc, move |_handle| async move {
        Ok::<ReplanBudgetSnapshot, zeph_durable::step::StepError>(snapshot)
    })
    .await?;

    Ok(())
}

/// Restore the replan budget for a graph that is being resumed.
///
/// Scans from `generation` downward (typically just one call at the stored value) until it finds
/// an execution with a committed `replan_budget` step result.  Returns `Ok(None)` when no
/// snapshot exists (first run, or durable was off at last pause), so the caller can fall back to
/// zeroing counters.
///
/// The `backend` must already have the cipher injected when encryption is enabled (see
/// `LocalBackend::with_cipher`).
///
/// # Errors
///
/// Returns [`DurableError`] only for storage failures, not for a missing snapshot.
#[instrument(
    level = "info",
    name = "orch.durable.budget_restore",
    skip(backend),
    fields(graph_id = %graph_id, generation)
)]
pub async fn restore_budget(
    graph_id: &GraphId,
    generation: u32,
    backend: Arc<DurableBackendEnum>,
) -> Result<Option<ReplanBudgetSnapshot>, DurableError> {
    tracing::Span::current().record("generation", generation);

    // Walk from the stored generation downward until we find a committed snapshot.
    // In practice this is always a single lookup (generation == last saved gen).
    for gn in (0..=generation).rev() {
        let exec_id = budget_exec_id(graph_id, gn);
        let entries = backend.read_execution(exec_id).await?;
        if let Some(snapshot) = find_budget_snapshot(entries) {
            return Ok(Some(snapshot));
        }
    }
    Ok(None)
}

/// Extract a [`ReplanBudgetSnapshot`] from a raw entry list, or return `None`.
///
/// Returns `None` both when the execution has no entries at all (interrupted write) and when no
/// `StepResult` with the budget step name is present — matching the MINOR-2 requirement.
fn find_budget_snapshot(entries: Vec<zeph_durable::JournalEntry>) -> Option<ReplanBudgetSnapshot> {
    use zeph_durable::journal::EntryKind;

    // v1 invariant: each budget-execution contains exactly one step, so the first successful
    // deserialization into ReplanBudgetSnapshot is the budget entry. If future versions add
    // additional steps (e.g., schema-version or metadata steps), this heuristic must be replaced
    // with an explicit step_name filter once the journal stores a step identifier field.
    for entry in entries {
        if let EntryKind::StepResult { payload, .. } = &entry.entry
            && let Ok(snapshot) = serde_json::from_slice::<ReplanBudgetSnapshot>(payload.as_ref())
        {
            return Some(snapshot);
        }
    }
    None
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphId;

    #[test]
    fn snapshot_serde_round_trip() {
        let mut snap = ReplanBudgetSnapshot {
            global_replan_count: 3,
            predicate_replans_used: 1,
            ..Default::default()
        };
        snap.task_replan_counts.insert(TaskId(0), 2);
        snap.predicate_reasons
            .insert(TaskId(1), "too slow".to_string());
        snap.lineage_chains.insert(
            TaskId(2),
            ErrorLineageDto {
                entries: vec![LineageEntryDto {
                    task_id: TaskId(2),
                    error_class: "timeout".to_string(),
                    ts_ms: 1_000_000,
                }],
            },
        );

        let json = serde_json::to_string(&snap).unwrap();
        let decoded: ReplanBudgetSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.global_replan_count, 3);
        assert_eq!(decoded.predicate_replans_used, 1);
        assert_eq!(*decoded.task_replan_counts.get(&TaskId(0)).unwrap(), 2);
        assert_eq!(
            decoded.predicate_reasons.get(&TaskId(1)).unwrap(),
            "too slow"
        );
        let chain = decoded.lineage_chains.get(&TaskId(2)).unwrap();
        assert_eq!(chain.entries.len(), 1);
        assert_eq!(chain.entries[0].error_class, "timeout");
    }

    #[test]
    fn error_lineage_dto_round_trip() {
        let mut lineage = ErrorLineage::default();
        lineage.push(LineageEntry {
            task_id: TaskId(5),
            kind: LineageKind::Failed {
                error_class: "llm_error".to_string(),
            },
            ts_ms: 42_000,
        });
        let dto = ErrorLineageDto::from(&lineage);
        assert_eq!(dto.entries.len(), 1);
        assert_eq!(dto.entries[0].task_id, TaskId(5));
        assert_eq!(dto.entries[0].error_class, "llm_error");

        let restored = ErrorLineage::from_dto(dto);
        assert_eq!(restored.entries().len(), 1);
        assert_eq!(restored.entries()[0].task_id, TaskId(5));
    }

    #[tokio::test]
    async fn generation_counter_produces_distinct_exec_ids() {
        let graph_id = GraphId::new();
        let id0 = budget_exec_id(&graph_id, 0);
        let id1 = budget_exec_id(&graph_id, 1);
        assert_ne!(
            id0, id1,
            "different generations must have different execution ids"
        );
    }

    // These tests open a real `LocalBackend` pool via `:memory:`, which is SQLite-specific
    // (mirroring `zeph-durable`'s `writer.rs`/`local.rs` test modules): under `--features postgres`
    // `DbConfig::connect()` takes cfg-priority and routes `:memory:` into `connect_postgres`, which
    // fails to parse it as a Postgres URL. See #5608.
    #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
    mod with_backend {
        use zeph_durable::{DurableBackendEnum, DurableConfig, JournalWriter, LocalBackend};

        use super::*;

        fn test_config() -> DurableConfig {
            DurableConfig {
                enabled: true,
                encrypt_payload: false,
                journal_flush_interval_ms: 1,
                journal_ack_timeout_ms: 1000,
                ..DurableConfig::default()
            }
        }

        async fn make_backend() -> (Arc<DurableBackendEnum>, JournalWriterHandle) {
            let local = LocalBackend::open(":memory:", 1_048_576).await.unwrap();
            local.init().await.unwrap();
            let local = Arc::new(local);
            let backend = Arc::new(DurableBackendEnum::Local(local.clone()));
            let (writer, handle) = JournalWriter::new(local, &test_config());
            tokio::spawn(async move { writer.run().await }); // EXEMPT: test-only helper
            (backend, handle)
        }

        #[tokio::test]
        async fn journal_then_restore_returns_snapshot() {
            let (backend, writer) = make_backend().await;
            let config = test_config();
            let graph_id = GraphId::new();
            let generation: u32 = 0;

            let snap = ReplanBudgetSnapshot {
                global_replan_count: 7,
                task_replan_counts: [(TaskId(0), 3)].into(),
                ..Default::default()
            };

            journal_budget(
                &graph_id,
                generation,
                backend.clone(),
                writer.clone(),
                &config,
                snap.clone(),
            )
            .await
            .unwrap();
            // flush ensures the buffered step result is persisted before we read it back.
            writer.flush().await.unwrap();

            let restored = restore_budget(&graph_id, generation, backend)
                .await
                .unwrap();
            let restored = restored.expect("snapshot should be present");
            assert_eq!(restored.global_replan_count, 7);
            assert_eq!(*restored.task_replan_counts.get(&TaskId(0)).unwrap(), 3);
        }

        #[tokio::test]
        async fn restore_returns_none_when_no_snapshot() {
            let (backend, _writer) = make_backend().await;
            let graph_id = GraphId::new();

            let result = restore_budget(&graph_id, 0, backend).await.unwrap();
            assert!(result.is_none());
        }

        #[tokio::test]
        async fn restore_picks_latest_generation() {
            let (backend, writer) = make_backend().await;
            let config = test_config();
            let graph_id = GraphId::new();

            // First pause: global_replan_count = 1
            let snap0 = ReplanBudgetSnapshot {
                global_replan_count: 1,
                ..Default::default()
            };
            journal_budget(
                &graph_id,
                0,
                backend.clone(),
                writer.clone(),
                &config,
                snap0,
            )
            .await
            .unwrap();

            // Second pause (after resume): global_replan_count = 2
            let snap1 = ReplanBudgetSnapshot {
                global_replan_count: 2,
                ..Default::default()
            };
            journal_budget(
                &graph_id,
                1,
                backend.clone(),
                writer.clone(),
                &config,
                snap1,
            )
            .await
            .unwrap();
            // flush ensures all buffered writes are persisted before reading back.
            writer.flush().await.unwrap();

            // restore_budget with generation=1 should return snap1
            let restored = restore_budget(&graph_id, 1, backend)
                .await
                .unwrap()
                .expect("snapshot must be present");
            assert_eq!(
                restored.global_replan_count, 2,
                "restore should pick the latest generation"
            );
        }
    }
}
