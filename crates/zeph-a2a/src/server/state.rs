// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared server state: task storage, processor interface, and event types.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::types::{AgentCard, Artifact, Message, Task, TaskState, TaskStatus};

/// Shared state injected into every axum handler via `State<AppState>`.
///
/// `AppState` is `Clone` so axum can inject it per-request without locking.
/// All mutable state (tasks) is behind an `Arc<RwLock<_>>` inside [`TaskManager`].
#[derive(Clone)]
pub struct AppState {
    /// This server's capability card, served at `/.well-known/agent.json`.
    pub card: AgentCard,
    /// In-memory task store shared across all handlers.
    pub task_manager: TaskManager,
    /// The application-level logic that handles incoming task messages.
    pub processor: Arc<dyn TaskProcessor>,
    /// Maximum time to wait for a [`TaskProcessor`] to complete before marking the task
    /// as [`TaskState::Failed`] and aborting the spawned future.
    pub request_timeout: Duration,
}

/// An event emitted by a [`TaskProcessor`] to drive the handler's response.
///
/// The handler accumulates [`ArtifactChunk`](ProcessorEvent::ArtifactChunk) events into a
/// final artifact. [`StatusUpdate`](ProcessorEvent::StatusUpdate) events update the task's
/// state in [`TaskManager`] and, for streaming calls, are forwarded as SSE events.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ProcessorEvent {
    /// A task lifecycle state transition. Set `is_final = true` on the terminal state.
    StatusUpdate { state: TaskState, is_final: bool },
    /// A chunk of text output. Set `is_final = true` when the artifact is complete.
    ArtifactChunk { text: String, is_final: bool },
}

/// Contract for processing A2A task messages through the agent pipeline.
///
/// Implementors receive a task ID, the incoming [`Message`], and a channel for emitting
/// [`ProcessorEvent`]s. They must return a boxed future to remain object-safe (no native
/// async trait across dyn dispatch).
///
/// # Contract
///
/// - The processor SHOULD emit at least one [`ProcessorEvent::StatusUpdate`] with a
///   terminal [`TaskState`] (e.g., `Completed` or `Failed`) before returning.
/// - The processor SHOULD close `event_tx` by dropping it before returning so the handler
///   can detect completion.
/// - Errors returned by the future cause the task to transition to [`TaskState::Failed`].
///
/// # Examples
///
/// ```rust
/// use std::pin::Pin;
/// use zeph_a2a::server::{TaskProcessor, ProcessorEvent};
/// use zeph_a2a::{A2aError, Message, TaskState};
///
/// struct EchoProcessor;
///
/// impl TaskProcessor for EchoProcessor {
///     fn process(
///         &self,
///         _task_id: String,
///         message: Message,
///         event_tx: tokio::sync::mpsc::Sender<ProcessorEvent>,
///     ) -> Pin<Box<dyn std::future::Future<Output = Result<(), A2aError>> + Send>> {
///         Box::pin(async move {
///             let text = message.text_content().unwrap_or("").to_owned();
///             let _ = event_tx.send(ProcessorEvent::ArtifactChunk {
///                 text: format!("echo: {text}"),
///                 is_final: true,
///             }).await;
///             let _ = event_tx.send(ProcessorEvent::StatusUpdate {
///                 state: TaskState::Completed,
///                 is_final: true,
///             }).await;
///             Ok(())
///         })
///     }
/// }
/// ```
#[cfg_attr(docsrs, doc(cfg(feature = "server")))]
pub trait TaskProcessor: Send + Sync {
    /// Process the incoming `message` for `task_id`, emitting events via `event_tx`.
    fn process(
        &self,
        task_id: String,
        message: Message,
        event_tx: tokio::sync::mpsc::Sender<ProcessorEvent>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), crate::error::A2aError>> + Send>,
    >;
}

/// Async, thread-safe in-memory store for A2A tasks.
///
/// All mutating methods take `&self` because the internal `HashMap` is behind an
/// `Arc<RwLock<_>>`. `TaskManager` is `Clone`: cloned instances share the same task store.
///
/// History trimming is done lazily on reads via `history_length` — the full history is
/// always stored, but responses return at most `N` most-recent messages when requested.
///
/// Expired terminal tasks are removed by [`evict_expired`](Self::evict_expired), which is
/// typically called from a background loop spawned by [`A2aServer`](crate::server::A2aServer).
#[derive(Clone)]
pub struct TaskManager {
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    /// Insertion timestamp for each task, used to compute age for TTL eviction.
    created_at: Arc<RwLock<HashMap<String, Instant>>>,
}

impl TaskManager {
    /// Create a new, empty `TaskManager`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            created_at: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new task from the initial user message and store it.
    ///
    /// The task starts in [`TaskState::Submitted`] with a UUID assigned as its ID.
    /// The `message` is prepended to the task's `history`.
    pub async fn create_task(&self, message: Message) -> Task {
        let id = uuid::Uuid::new_v4().to_string();
        let task = Task {
            id: id.clone(),
            context_id: message.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Submitted,
                timestamp: now_rfc3339(),
                message: None,
            },
            artifacts: vec![],
            history: vec![message],
            metadata: None,
        };
        self.created_at
            .write()
            .await
            .insert(id.clone(), Instant::now());
        self.tasks.write().await.insert(id, task.clone());
        task
    }

    /// Retrieve a task by ID, optionally truncating its history.
    ///
    /// If `history_length` is `Some(n)`, at most the `n` most recent messages are returned.
    /// The full history remains stored — truncation only affects the returned clone.
    /// Returns `None` if the task does not exist.
    pub async fn get_task(&self, id: &str, history_length: Option<u32>) -> Option<Task> {
        let tasks = self.tasks.read().await;
        tasks.get(id).map(|t| {
            if let Some(limit) = history_length {
                let mut task = t.clone();
                let len = task.history.len();
                let limit = limit as usize;
                if len > limit {
                    task.history = task.history[len - limit..].to_vec();
                }
                task
            } else {
                t.clone()
            }
        })
    }

    /// Transition the task's status to `state`, optionally attaching a status message.
    ///
    /// Returns the updated task, or `None` if the task does not exist.
    /// The `timestamp` field is set to the current UTC time.
    pub async fn update_status(
        &self,
        id: &str,
        state: TaskState,
        message: Option<Message>,
    ) -> Option<Task> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(id)?;
        task.status = TaskStatus {
            state,
            timestamp: now_rfc3339(),
            message,
        };
        Some(task.clone())
    }

    /// Append an artifact to the task's artifact list.
    ///
    /// Returns the updated task, or `None` if the task does not exist.
    pub async fn add_artifact(&self, id: &str, artifact: Artifact) -> Option<Task> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(id)?;
        task.artifacts.push(artifact);
        Some(task.clone())
    }

    /// Append a message to the task's conversation history.
    ///
    /// Returns `Some(())` on success, or `None` if the task does not exist.
    pub async fn append_history(&self, id: &str, message: Message) -> Option<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(id)?;
        task.history.push(message);
        Some(())
    }

    /// # Errors
    ///
    /// Returns `CancelError::NotFound` if the task doesn't exist, or
    /// `CancelError::NotCancelable` if the task is in a terminal state.
    pub async fn cancel_task(&self, id: &str) -> Result<Task, CancelError> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(id).ok_or(CancelError::NotFound)?;

        match task.status.state {
            TaskState::Completed
            | TaskState::Failed
            | TaskState::Canceled
            | TaskState::Rejected => {
                return Err(CancelError::NotCancelable(task.status.state));
            }
            _ => {}
        }

        task.status = TaskStatus {
            state: TaskState::Canceled,
            timestamp: now_rfc3339(),
            message: None,
        };
        Ok(task.clone())
    }

    /// Remove terminal tasks that have exceeded `ttl`.
    ///
    /// Only tasks in a terminal state ([`TaskState::Completed`], [`TaskState::Failed`],
    /// [`TaskState::Canceled`], or [`TaskState::Rejected`]) are eligible for removal.
    /// Non-terminal tasks are never evicted regardless of age, because they may still be
    /// actively polled or streamed by a client.
    ///
    /// This method is designed to be called periodically from a background task. The caller
    /// is responsible for choosing a suitable interval (e.g., every 60 seconds).
    pub async fn evict_expired(&self, ttl: Duration) {
        let now = Instant::now();

        let mut ages = self.created_at.write().await;
        let mut tasks = self.tasks.write().await;

        let expired: Vec<String> = ages
            .iter()
            .filter_map(|(id, created)| {
                if now.duration_since(*created) < ttl {
                    return None;
                }
                let is_terminal = tasks.get(id).is_some_and(|t| {
                    matches!(
                        t.status.state,
                        TaskState::Completed
                            | TaskState::Failed
                            | TaskState::Canceled
                            | TaskState::Rejected
                    )
                });
                is_terminal.then(|| id.clone())
            })
            .collect();

        let count = expired.len();
        for id in expired {
            tasks.remove(&id);
            ages.remove(&id);
        }

        if count > 0 {
            tracing::debug!(evicted = count, "a2a task manager: evicted expired tasks");
        }
    }
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Error returned by [`TaskManager::cancel_task`] when cancellation cannot proceed.
#[derive(Debug)]
#[non_exhaustive]
pub enum CancelError {
    /// No task with the given ID exists in the store.
    NotFound,
    /// The task is already in a terminal state ([`TaskState::Completed`],
    /// [`TaskState::Failed`], [`TaskState::Canceled`], or [`TaskState::Rejected`]).
    NotCancelable(TaskState),
}

pub(super) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_message(text: &str) -> Message {
        Message::user_text(text)
    }

    #[tokio::test]
    async fn create_and_get_task() {
        let tm = TaskManager::new();
        let task = tm.create_task(test_message("hello")).await;
        assert_eq!(task.status.state, TaskState::Submitted);
        assert_eq!(task.history.len(), 1);

        let fetched = tm.get_task(&task.id, None).await.unwrap();
        assert_eq!(fetched.id, task.id);
    }

    #[tokio::test]
    async fn get_task_with_history_limit() {
        let tm = TaskManager::new();
        let task = tm.create_task(test_message("msg1")).await;
        tm.append_history(&task.id, test_message("msg2")).await;
        tm.append_history(&task.id, test_message("msg3")).await;

        let fetched = tm.get_task(&task.id, Some(2)).await.unwrap();
        assert_eq!(fetched.history.len(), 2);
        assert_eq!(fetched.history[0].text_content(), Some("msg2"));
    }

    #[tokio::test]
    async fn update_status() {
        let tm = TaskManager::new();
        let task = tm.create_task(test_message("test")).await;
        let updated = tm
            .update_status(&task.id, TaskState::Working, None)
            .await
            .unwrap();
        assert_eq!(updated.status.state, TaskState::Working);
    }

    #[tokio::test]
    async fn add_artifact() {
        let tm = TaskManager::new();
        let task = tm.create_task(test_message("test")).await;
        let artifact = Artifact {
            artifact_id: "a1".into(),
            name: None,
            parts: vec![crate::types::Part::text("result")],
            metadata: None,
        };
        let updated = tm.add_artifact(&task.id, artifact).await.unwrap();
        assert_eq!(updated.artifacts.len(), 1);
    }

    #[tokio::test]
    async fn cancel_task_success() {
        let tm = TaskManager::new();
        let task = tm.create_task(test_message("test")).await;
        let _ = tm.update_status(&task.id, TaskState::Working, None).await;
        let result = tm.cancel_task(&task.id).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn cancel_completed_task_fails() {
        let tm = TaskManager::new();
        let task = tm.create_task(test_message("test")).await;
        tm.update_status(&task.id, TaskState::Completed, None).await;
        let result = tm.cancel_task(&task.id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_nonexistent_task() {
        let tm = TaskManager::new();
        assert!(tm.get_task("nonexistent", None).await.is_none());
    }

    #[tokio::test]
    async fn cancel_all_terminal_states_rejected() {
        let tm = TaskManager::new();
        for terminal in [TaskState::Failed, TaskState::Canceled, TaskState::Rejected] {
            let task = tm.create_task(test_message("test")).await;
            tm.update_status(&task.id, terminal, None).await;
            let result = tm.cancel_task(&task.id).await;
            assert!(result.is_err(), "expected cancel to fail for {terminal:?}");
        }
    }

    #[tokio::test]
    async fn cancel_submitted_task_succeeds() {
        let tm = TaskManager::new();
        let task = tm.create_task(test_message("test")).await;
        let result = tm.cancel_task(&task.id).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn history_limit_gte_len_returns_all() {
        let tm = TaskManager::new();
        let task = tm.create_task(test_message("msg1")).await;
        tm.append_history(&task.id, test_message("msg2")).await;

        let fetched = tm.get_task(&task.id, Some(5)).await.unwrap();
        assert_eq!(fetched.history.len(), 2);

        let fetched_exact = tm.get_task(&task.id, Some(2)).await.unwrap();
        assert_eq!(fetched_exact.history.len(), 2);
    }

    #[tokio::test]
    async fn append_history_nonexistent_returns_none() {
        let tm = TaskManager::new();
        assert!(
            tm.append_history("no-such-id", test_message("x"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn update_status_nonexistent_returns_none() {
        let tm = TaskManager::new();
        assert!(
            tm.update_status("no-such-id", TaskState::Working, None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn add_artifact_nonexistent_returns_none() {
        let tm = TaskManager::new();
        let artifact = Artifact {
            artifact_id: "a".into(),
            name: None,
            parts: vec![],
            metadata: None,
        };
        assert!(tm.add_artifact("no-such-id", artifact).await.is_none());
    }

    #[tokio::test]
    async fn cancel_nonexistent_returns_not_found() {
        let tm = TaskManager::new();
        let result = tm.cancel_task("no-such-id").await;
        assert!(matches!(result, Err(CancelError::NotFound)));
    }

    #[test]
    fn now_rfc3339_is_valid_rfc3339() {
        let ts = now_rfc3339();
        assert!(ts.contains('T'), "must contain date/time separator");
        // chrono produces RFC3339 with timezone offset or 'Z'; parse back to verify
        chrono::DateTime::parse_from_rfc3339(&ts)
            .expect("now_rfc3339 must produce a valid RFC3339 string");
    }

    #[tokio::test]
    async fn evict_expired_removes_terminal_tasks_past_ttl() {
        let tm = TaskManager::new();

        // Create three tasks
        let t_completed = tm.create_task(test_message("done")).await;
        let t_failed = tm.create_task(test_message("fail")).await;
        let t_working = tm.create_task(test_message("active")).await;

        // Transition to terminal / non-terminal states
        tm.update_status(&t_completed.id, TaskState::Completed, None)
            .await;
        tm.update_status(&t_failed.id, TaskState::Failed, None)
            .await;
        tm.update_status(&t_working.id, TaskState::Working, None)
            .await;

        // Back-date the completed and failed tasks by inserting a past Instant.
        // We use a zero-duration TTL and a slightly-past instant to avoid
        // relying on sub-millisecond timing precision in CI environments.
        {
            let mut ages = tm.created_at.write().await;
            // Subtract 2s to ensure elapsed > TTL of 1s used below
            let old = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
            ages.insert(t_completed.id.clone(), old);
            ages.insert(t_failed.id.clone(), old);
            // t_working keeps its real (recent) instant
        }

        tm.evict_expired(Duration::from_secs(1)).await;

        assert!(
            tm.get_task(&t_completed.id, None).await.is_none(),
            "completed task past TTL must be evicted"
        );
        assert!(
            tm.get_task(&t_failed.id, None).await.is_none(),
            "failed task past TTL must be evicted"
        );
        assert!(
            tm.get_task(&t_working.id, None).await.is_some(),
            "non-terminal task must not be evicted"
        );
    }

    #[tokio::test]
    async fn evict_expired_keeps_terminal_tasks_within_ttl() {
        let tm = TaskManager::new();

        let task = tm.create_task(test_message("recent done")).await;
        tm.update_status(&task.id, TaskState::Completed, None).await;

        // task was just created — elapsed ≈ 0, well within 1-hour TTL
        tm.evict_expired(Duration::from_hours(1)).await;

        assert!(
            tm.get_task(&task.id, None).await.is_some(),
            "recently completed task within TTL must not be evicted"
        );
    }

    #[tokio::test]
    async fn evict_expired_handles_all_terminal_states() {
        let tm = TaskManager::new();

        for state in [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
        ] {
            let task = tm.create_task(test_message("terminal")).await;
            tm.update_status(&task.id, state, None).await;

            // Back-date the task by 2s so elapsed > TTL of 1s used below
            {
                let old = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
                tm.created_at.write().await.insert(task.id.clone(), old);
            }

            tm.evict_expired(Duration::from_secs(1)).await;

            assert!(
                tm.get_task(&task.id, None).await.is_none(),
                "terminal state {state:?} past TTL must be evicted"
            );
        }
    }

    #[tokio::test]
    async fn evict_expired_never_evicts_non_terminal_tasks_past_ttl() {
        // Even when a non-terminal task is older than the TTL it must NOT be removed,
        // because it may still be actively polled or streamed by a client.
        let tm = TaskManager::new();
        let old = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();

        for state in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::InputRequired,
        ] {
            let task = tm.create_task(test_message("active")).await;
            tm.update_status(&task.id, state, None).await;
            tm.created_at.write().await.insert(task.id.clone(), old);

            tm.evict_expired(Duration::from_secs(1)).await;

            assert!(
                tm.get_task(&task.id, None).await.is_some(),
                "non-terminal state {state:?} must not be evicted even past TTL"
            );
        }
    }

    #[tokio::test]
    async fn evict_expired_empty_store_is_noop() {
        let tm = TaskManager::new();
        // Must not panic on an empty store.
        tm.evict_expired(Duration::from_secs(0)).await;
        assert!(tm.get_task("any", None).await.is_none());
    }
}
