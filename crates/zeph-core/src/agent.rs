use tokio::sync::watch;
use tokio_stream::StreamExt;
use zeph_llm::provider::{LlmProvider, Message, Role};
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::sqlite::role_str;
use zeph_tools::executor::{ToolError, ToolExecutor};

use crate::channel::Channel;
use crate::context::build_system_prompt;

// TODO(M14): Make configurable via AgentConfig (currently hardcoded for MVP)
const MAX_SHELL_ITERATIONS: usize = 3;

pub struct Agent<P: LlmProvider, C: Channel, T: ToolExecutor> {
    provider: P,
    channel: C,
    tool_executor: T,
    messages: Vec<Message>,
    memory: Option<SemanticMemory<P>>,
    conversation_id: Option<i64>,
    history_limit: u32,
    recall_limit: usize,
    summarization_threshold: usize,
    shutdown: watch::Receiver<bool>,
}

impl<P: LlmProvider, C: Channel, T: ToolExecutor> Agent<P, C, T> {
    #[must_use]
    pub fn new(provider: P, channel: C, skills_prompt: &str, tool_executor: T) -> Self {
        let system_prompt = build_system_prompt(skills_prompt);
        let (_tx, rx) = watch::channel(false);
        Self {
            provider,
            channel,
            tool_executor,
            messages: vec![Message {
                role: Role::System,
                content: system_prompt,
            }],
            memory: None,
            conversation_id: None,
            history_limit: 50,
            recall_limit: 5,
            summarization_threshold: 100,
            shutdown: rx,
        }
    }

    #[must_use]
    pub fn with_memory(
        mut self,
        memory: SemanticMemory<P>,
        conversation_id: i64,
        history_limit: u32,
        recall_limit: usize,
        summarization_threshold: usize,
    ) -> Self {
        self.memory = Some(memory);
        self.conversation_id = Some(conversation_id);
        self.history_limit = history_limit;
        self.recall_limit = recall_limit;
        self.summarization_threshold = summarization_threshold;
        self
    }

    #[must_use]
    pub fn with_shutdown(mut self, rx: watch::Receiver<bool>) -> Self {
        self.shutdown = rx;
        self
    }

    /// Load conversation history from memory and inject into messages.
    ///
    /// # Errors
    ///
    /// Returns an error if loading history from `SQLite` fails.
    pub async fn load_history(&mut self) -> anyhow::Result<()> {
        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return Ok(());
        };

        let history = memory
            .sqlite()
            .load_history(cid, self.history_limit)
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
        Ok(())
    }

    /// Run the chat loop, receiving messages via the channel until EOF or shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if channel I/O or LLM communication fails.
    pub async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            let incoming = tokio::select! {
                result = self.channel.recv() => result?,
                () = shutdown_signal(&mut self.shutdown) => {
                    tracing::info!("shutting down");
                    break;
                }
            };

            let Some(incoming) = incoming else {
                break;
            };

            self.messages.push(Message {
                role: Role::User,
                content: incoming.text.clone(),
            });
            self.persist_message(Role::User, &incoming.text).await;

            if let Err(e) = self.process_response().await {
                tracing::error!("Response processing failed: {e:#}");
                self.channel
                    .send("An error occurred while processing your request. Please try again.")
                    .await?;
                self.messages.pop();
            }
        }

        Ok(())
    }

    async fn process_response(&mut self) -> anyhow::Result<()> {
        for _ in 0..MAX_SHELL_ITERATIONS {
            self.channel.send_typing().await?;

            let response = if self.provider.supports_streaming() {
                self.process_response_streaming().await?
            } else {
                let resp = self.provider.chat(&self.messages).await?;
                self.channel.send(&resp).await?;
                resp
            };

            if response.trim().is_empty() {
                tracing::warn!("received empty response from LLM, skipping");
                self.channel
                    .send("Received an empty response. Please try again.")
                    .await?;
                return Ok(());
            }

            self.messages.push(Message {
                role: Role::Assistant,
                content: response.clone(),
            });
            self.persist_message(Role::Assistant, &response).await;

            match self.tool_executor.execute(&response).await {
                Ok(Some(output)) => {
                    if output.summary.trim().is_empty() {
                        tracing::warn!("tool execution returned empty output");
                        return Ok(());
                    }

                    let formatted_output = format!("[shell output]\n```\n{output}\n```");
                    self.channel.send(&formatted_output).await?;

                    self.messages.push(Message {
                        role: Role::User,
                        content: formatted_output.clone(),
                    });
                    self.persist_message(Role::User, &formatted_output).await;
                }
                Ok(None) => return Ok(()),
                Err(ToolError::Blocked { command }) => {
                    tracing::warn!("blocked command: {command}");
                    let error_msg = "This command is blocked by security policy.".to_string();
                    self.channel.send(&error_msg).await?;
                    return Ok(());
                }
                Err(e) => {
                    tracing::error!("tool execution error: {e:#}");
                    self.channel
                        .send("Tool execution failed. Please try a different approach.")
                        .await?;
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    async fn process_response_streaming(&mut self) -> anyhow::Result<String> {
        let mut stream = self.provider.chat_stream(&self.messages).await?;
        let mut response = String::with_capacity(2048);

        while let Some(chunk_result) = stream.next().await {
            let chunk: String = chunk_result?;
            response.push_str(&chunk);
            self.channel.send_chunk(&chunk).await?;
        }

        self.channel.flush_chunks().await?;
        Ok(response)
    }

    async fn persist_message(&self, role: Role, content: &str) {
        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return;
        };
        if let Err(e) = memory.remember(cid, role_str(role), content).await {
            tracing::error!("failed to persist message: {e:#}");
            return;
        }

        self.check_summarization().await;
    }

    async fn check_summarization(&self) {
        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return;
        };

        let count = match memory.message_count(cid).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to get message count: {e:#}");
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

        if count_usize > self.summarization_threshold {
            let batch_size = self.summarization_threshold / 2;
            match memory.summarize(cid, batch_size).await {
                Ok(Some(summary_id)) => {
                    tracing::info!("created summary {summary_id} for conversation {cid}");
                }
                Ok(None) => {
                    tracing::debug!("no summarization needed");
                }
                Err(e) => {
                    tracing::error!("summarization failed: {e:#}");
                }
            }
        }
    }
}

async fn shutdown_signal(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}
