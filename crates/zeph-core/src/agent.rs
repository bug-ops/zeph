use std::io::{self, BufRead, Write};

use zeph_llm::provider::{LlmProvider, Message, Role};

const SYSTEM_PROMPT: &str = "You are Zeph, a helpful assistant.";

pub struct Agent<P: LlmProvider> {
    provider: P,
    messages: Vec<Message>,
}

impl<P: LlmProvider> Agent<P> {
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            messages: vec![Message {
                role: Role::System,
                content: SYSTEM_PROMPT.into(),
            }],
        }
    }

    /// Run the interactive chat loop, reading from stdin until EOF or "exit"/"quit".
    ///
    /// # Errors
    ///
    /// Returns an error if stdout flushing or stdin reading fails.
    pub async fn run(&mut self) -> anyhow::Result<()> {
        let stdin = io::stdin();
        let mut reader = stdin.lock().lines();

        loop {
            print!("You: ");
            io::stdout().flush()?;

            let Some(Ok(line)) = reader.next() else {
                break;
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "exit" || trimmed == "quit" {
                break;
            }

            self.messages.push(Message {
                role: Role::User,
                content: trimmed.to_string(),
            });

            match self.provider.chat(&self.messages).await {
                Ok(response) => {
                    println!("Zeph: {response}");
                    self.messages.push(Message {
                        role: Role::Assistant,
                        content: response,
                    });
                }
                Err(e) => {
                    tracing::error!("LLM error: {e:#}");
                    eprintln!("Error: {e:#}");
                    self.messages.pop();
                }
            }
        }

        Ok(())
    }
}
