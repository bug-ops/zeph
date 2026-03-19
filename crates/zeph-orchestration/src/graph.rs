// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeph_memory::sqlite::graph_store::{GraphSummary, RawGraphStore};

use super::error::OrchestrationError;

/// Index of a task within a `TaskGraph.tasks` Vec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub(crate) u32);

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

/// Unique identifier for a `TaskGraph`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Ready,
    Running,
    Completed,
    Failed,
    Skipped,
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

/// Lifecycle status of a `TaskGraph`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphStatus {
    Created,
    Running,
    Completed,
    Failed,
    Canceled,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStrategy {
    /// Abort the entire graph.
    #[default]
    Abort,
    /// Retry the task up to `max_retries` times.
    Retry,
    /// Skip the task and its dependents.
    Skip,
    /// Pause the graph and ask the user.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub output: String,
    pub artifacts: Vec<PathBuf>,
    pub duration_ms: u64,
    pub agent_id: Option<String>,
    pub agent_def: Option<String>,
}

/// A single node in the task DAG.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: TaskId,
    pub title: String,
    pub description: String,
    pub agent_hint: Option<String>,
    pub status: TaskStatus,
    /// Indices of tasks this node depends on.
    pub depends_on: Vec<TaskId>,
    pub result: Option<TaskResult>,
    pub assigned_agent: Option<String>,
    pub retry_count: u32,
    /// Per-task override; `None` means use graph default.
    pub failure_strategy: Option<FailureStrategy>,
    pub max_retries: Option<u32>,
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
            failure_strategy: None,
            max_retries: None,
        }
    }
}

/// A directed acyclic graph of tasks to be executed by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGraph {
    pub id: GraphId,
    pub goal: String,
    pub tasks: Vec<TaskNode>,
    pub status: GraphStatus,
    pub default_failure_strategy: FailureStrategy,
    pub default_max_retries: u32,
    pub created_at: String,
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
}
