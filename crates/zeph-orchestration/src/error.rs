// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_subagent::SubAgentError;

#[derive(Debug, thiserror::Error)]
pub enum OrchestrationError {
    #[error("orchestration is disabled")]
    Disabled,

    #[error("planning failed: {0}")]
    PlanningFailed(String),

    #[error("invalid graph: {0}")]
    InvalidGraph(String),

    #[error("cycle detected in task graph")]
    CycleDetected,

    #[error("task not found: {0}")]
    TaskNotFound(String),

    #[error("no agent available for task: {0}")]
    NoAgentAvailable(String),

    #[error("graph not found: {0}")]
    GraphNotFound(String),

    #[error("scheduler error: {0}")]
    Scheduler(String),

    #[error("aggregation failed: {0}")]
    AggregationFailed(String),

    #[error("persistence error: {0}")]
    Persistence(String),

    #[error("task timed out: {0}")]
    TaskTimeout(String),

    #[error("canceled")]
    Canceled,

    #[error("invalid command: {0}")]
    InvalidCommand(String),

    /// Hard invariant violation during verification (e.g. cycle detected after `inject_tasks`).
    /// Never used for LLM call failures — those are fail-open.
    #[error("verification failed: {0}")]
    VerificationFailed(String),

    #[error(transparent)]
    SubAgent(#[from] SubAgentError),
}
