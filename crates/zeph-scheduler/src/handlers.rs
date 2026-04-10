// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc;

use crate::error::SchedulerError;
use crate::sanitize::sanitize_task_prompt;
use crate::task::TaskHandler;

/// [`TaskHandler`] that injects a custom prompt into the agent loop.
///
/// When a [`TaskKind::Custom`](crate::TaskKind::Custom) task is due, `CustomTaskHandler`
/// reads the `"task"` field from the task's JSON config, sanitises it with
/// [`crate::sanitize_task_prompt`], and sends the resulting string on the provided
/// `mpsc::Sender`. The agent loop receives the prompt and processes it as a new
/// user message.
///
/// Sending is best-effort: if the channel is full or closed, the error is logged at
/// warn level and `Ok(())` is returned so the scheduler continues running.
///
/// # Examples
///
/// ```rust
/// use tokio::sync::mpsc;
/// use zeph_scheduler::CustomTaskHandler;
///
/// # #[tokio::main]
/// # async fn main() {
/// let (tx, mut rx) = mpsc::channel(8);
/// let handler = CustomTaskHandler::new(tx);
///
/// use zeph_scheduler::TaskHandler;
/// handler
///     .execute(&serde_json::json!({"task": "Generate a daily report"}))
///     .await
///     .expect("handler should not fail");
///
/// let prompt = rx.recv().await.unwrap();
/// assert_eq!(prompt, "Generate a daily report");
/// # }
/// ```
pub struct CustomTaskHandler {
    tx: mpsc::Sender<String>,
}

impl CustomTaskHandler {
    /// Create a new handler that sends prompts on `tx`.
    #[must_use]
    pub fn new(tx: mpsc::Sender<String>) -> Self {
        Self { tx }
    }
}

impl TaskHandler for CustomTaskHandler {
    fn execute(
        &self,
        config: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), SchedulerError>> + Send + '_>> {
        let raw = config
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("Execute the following scheduled task now: check status");
        let prompt = sanitize_task_prompt(raw);
        let tx = self.tx.clone();
        Box::pin(async move {
            if tx.try_send(prompt).is_err() {
                tracing::warn!("custom task handler: agent channel full or closed");
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn custom_handler_sends_task_prompt() {
        let (tx, mut rx) = mpsc::channel(1);
        let handler = CustomTaskHandler::new(tx);
        let config = serde_json::json!({"task": "do something important"});
        handler.execute(&config).await.unwrap();
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg, "do something important");
    }

    #[tokio::test]
    async fn custom_handler_uses_default_when_no_task_field() {
        let (tx, mut rx) = mpsc::channel(1);
        let handler = CustomTaskHandler::new(tx);
        handler.execute(&serde_json::Value::Null).await.unwrap();
        let msg = rx.recv().await.unwrap();
        assert!(msg.contains("Execute the following scheduled task now:"));
    }

    #[tokio::test]
    async fn custom_handler_ok_when_channel_full() {
        let (tx, _rx) = mpsc::channel(1);
        // pre-fill the channel so next try_send will fail
        let _ = tx.try_send("fill".to_owned());
        let handler = CustomTaskHandler::new(tx);
        let config = serde_json::json!({"task": "overflow"});
        let result = handler.execute(&config).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn custom_handler_ok_when_channel_closed() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let handler = CustomTaskHandler::new(tx);
        let config = serde_json::json!({"task": "closed"});
        let result = handler.execute(&config).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn custom_handler_strips_control_chars() {
        let (tx, mut rx) = mpsc::channel(1);
        let handler = CustomTaskHandler::new(tx);
        let config = serde_json::json!({"task": "hello\x01\x00world"});
        handler.execute(&config).await.unwrap();
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg, "helloworld");
    }

    #[tokio::test]
    async fn custom_handler_truncates_long_prompt() {
        let (tx, mut rx) = mpsc::channel(1);
        let handler = CustomTaskHandler::new(tx);
        let long_task = "a".repeat(1000);
        let config = serde_json::json!({"task": long_task});
        handler.execute(&config).await.unwrap();
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.chars().count(), 512);
    }
}
