use zeph_llm::provider::{LlmProvider, Role};
use zeph_memory::sqlite::role_str;
use zeph_tools::executor::ToolExecutor;

use crate::channel::Channel;

use super::Agent;

impl<P: LlmProvider + Clone + 'static, C: Channel, T: ToolExecutor> Agent<P, C, T> {
    /// Load conversation history from memory and inject into messages.
    ///
    /// # Errors
    ///
    /// Returns an error if loading history from `SQLite` fails.
    pub async fn load_history(&mut self) -> anyhow::Result<()> {
        let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        else {
            return Ok(());
        };

        let history = memory
            .sqlite()
            .load_history(cid, self.memory_state.history_limit)
            .await?;
        if !history.is_empty() {
            let mut loaded = 0;
            let mut skipped = 0;

            for msg in history {
                if msg.content.trim().is_empty() {
                    tracing::warn!("skipping empty message from history (role: {:?})", msg.role);
                    skipped += 1;
                    continue;
                }
                self.messages.push(msg);
                loaded += 1;
            }

            tracing::info!("restored {loaded} message(s) from conversation {cid}");
            if skipped > 0 {
                tracing::warn!("skipped {skipped} empty message(s) from history");
            }
        }

        if let Ok(count) = memory.message_count(cid).await {
            let count_u64 = u64::try_from(count).unwrap_or(0);
            self.update_metrics(|m| {
                m.sqlite_message_count = count_u64;
            });
        }

        Ok(())
    }

    pub(crate) async fn persist_message(&mut self, role: Role, content: &str) {
        let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        else {
            return;
        };

        let parts_json = self
            .messages
            .last()
            .filter(|m| !m.parts.is_empty())
            .and_then(|m| serde_json::to_string(&m.parts).ok())
            .unwrap_or_else(|| "[]".to_string());

        let (_message_id, embedding_stored) = match memory
            .remember_with_parts(cid, role_str(role), content, &parts_json)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                tracing::error!("failed to persist message: {e:#}");
                return;
            }
        };

        self.update_metrics(|m| {
            m.sqlite_message_count += 1;
            if embedding_stored {
                m.embeddings_generated += 1;
            }
        });

        self.check_summarization().await;
    }

    pub(crate) async fn check_summarization(&mut self) {
        let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        else {
            return;
        };

        let count = match memory.unsummarized_message_count(cid).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to get unsummarized message count: {e:#}");
                return;
            }
        };

        let count_usize = match usize::try_from(count) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("message count overflow: {e:#}");
                return;
            }
        };

        if count_usize > self.memory_state.summarization_threshold {
            let _ = self.channel.send_status("summarizing...").await;
            let batch_size = self.memory_state.summarization_threshold / 2;
            match memory.summarize(cid, batch_size).await {
                Ok(Some(summary_id)) => {
                    tracing::info!("created summary {summary_id} for conversation {cid}");
                    self.update_metrics(|m| {
                        m.summaries_count += 1;
                    });
                }
                Ok(None) => {
                    tracing::debug!("no summarization needed");
                }
                Err(e) => {
                    tracing::error!("summarization failed: {e:#}");
                }
            }
            let _ = self.channel.send_status("").await;
        }
    }
}
