use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::provider::{LlmProvider, Message, Role};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug)]
pub struct ClaudeProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl ClaudeProvider {
    #[must_use]
    pub fn new(api_key: String, model: String, max_tokens: u32) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            max_tokens,
        }
    }

    async fn send_request(&self, messages: &[Message]) -> anyhow::Result<String> {
        let (system, chat_messages) = split_messages(messages);

        let body = RequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system.as_deref(),
            messages: &chat_messages,
        };

        let response = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to send request to Claude API")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read response body")?;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(anyhow::anyhow!("rate_limited"));
        }

        if !status.is_success() {
            bail!("Claude API error {status}: {text}");
        }

        let resp: ApiResponse =
            serde_json::from_str(&text).context("failed to parse Claude API response")?;

        resp.content
            .first()
            .map(|c| c.text.clone())
            .context("empty response from Claude API")
    }
}

impl LlmProvider for ClaudeProvider {
    async fn chat(&self, messages: &[Message]) -> anyhow::Result<String> {
        match self.send_request(messages).await {
            Ok(text) => Ok(text),
            Err(e) if e.to_string().contains("rate_limited") => {
                tracing::warn!("Claude rate limited, retrying in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.send_request(messages).await
            }
            Err(e) => Err(e),
        }
    }

    fn name(&self) -> &'static str {
        "claude"
    }
}

fn split_messages(messages: &[Message]) -> (Option<String>, Vec<ApiMessage<'_>>) {
    let mut system_parts = Vec::new();
    let mut chat = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => system_parts.push(msg.content.as_str()),
            Role::User => chat.push(ApiMessage {
                role: "user",
                content: &msg.content,
            }),
            Role::Assistant => chat.push(ApiMessage {
                role: "assistant",
                content: &msg.content,
            }),
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, chat)
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: &'a [ApiMessage<'a>],
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_messages_extracts_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "You are helpful.".into(),
            },
            Message {
                role: Role::User,
                content: "Hi".into(),
            },
        ];

        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "You are helpful.");
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].role, "user");
    }

    #[test]
    fn split_messages_no_system() {
        let messages = vec![Message {
            role: Role::User,
            content: "Hi".into(),
        }];

        let (system, chat) = split_messages(&messages);
        assert!(system.is_none());
        assert_eq!(chat.len(), 1);
    }

    #[test]
    fn split_messages_multiple_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "Part 1".into(),
            },
            Message {
                role: Role::System,
                content: "Part 2".into(),
            },
            Message {
                role: Role::User,
                content: "Hi".into(),
            },
        ];

        let (system, _) = split_messages(&messages);
        assert_eq!(system.unwrap(), "Part 1\n\nPart 2");
    }

    #[tokio::test]
    #[ignore = "requires ZEPH_CLAUDE_API_KEY env var"]
    async fn integration_claude_chat() {
        let api_key =
            std::env::var("ZEPH_CLAUDE_API_KEY").expect("ZEPH_CLAUDE_API_KEY must be set");
        let provider = ClaudeProvider::new(api_key, "claude-sonnet-4-5-20250929".into(), 256);

        let messages = vec![Message {
            role: Role::User,
            content: "Reply with exactly: pong".into(),
        }];

        let response = provider.chat(&messages).await.unwrap();
        assert!(response.to_lowercase().contains("pong"));
    }
}
