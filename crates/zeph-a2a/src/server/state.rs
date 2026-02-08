use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::types::{AgentCard, Artifact, Message, Task, TaskState, TaskStatus};

#[derive(Clone)]
pub struct AppState {
    pub card: AgentCard,
    pub task_manager: TaskManager,
    pub processor: Arc<dyn TaskProcessor>,
}

/// Trait for processing A2A task messages through the agent pipeline.
pub trait TaskProcessor: Send + Sync {
    fn process(
        &self,
        task_id: String,
        message: Message,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ProcessResult, crate::error::A2aError>> + Send>,
    >;
}

pub struct ProcessResult {
    pub response: Message,
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone)]
pub struct TaskManager {
    tasks: Arc<RwLock<HashMap<String, Task>>>,
}

impl TaskManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

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
        self.tasks.write().await.insert(id, task.clone());
        task
    }

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

    pub async fn add_artifact(&self, id: &str, artifact: Artifact) -> Option<Task> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(id)?;
        task.artifacts.push(artifact);
        Some(task.clone())
    }

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
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum CancelError {
    NotFound,
    NotCancelable(TaskState),
}

pub(super) fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple ISO 8601 without external dep
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since epoch to Y-M-D (simplified leap year calc)
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut year = 1970;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let month_days: [u64; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 0;
    for (i, &md) in month_days.iter().enumerate() {
        if days < md {
            month = i as u64 + 1;
            break;
        }
        days -= md;
    }
    (year, month, days + 1)
}

fn is_leap(y: u64) -> bool {
    y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400))
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
    fn now_rfc3339_format() {
        let ts = now_rfc3339();
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
        assert_eq!(ts.len(), 20);
    }
}
