use std::time::Duration;

use tokio::process::Command;
use zeph_llm::provider::{LlmProvider, Message, Role};
use zeph_memory::sqlite::{SqliteStore, role_str};

use crate::channel::Channel;
use crate::context::build_system_prompt;

const MAX_SHELL_ITERATIONS: usize = 3;
const SHELL_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Agent<P: LlmProvider, C: Channel> {
    provider: P,
    channel: C,
    messages: Vec<Message>,
    memory: Option<SqliteStore>,
    conversation_id: Option<i64>,
    history_limit: u32,
}

impl<P: LlmProvider, C: Channel> Agent<P, C> {
    #[must_use]
    pub fn new(provider: P, channel: C, skills_prompt: &str) -> Self {
        let system_prompt = build_system_prompt(skills_prompt);
        Self {
            provider,
            channel,
            messages: vec![Message {
                role: Role::System,
                content: system_prompt,
            }],
            memory: None,
            conversation_id: None,
            history_limit: 50,
        }
    }

    #[must_use]
    pub fn with_memory(mut self, store: SqliteStore, conversation_id: i64, limit: u32) -> Self {
        self.memory = Some(store);
        self.conversation_id = Some(conversation_id);
        self.history_limit = limit;
        self
    }

    /// Load conversation history from memory and inject into messages.
    ///
    /// # Errors
    ///
    /// Returns an error if loading history from `SQLite` fails.
    pub async fn load_history(&mut self) -> anyhow::Result<()> {
        let (Some(store), Some(cid)) = (&self.memory, self.conversation_id) else {
            return Ok(());
        };

        let history = store.load_history(cid, self.history_limit).await?;
        if !history.is_empty() {
            tracing::info!("restored {} message(s) from conversation {cid}", history.len());
            for msg in history {
                self.messages.push(msg);
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
            let Some(incoming) = self.channel.recv().await? else {
                break;
            };

            self.messages.push(Message {
                role: Role::User,
                content: incoming.text.clone(),
            });
            self.persist_message(Role::User, &incoming.text).await;

            if let Err(e) = self.process_response().await {
                tracing::error!("LLM error: {e:#}");
                self.channel
                    .send(&format!("Error: {e:#}"))
                    .await?;
                self.messages.pop();
            }
        }

        Ok(())
    }

    async fn process_response(&mut self) -> anyhow::Result<()> {
        for _ in 0..MAX_SHELL_ITERATIONS {
            self.channel.send_typing().await?;

            let response = self.provider.chat(&self.messages).await?;
            self.channel.send(&response).await?;

            self.messages.push(Message {
                role: Role::Assistant,
                content: response.clone(),
            });
            self.persist_message(Role::Assistant, &response).await;

            let Some(output) = extract_and_execute_bash(&response).await else {
                return Ok(());
            };

            self.channel.send(&format!("[shell output]\n{output}")).await?;

            let shell_msg = format!("[shell output]\n{output}");
            self.messages.push(Message {
                role: Role::User,
                content: shell_msg.clone(),
            });
            self.persist_message(Role::User, &shell_msg).await;
        }

        Ok(())
    }

    async fn persist_message(&self, role: Role, content: &str) {
        let (Some(store), Some(cid)) = (&self.memory, self.conversation_id) else {
            return;
        };
        if let Err(e) = store.save_message(cid, role_str(role), content).await {
            tracing::error!("failed to persist message: {e:#}");
        }
    }
}

fn extract_bash_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find("```bash") {
        let code_start = start + 7;
        let after = &rest[code_start..];
        if let Some(end) = after.find("```") {
            blocks.push(after[..end].trim());
            rest = &after[end + 3..];
        } else {
            break;
        }
    }

    blocks
}

async fn execute_bash(code: &str) -> anyhow::Result<String> {
    let result = tokio::time::timeout(
        SHELL_TIMEOUT,
        Command::new("bash").arg("-c").arg(code).output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut combined = String::new();
            if !stdout.is_empty() {
                combined.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str("[stderr] ");
                combined.push_str(&stderr);
            }
            if combined.is_empty() {
                combined.push_str("(no output)");
            }
            Ok(combined)
        }
        Ok(Err(e)) => Ok(format!("[error] {e}")),
        Err(_) => Ok("[error] command timed out after 30s".to_string()),
    }
}

async fn extract_and_execute_bash(response: &str) -> Option<String> {
    let blocks = extract_bash_blocks(response);
    if blocks.is_empty() {
        return None;
    }

    let mut outputs = Vec::with_capacity(blocks.len());
    for block in blocks {
        match execute_bash(block).await {
            Ok(out) => outputs.push(format!("$ {block}\n{out}")),
            Err(e) => outputs.push(format!("$ {block}\n[error] {e}")),
        }
    }

    Some(outputs.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_single_bash_block() {
        let text = "Here is code:\n```bash\necho hello\n```\nDone.";
        let blocks = extract_bash_blocks(text);
        assert_eq!(blocks, vec!["echo hello"]);
    }

    #[test]
    fn extract_multiple_bash_blocks() {
        let text = "```bash\nls\n```\ntext\n```bash\npwd\n```";
        let blocks = extract_bash_blocks(text);
        assert_eq!(blocks, vec!["ls", "pwd"]);
    }

    #[test]
    fn ignore_non_bash_blocks() {
        let text = "```python\nprint('hi')\n```\n```bash\necho hi\n```";
        let blocks = extract_bash_blocks(text);
        assert_eq!(blocks, vec!["echo hi"]);
    }

    #[test]
    fn no_blocks() {
        let text = "Just plain text, no code blocks.";
        let blocks = extract_bash_blocks(text);
        assert!(blocks.is_empty());
    }

    #[test]
    fn unclosed_block_ignored() {
        let text = "```bash\necho hello";
        let blocks = extract_bash_blocks(text);
        assert!(blocks.is_empty());
    }

    #[tokio::test]
    async fn execute_bash_simple() {
        let result = execute_bash("echo hello").await.unwrap();
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn execute_bash_stderr() {
        let result = execute_bash("echo err >&2").await.unwrap();
        assert!(result.contains("[stderr]"));
        assert!(result.contains("err"));
    }

    #[tokio::test]
    async fn extract_and_execute_no_blocks() {
        let result = extract_and_execute_bash("plain text").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn extract_and_execute_with_block() {
        let text = "Run this:\n```bash\necho test123\n```";
        let result = extract_and_execute_bash(text).await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("test123"));
    }
}
