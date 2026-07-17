// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_subagent::SubAgentError;

use super::lineage::LineageEntry;

/// All error variants produced by the orchestration subsystem.
///
/// Variants are exhaustive — callers that match on this type should use a
/// `_ => …` arm to stay robust against future additions.
///
/// # Fail-open policy
///
/// LLM-backed steps (verification, replan) are always fail-open: on failure
/// they log a warning and continue rather than returning an error. Only
/// structural invariant violations and hard configuration errors propagate as
/// `Err`.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::OrchestrationError;
///
/// fn describe(err: &OrchestrationError) -> &'static str {
///     match err {
///         OrchestrationError::CycleDetected => "graph has a cycle",
///         OrchestrationError::Disabled => "orchestration is off",
///         _ => "other orchestration error",
///     }
/// }
///
/// let err = OrchestrationError::CycleDetected;
/// assert_eq!(describe(&err), "graph has a cycle");
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OrchestrationError {
    /// Orchestration is disabled in configuration.
    #[error("orchestration is disabled")]
    Disabled,

    /// The LLM planner failed to produce a valid task graph.
    #[error("planning failed: {0}")]
    PlanningFailed(String),

    /// The task graph structure is invalid (e.g. wrong task-id invariant, bad reference).
    #[error("invalid graph: {0}")]
    InvalidGraph(String),

    /// A cycle was detected during topological sort of the task graph.
    #[error("cycle detected in task graph")]
    CycleDetected,

    /// A `TaskId` or task title lookup yielded no result.
    #[error("task not found: {0}")]
    TaskNotFound(String),

    /// No agent in the available pool can be routed to a task.
    #[error("no agent available for task: {0}")]
    NoAgentAvailable(String),

    /// A `GraphId` could not be found in persistence.
    #[error("graph not found: {0}")]
    GraphNotFound(String),

    /// An internal scheduler invariant was violated.
    #[error("scheduler error: {0}")]
    Scheduler(String),

    /// Result aggregation failed and the fallback path also failed.
    #[error("aggregation failed: {0}")]
    AggregationFailed(String),

    /// A database read/write or serialization error in graph persistence.
    #[error("persistence error: {0}")]
    Persistence(String),

    /// A task exceeded its per-task wall-clock timeout.
    #[error("task timed out: {0}")]
    TaskTimeout(String),

    /// The scheduler or a task was canceled by the caller.
    #[error("canceled")]
    Canceled,

    /// A `/plan` CLI command could not be parsed.
    #[error("invalid command: {0}")]
    InvalidCommand(String),

    /// Hard invariant violation during verification (e.g. cycle detected after `inject_tasks`).
    ///
    /// Never used for LLM call failures — those are fail-open and only log a warning.
    #[error("verification failed: {0}")]
    VerificationFailed(String),

    /// A required configuration value is missing or out of range.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// Propagated error from a sub-agent execution.
    #[error(transparent)]
    SubAgent(#[from] SubAgentError),

    /// A `VerifyPredicate::Expression` was encountered; only `Natural` is
    /// supported in v1.
    #[error("predicate type not supported: {0}")]
    PredicateNotSupported(String),

    /// Predicate remediation could not be injected because the replan budget is exhausted.
    #[error("replan budget exhausted for task {task_id}: {reason}")]
    ReplanBudgetExhausted {
        /// Task that triggered remediation.
        task_id: String,
        /// Human-readable reason (e.g. "predicate remediation").
        reason: String,
    },

    /// The DAG was aborted because a consecutive error chain in a `depends_on` path
    /// (or a region fan-out failure rate) exceeded the configured threshold.
    ///
    /// `chain_depth` in the display is `chain.len()` for quick log scanning; the full
    /// [`LineageEntry`] list is emitted to the structured audit log.
    #[error("cascade abort: root={root:?}, chain_depth={}", chain.len())]
    CascadeAborted {
        /// Root task ID where the failure chain began.
        root: super::graph::TaskId,
        /// Full lineage chain at the time of abort; earliest entry first.
        chain: Vec<LineageEntry>,
    },

    /// A `Command.goto` target failed validation (spec-080, GitHub #6363): out of range,
    /// an ambiguous or absent `ByTitle` match, already `Completed` (forward-only), holding
    /// a live `route_to` reservation from a non-terminal source, or having unsatisfied
    /// `depends_on`.
    #[error("invalid handoff target: {0}")]
    InvalidHandoffTarget(String),

    /// The per-graph `[orchestration.command].max_handoffs` budget (spec-080) was
    /// exhausted — the livelock backstop for forward fan-out `Command.goto` chains.
    #[error(
        "handoff budget exhausted: {handoff_count} handoffs performed, limit is {max_handoffs}"
    )]
    HandoffBudgetExhausted {
        /// Number of handoffs already performed on this graph.
        handoff_count: u32,
        /// Configured per-graph budget (`[orchestration.command].max_handoffs`).
        max_handoffs: u32,
    },
}
