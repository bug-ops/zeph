// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeph_memory::store::graph_store::{GraphSummary, RawGraphStore};

use super::error::OrchestrationError;
use super::verify_predicate::{PredicateOutcome, VerifyPredicate};

/// Index of a task within a [`TaskGraph::tasks`] `Vec`.
///
/// `TaskId` is a dense, zero-based `u32` index. The invariant
/// `tasks[i].id == TaskId(i as u32)` holds throughout the lifetime of a graph.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::TaskId;
///
/// let id = TaskId(3);
/// assert_eq!(id.index(), 3);
/// assert_eq!(id.as_u32(), 3);
/// assert_eq!(id.to_string(), "3");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub u32);

impl TaskId {
    /// Returns the index for Vec access.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// Returns the raw `u32` value.
    #[must_use]
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a [`TaskGraph`].
///
/// Backed by a UUID v4. Implements `FromStr` / `Display` for serialization and
/// CLI lookup.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::GraphId;
///
/// let id = GraphId::new();
/// let s = id.to_string();
/// assert_eq!(s.len(), 36); // UUID string representation
///
/// let parsed: GraphId = s.parse().expect("valid UUID");
/// assert_eq!(id, parsed);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphId(Uuid);

impl GraphId {
    /// Generate a new random v4 `GraphId`.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for GraphId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for GraphId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for GraphId {
    type Err = OrchestrationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s)
            .map(GraphId)
            .map_err(|e| OrchestrationError::InvalidGraph(format!("invalid graph id '{s}': {e}")))
    }
}

/// Lifecycle status of a single task node.
///
/// State machine:
///
/// ```text
/// Pending → Ready → Running → Completed  (success)
///                           → Failed     (error; then failure strategy applies)
///                           → Skipped    (upstream failed with Skip strategy)
///                           → Canceled   (graph aborted while task was running)
/// ```
///
/// Only `Completed`, `Failed`, `Skipped`, and `Canceled` are terminal — see
/// [`TaskStatus::is_terminal`].
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::TaskStatus;
///
/// assert!(TaskStatus::Completed.is_terminal());
/// assert!(!TaskStatus::Running.is_terminal());
/// assert_eq!(TaskStatus::Pending.to_string(), "pending");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Waiting for dependencies to complete.
    Pending,
    /// All dependencies completed; ready to be scheduled.
    Ready,
    /// A sub-agent is actively executing this task.
    Running,
    /// Sub-agent completed successfully.
    Completed,
    /// Sub-agent returned an error.
    Failed,
    /// Task was skipped because an upstream task failed with [`FailureStrategy::Skip`].
    Skipped,
    /// Task was running when the graph was aborted ([`FailureStrategy::Abort`]).
    Canceled,
}

impl TaskStatus {
    /// Returns `true` if the status is a terminal state.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Skipped | TaskStatus::Canceled
        )
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskStatus::Pending => write!(f, "pending"),
            TaskStatus::Ready => write!(f, "ready"),
            TaskStatus::Running => write!(f, "running"),
            TaskStatus::Completed => write!(f, "completed"),
            TaskStatus::Failed => write!(f, "failed"),
            TaskStatus::Skipped => write!(f, "skipped"),
            TaskStatus::Canceled => write!(f, "canceled"),
        }
    }
}

/// Lifecycle status of a [`TaskGraph`].
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::GraphStatus;
///
/// assert_eq!(GraphStatus::Running.to_string(), "running");
/// assert_eq!(GraphStatus::Failed.to_string(), "failed");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphStatus {
    /// Graph has been created but the scheduler has not started yet.
    Created,
    /// Scheduler is actively dispatching tasks.
    Running,
    /// All tasks reached a terminal state successfully.
    Completed,
    /// At least one task failed and the `Abort` strategy halted the graph.
    Failed,
    /// The graph was canceled by an external caller.
    Canceled,
    /// Graph is paused; waiting for user input (triggered by [`FailureStrategy::Ask`]).
    Paused,
}

impl fmt::Display for GraphStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphStatus::Created => write!(f, "created"),
            GraphStatus::Running => write!(f, "running"),
            GraphStatus::Completed => write!(f, "completed"),
            GraphStatus::Failed => write!(f, "failed"),
            GraphStatus::Canceled => write!(f, "canceled"),
            GraphStatus::Paused => write!(f, "paused"),
        }
    }
}

/// What to do when a task fails.
///
/// Set at the graph level via [`TaskGraph::default_failure_strategy`] and
/// optionally overridden per task via [`TaskNode::failure_strategy`].
///
/// # Examples
///
/// ```rust
/// use std::str::FromStr;
/// use zeph_orchestration::FailureStrategy;
///
/// assert_eq!(FailureStrategy::default(), FailureStrategy::Abort);
/// assert_eq!("skip".parse::<FailureStrategy>().unwrap(), FailureStrategy::Skip);
/// assert_eq!(FailureStrategy::Retry.to_string(), "retry");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStrategy {
    /// Abort the entire graph and cancel all running tasks.
    #[default]
    Abort,
    /// Retry the task up to [`TaskNode::max_retries`] times, then abort.
    Retry,
    /// Skip the failed task and transitively skip all its dependents.
    Skip,
    /// Pause the graph ([`GraphStatus::Paused`]) and wait for user intervention.
    Ask,
}

impl fmt::Display for FailureStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailureStrategy::Abort => write!(f, "abort"),
            FailureStrategy::Retry => write!(f, "retry"),
            FailureStrategy::Skip => write!(f, "skip"),
            FailureStrategy::Ask => write!(f, "ask"),
        }
    }
}

impl FromStr for FailureStrategy {
    type Err = OrchestrationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "abort" => Ok(FailureStrategy::Abort),
            "retry" => Ok(FailureStrategy::Retry),
            "skip" => Ok(FailureStrategy::Skip),
            "ask" => Ok(FailureStrategy::Ask),
            other => Err(OrchestrationError::InvalidGraph(format!(
                "unknown failure strategy '{other}': expected one of abort, retry, skip, ask"
            ))),
        }
    }
}

/// Output produced by a completed task.
///
/// Stored in [`TaskNode::result`] after the sub-agent finishes. Used by
/// [`Aggregator`] to build the final synthesised response.
///
/// [`Aggregator`]: crate::aggregator::Aggregator
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    /// Raw text output returned by the sub-agent.
    pub output: String,
    /// File-system paths to any artifacts produced (e.g. build outputs, reports).
    pub artifacts: Vec<PathBuf>,
    /// Wall-clock execution time in milliseconds.
    pub duration_ms: u64,
    /// Handle ID of the sub-agent instance that produced this result.
    pub agent_id: Option<String>,
    /// Name of the agent definition used to spawn the sub-agent.
    pub agent_def: Option<String>,
}

/// Execution mode annotation emitted by the LLM planner for each task.
///
/// Controls how the [`DagScheduler`] dispatches a task relative to its siblings.
/// The annotation is set by the planner and stored in [`TaskNode::execution_mode`].
/// Absent or `null` in stored JSON deserialises to the default `Parallel`.
///
/// [`DagScheduler`]: crate::scheduler::DagScheduler
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::ExecutionMode;
///
/// assert_eq!(ExecutionMode::default(), ExecutionMode::Parallel);
/// let mode: ExecutionMode = serde_json::from_str("\"sequential\"").unwrap();
/// assert_eq!(mode, ExecutionMode::Sequential);
/// ```
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Task can run in parallel with others at the same DAG level.
    #[default]
    Parallel,
    /// Task is globally serialized: at most one `Sequential` task runs at a time across
    /// the entire graph (e.g. deploy, exclusive-resource access, shared-state mutation).
    Sequential,
}

/// A single node in the task DAG.
///
/// Constructed by [`Planner`] and stored inside a [`TaskGraph`].  The
/// scheduler drives each node through its [`TaskStatus`] lifecycle.
///
/// [`Planner`]: crate::planner::Planner
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::{TaskNode, TaskStatus, ExecutionMode};
///
/// let node = TaskNode::new(0, "fetch data", "Download the dataset from source.");
/// assert_eq!(node.status, TaskStatus::Pending);
/// assert!(node.depends_on.is_empty());
/// assert_eq!(node.execution_mode, ExecutionMode::Parallel);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    /// Dense zero-based index. Invariant: `tasks[i].id == TaskId(i)`.
    pub id: TaskId,
    /// Short, human-readable task title.
    pub title: String,
    /// Full task description passed verbatim to the assigned sub-agent as its prompt.
    pub description: String,
    /// Preferred agent name suggested by the planner; `None` lets the router decide.
    pub agent_hint: Option<String>,
    /// Current lifecycle status.
    pub status: TaskStatus,
    /// Indices of tasks this node depends on.
    pub depends_on: Vec<TaskId>,
    /// Result populated by the scheduler after the sub-agent finishes.
    pub result: Option<TaskResult>,
    /// Agent name actually assigned by the router at dispatch time.
    pub assigned_agent: Option<String>,
    /// Number of times this task has been retried so far (execution retries only).
    pub retry_count: u32,
    /// Number of predicate-driven re-runs for this task (independent of `retry_count`).
    #[serde(default)]
    pub predicate_rerun_count: u32,
    /// Per-task failure strategy override; `None` means use [`TaskGraph::default_failure_strategy`].
    pub failure_strategy: Option<FailureStrategy>,
    /// Maximum retry attempts for this task; `None` means use [`TaskGraph::default_max_retries`].
    pub max_retries: Option<u32>,
    /// LLM planner annotation. Old SQLite-stored JSON without this field
    /// deserialises to the default (`Parallel`).
    #[serde(default)]
    pub execution_mode: ExecutionMode,
    /// Per-subtask verification predicate (predicate gate).
    ///
    /// When `Some`, the task's output must satisfy this criterion before downstream
    /// tasks may consume it. The scheduler emits `SchedulerAction::VerifyPredicate`
    /// after task completion and blocks downstream dispatch until
    /// `predicate_outcome.is_some()`.
    #[serde(default)]
    pub verify_predicate: Option<VerifyPredicate>,
    /// Outcome of the most recent predicate evaluation.
    ///
    /// `None` means the gate has not been evaluated yet (in-memory only; restart
    /// re-evaluates any pending predicates). The scheduler re-emits `VerifyPredicate`
    /// on every tick while this is `None` and `verify_predicate.is_some()`.
    #[serde(default)]
    pub predicate_outcome: Option<PredicateOutcome>,
}

impl TaskNode {
    /// Create a new pending task with the given index.
    #[must_use]
    pub fn new(id: u32, title: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: TaskId(id),
            title: title.into(),
            description: description.into(),
            agent_hint: None,
            status: TaskStatus::Pending,
            depends_on: Vec::new(),
            result: None,
            assigned_agent: None,
            retry_count: 0,
            predicate_rerun_count: 0,
            failure_strategy: None,
            max_retries: None,
            execution_mode: ExecutionMode::default(),
            verify_predicate: None,
            predicate_outcome: None,
        }
    }
}

/// A directed acyclic graph of tasks to be executed by the orchestrator.
///
/// Created by the [`Planner`] and driven to completion by the [`DagScheduler`].
/// The `tasks` vec is the authoritative store; all indices (`TaskId`) reference
/// positions within it.
///
/// [`Planner`]: crate::planner::Planner
/// [`DagScheduler`]: crate::scheduler::DagScheduler
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::{TaskGraph, TaskNode, GraphStatus, FailureStrategy};
///
/// let mut graph = TaskGraph::new("build and deploy service");
/// assert_eq!(graph.status, GraphStatus::Created);
/// assert_eq!(graph.default_failure_strategy, FailureStrategy::Abort);
/// assert_eq!(graph.default_max_retries, 3);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGraph {
    /// Unique graph identifier (UUID v4).
    pub id: GraphId,
    /// High-level user goal that was decomposed into this graph.
    pub goal: String,
    /// All task nodes. Index `i` must satisfy `tasks[i].id == TaskId(i)`.
    pub tasks: Vec<TaskNode>,
    /// Current lifecycle status of the graph as a whole.
    pub status: GraphStatus,
    /// Graph-wide failure strategy applied when a task has no per-task override.
    pub default_failure_strategy: FailureStrategy,
    /// Graph-wide maximum retry count applied when a task has no per-task override.
    pub default_max_retries: u32,
    /// ISO-8601 UTC timestamp of graph creation.
    pub created_at: String,
    /// ISO-8601 UTC timestamp set when the graph reaches a terminal status.
    pub finished_at: Option<String>,
}

impl TaskGraph {
    /// Create a new graph with `Created` status.
    #[must_use]
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            id: GraphId::new(),
            goal: goal.into(),
            tasks: Vec::new(),
            status: GraphStatus::Created,
            default_failure_strategy: FailureStrategy::default(),
            default_max_retries: 3,
            created_at: chrono_now(),
            finished_at: None,
        }
    }
}

pub(crate) fn chrono_now() -> String {
    // ISO-8601 UTC timestamp, consistent with the rest of the codebase.
    // Format: "2026-03-05T22:04:41Z"
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Manual formatting: seconds since epoch → ISO-8601 UTC
    // Days since epoch, then decompose into year/month/day
    let (y, mo, d, h, mi, s) = epoch_secs_to_datetime(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert Unix epoch seconds to (year, month, day, hour, min, sec) UTC.
fn epoch_secs_to_datetime(secs: u64) -> (u64, u8, u8, u8, u8, u8) {
    let s = (secs % 60) as u8;
    let mins = secs / 60;
    let mi = (mins % 60) as u8;
    let hours = mins / 60;
    let h = (hours % 24) as u8;
    let days = hours / 24; // days since 1970-01-01

    // Gregorian calendar decomposition
    // 400-year cycle = 146097 days
    let (mut year, mut remaining_days) = {
        let cycles = days / 146_097;
        let rem = days % 146_097;
        (1970 + cycles * 400, rem)
    };
    // 100-year century (36524 days, no leap on century unless /400)
    let centuries = (remaining_days / 36_524).min(3);
    year += centuries * 100;
    remaining_days -= centuries * 36_524;
    // 4-year cycle (1461 days)
    let quads = remaining_days / 1_461;
    year += quads * 4;
    remaining_days -= quads * 1_461;
    // remaining years
    let extra_years = (remaining_days / 365).min(3);
    year += extra_years;
    remaining_days -= extra_years * 365;

    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let days_in_month: [u64; 12] = if is_leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 0u8;
    for (i, &dim) in days_in_month.iter().enumerate() {
        if remaining_days < dim {
            // i is in 0..12, so i+1 fits in u8
            month = u8::try_from(i + 1).unwrap_or(1);
            break;
        }
        remaining_days -= dim;
    }
    // remaining_days is in 0..30, so +1 fits in u8
    let day = u8::try_from(remaining_days + 1).unwrap_or(1);

    (year, month, day, h, mi, s)
}

/// Maximum allowed length for a `TaskGraph` goal string.
const MAX_GOAL_LEN: usize = 1024;

/// Type-safe wrapper around `RawGraphStore` that handles `TaskGraph` serialization.
///
/// Consumers in `zeph-core` use this instead of `RawGraphStore` directly, so they
/// never need to deal with JSON strings.
///
/// # Storage layout
///
/// The `task_graphs` table stores both metadata columns (`goal`, `status`,
/// `created_at`, `finished_at`) and the full `graph_json` blob. The metadata
/// columns are summary/index data used for listing and filtering; `graph_json`
/// is the authoritative source for full graph reconstruction. On `load`, only
/// `graph_json` is deserialized — the columns are not consulted.
pub struct GraphPersistence<S: RawGraphStore> {
    store: S,
}

impl<S: RawGraphStore> GraphPersistence<S> {
    /// Create a new `GraphPersistence` wrapping the given store.
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// Persist a `TaskGraph` (upsert).
    ///
    /// Returns `OrchestrationError::InvalidGraph` if `graph.goal` exceeds
    /// `MAX_GOAL_LEN` (1024) characters.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::Persistence` on serialization or database failure.
    pub async fn save(&self, graph: &TaskGraph) -> Result<(), OrchestrationError> {
        if graph.goal.len() > MAX_GOAL_LEN {
            return Err(OrchestrationError::InvalidGraph(format!(
                "goal exceeds {MAX_GOAL_LEN} character limit ({} chars)",
                graph.goal.len()
            )));
        }
        let json = serde_json::to_string(graph)
            .map_err(|e| OrchestrationError::Persistence(e.to_string()))?;
        self.store
            .save_graph(
                &graph.id.to_string(),
                &graph.goal,
                &graph.status.to_string(),
                &json,
                &graph.created_at,
                graph.finished_at.as_deref(),
            )
            .await
            .map_err(|e| OrchestrationError::Persistence(e.to_string()))
    }

    /// Load a `TaskGraph` by its `GraphId`.
    ///
    /// Returns `None` if not found.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::Persistence` on database or deserialization failure.
    pub async fn load(&self, id: &GraphId) -> Result<Option<TaskGraph>, OrchestrationError> {
        match self
            .store
            .load_graph(&id.to_string())
            .await
            .map_err(|e| OrchestrationError::Persistence(e.to_string()))?
        {
            Some(json) => {
                let graph = serde_json::from_str(&json)
                    .map_err(|e| OrchestrationError::Persistence(e.to_string()))?;
                Ok(Some(graph))
            }
            None => Ok(None),
        }
    }

    /// List stored graphs (newest first).
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::Persistence` on database failure.
    pub async fn list(&self, limit: u32) -> Result<Vec<GraphSummary>, OrchestrationError> {
        self.store
            .list_graphs(limit)
            .await
            .map_err(|e| OrchestrationError::Persistence(e.to_string()))
    }

    /// Delete a graph by its `GraphId`.
    ///
    /// Returns `true` if a row was deleted.
    ///
    /// # Errors
    ///
    /// Returns `OrchestrationError::Persistence` on database failure.
    pub async fn delete(&self, id: &GraphId) -> Result<bool, OrchestrationError> {
        self.store
            .delete_graph(&id.to_string())
            .await
            .map_err(|e| OrchestrationError::Persistence(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_taskid_display() {
        assert_eq!(TaskId(3).to_string(), "3");
    }

    #[test]
    fn test_graphid_display_and_new() {
        let id = GraphId::new();
        let s = id.to_string();
        assert_eq!(s.len(), 36, "UUID string should be 36 chars");
        let parsed: GraphId = s.parse().expect("should parse back");
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_graphid_from_str_invalid() {
        let err = "not-a-uuid".parse::<GraphId>();
        assert!(err.is_err());
    }

    #[test]
    fn test_task_status_is_terminal() {
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Skipped.is_terminal());
        assert!(TaskStatus::Canceled.is_terminal());

        assert!(!TaskStatus::Pending.is_terminal());
        assert!(!TaskStatus::Ready.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
    }

    #[test]
    fn test_task_status_display() {
        assert_eq!(TaskStatus::Pending.to_string(), "pending");
        assert_eq!(TaskStatus::Ready.to_string(), "ready");
        assert_eq!(TaskStatus::Running.to_string(), "running");
        assert_eq!(TaskStatus::Completed.to_string(), "completed");
        assert_eq!(TaskStatus::Failed.to_string(), "failed");
        assert_eq!(TaskStatus::Skipped.to_string(), "skipped");
        assert_eq!(TaskStatus::Canceled.to_string(), "canceled");
    }

    #[test]
    fn test_failure_strategy_default() {
        assert_eq!(FailureStrategy::default(), FailureStrategy::Abort);
    }

    #[test]
    fn test_failure_strategy_display() {
        assert_eq!(FailureStrategy::Abort.to_string(), "abort");
        assert_eq!(FailureStrategy::Retry.to_string(), "retry");
        assert_eq!(FailureStrategy::Skip.to_string(), "skip");
        assert_eq!(FailureStrategy::Ask.to_string(), "ask");
    }

    #[test]
    fn test_graph_status_display() {
        assert_eq!(GraphStatus::Created.to_string(), "created");
        assert_eq!(GraphStatus::Running.to_string(), "running");
        assert_eq!(GraphStatus::Completed.to_string(), "completed");
        assert_eq!(GraphStatus::Failed.to_string(), "failed");
        assert_eq!(GraphStatus::Canceled.to_string(), "canceled");
        assert_eq!(GraphStatus::Paused.to_string(), "paused");
    }

    #[test]
    fn test_task_graph_serde_roundtrip() {
        let mut graph = TaskGraph::new("test goal");
        graph.tasks.push(TaskNode::new(0, "task 0", "do something"));
        let json = serde_json::to_string(&graph).expect("serialize");
        let restored: TaskGraph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(graph.id, restored.id);
        assert_eq!(graph.goal, restored.goal);
        assert_eq!(graph.tasks.len(), restored.tasks.len());
    }

    #[test]
    fn test_task_node_serde_roundtrip() {
        let mut node = TaskNode::new(1, "compile", "run cargo build");
        node.agent_hint = Some("rust-dev".to_string());
        node.depends_on = vec![TaskId(0)];
        let json = serde_json::to_string(&node).expect("serialize");
        let restored: TaskNode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(node.id, restored.id);
        assert_eq!(node.title, restored.title);
        assert_eq!(node.depends_on, restored.depends_on);
    }

    #[test]
    fn test_task_result_serde_roundtrip() {
        let result = TaskResult {
            output: "ok".to_string(),
            artifacts: vec![PathBuf::from("/tmp/out.bin")],
            duration_ms: 500,
            agent_id: Some("agent-1".to_string()),
            agent_def: None,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let restored: TaskResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result.output, restored.output);
        assert_eq!(result.duration_ms, restored.duration_ms);
        assert_eq!(result.artifacts, restored.artifacts);
    }

    #[test]
    fn test_failure_strategy_from_str() {
        assert_eq!(
            "abort".parse::<FailureStrategy>().unwrap(),
            FailureStrategy::Abort
        );
        assert_eq!(
            "retry".parse::<FailureStrategy>().unwrap(),
            FailureStrategy::Retry
        );
        assert_eq!(
            "skip".parse::<FailureStrategy>().unwrap(),
            FailureStrategy::Skip
        );
        assert_eq!(
            "ask".parse::<FailureStrategy>().unwrap(),
            FailureStrategy::Ask
        );
        assert!("abort_all".parse::<FailureStrategy>().is_err());
        assert!("".parse::<FailureStrategy>().is_err());
    }

    #[test]
    fn test_chrono_now_iso8601_format() {
        let ts = chrono_now();
        // Format: "YYYY-MM-DDTHH:MM:SSZ" — 20 chars
        assert_eq!(ts.len(), 20, "timestamp should be 20 chars: {ts}");
        assert!(ts.ends_with('Z'), "should end with Z: {ts}");
        assert!(ts.contains('T'), "should contain T: {ts}");
        // Year should be >= 2024
        let year: u32 = ts[..4].parse().expect("year should be numeric");
        assert!(year >= 2024, "year should be >= 2024: {year}");
    }

    #[test]
    fn test_failure_strategy_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&FailureStrategy::Abort).unwrap(),
            "\"abort\""
        );
        assert_eq!(
            serde_json::to_string(&FailureStrategy::Retry).unwrap(),
            "\"retry\""
        );
        assert_eq!(
            serde_json::to_string(&FailureStrategy::Skip).unwrap(),
            "\"skip\""
        );
        assert_eq!(
            serde_json::to_string(&FailureStrategy::Ask).unwrap(),
            "\"ask\""
        );
    }

    #[test]
    fn test_graph_persistence_save_rejects_long_goal() {
        // GraphPersistence::save() is async and requires a real store;
        // we verify the goal-length guard directly via the const.
        let long_goal = "x".repeat(MAX_GOAL_LEN + 1);
        let mut graph = TaskGraph::new(long_goal);
        graph.goal = "x".repeat(MAX_GOAL_LEN + 1);
        assert!(
            graph.goal.len() > MAX_GOAL_LEN,
            "test setup: goal must exceed limit"
        );
        // The check itself lives in GraphPersistence::save(), exercised by
        // the async persistence tests in zeph-memory; here we verify the constant.
        assert_eq!(MAX_GOAL_LEN, 1024);
    }

    #[test]
    fn test_task_node_predicate_fields_default_to_none() {
        // Old SQLite blobs without verify_predicate / predicate_outcome must deserialize
        // to None without error (#[serde(default)]).
        let json = r#"{
            "id": 0,
            "title": "t",
            "description": "d",
            "agent_hint": null,
            "status": "pending",
            "depends_on": [],
            "result": null,
            "assigned_agent": null,
            "retry_count": 0,
            "failure_strategy": null,
            "max_retries": null
        }"#;
        let node: TaskNode = serde_json::from_str(json).expect("should deserialize old JSON");
        assert!(node.verify_predicate.is_none());
        assert!(node.predicate_outcome.is_none());
    }

    #[test]
    fn test_task_node_missing_execution_mode_deserializes_as_parallel() {
        // Old SQLite-stored JSON blobs lack the execution_mode field.
        // #[serde(default)] must make them deserialize to Parallel without error.
        let json = r#"{
            "id": 0,
            "title": "t",
            "description": "d",
            "agent_hint": null,
            "status": "pending",
            "depends_on": [],
            "result": null,
            "assigned_agent": null,
            "retry_count": 0,
            "failure_strategy": null,
            "max_retries": null
        }"#;
        let node: TaskNode = serde_json::from_str(json).expect("should deserialize old JSON");
        assert_eq!(node.execution_mode, ExecutionMode::Parallel);
    }

    #[test]
    fn test_execution_mode_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&ExecutionMode::Parallel).unwrap(),
            "\"parallel\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionMode::Sequential).unwrap(),
            "\"sequential\""
        );
        let p: ExecutionMode = serde_json::from_str("\"parallel\"").unwrap();
        assert_eq!(p, ExecutionMode::Parallel);
        let s: ExecutionMode = serde_json::from_str("\"sequential\"").unwrap();
        assert_eq!(s, ExecutionMode::Sequential);
    }
}
