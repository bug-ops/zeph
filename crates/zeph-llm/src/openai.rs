use std::fmt;
use std::time::Duration;

use anyhow::{Context, bail};
use eventsource_stream::Eventsource;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;

use crate::provider::{ChatStream, LlmProvider, Message, Role, StatusTx};

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: u32,
    embedding_model: Option<String>,
    reasoning_effort: Option<String>,
    pub(crate) status_tx: Option<StatusTx>,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("client", &"<reqwest::Client>")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("embedding_model", &self.embedding_model)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("status_tx", &self.status_tx.is_some())
            .finish()
    }
}

impl Clone for OpenAiProvider {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            api_key: self.api_key.clone(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            embedding_model: self.embedding_model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            status_tx: self.status_tx.clone(),
        }
    }
}

impl OpenAiProvider {
    #[must_use]
    pub fn new(
        api_key: String,
        mut base_url: String,
        model: String,
        max_tokens: u32,
        embedding_model: Option<String>,
        reasoning_effort: Option<String>,
    ) -> Self {
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url,
            model,
            max_tokens,
            embedding_model,
            reasoning_effort,
            status_tx: None,
        }
    }

    #[must_use]
    pub fn with_status_tx(mut self, tx: StatusTx) -> Self {
        self.status_tx = Some(tx);
        self
    }

    fn emit_status(&self, msg: impl Into<String>) {
        if let Some(ref tx) = self.status_tx {
            let _ = tx.send(msg.into());
        }
    }

    async fn send_request(&self, messages: &[Message]) -> anyhow::Result<String> {
        let api_messages = convert_messages(messages);
        let reasoning = self
            .reasoning_effort
            .as_deref()
            .map(|effort| Reasoning { effort });

        let body = ChatRequest {
            model: &self.model,
            messages: &api_messages,
            max_tokens: self.max_tokens,
            stream: false,
            reasoning,
        };

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to send request to OpenAI API")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read response body")?;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(anyhow::anyhow!("rate_limited"));
        }

        if !status.is_success() {
            tracing::error!("OpenAI API error {status}: {text}");
            bail!("OpenAI API request failed (status {status})");
        }

        let resp: ChatResponse =
            serde_json::from_str(&text).context("failed to parse OpenAI API response")?;

        resp.choices
            .first()
            .map(|c| c.message.content.clone())
            .context("empty response from OpenAI API")
    }

    async fn send_stream_request(&self, messages: &[Message]) -> anyhow::Result<reqwest::Response> {
        let api_messages = convert_messages(messages);
        let reasoning = self
            .reasoning_effort
            .as_deref()
            .map(|effort| Reasoning { effort });

        let body = ChatRequest {
            model: &self.model,
            messages: &api_messages,
            max_tokens: self.max_tokens,
            stream: true,
            reasoning,
        };

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to send streaming request to OpenAI API")?;

        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            bail!("rate_limited");
        }

        if !status.is_success() {
            let text = response
                .text()
                .await
                .context("failed to read error response body")?;
            tracing::error!("OpenAI API streaming request error {status}: {text}");
            bail!("OpenAI API streaming request failed (status {status})");
        }

        Ok(response)
    }
}

impl LlmProvider for OpenAiProvider {
    fn context_window(&self) -> Option<usize> {
        if self.model.starts_with("gpt-4o") || self.model.starts_with("gpt-4") {
            Some(128_000)
        } else if self.model.starts_with("gpt-3.5") {
            Some(16_385)
        } else if self.model.starts_with("gpt-5") {
            Some(1_000_000)
        } else {
            None
        }
    }

    async fn chat(&self, messages: &[Message]) -> anyhow::Result<String> {
        match self.send_request(messages).await {
            Ok(text) => Ok(text),
            Err(e) if e.to_string().contains("rate_limited") => {
                self.emit_status("OpenAI rate limited, retrying in 1s");
                tracing::warn!("OpenAI rate limited, retrying in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.send_request(messages).await
            }
            Err(e) => Err(e),
        }
    }

    async fn chat_stream(&self, messages: &[Message]) -> anyhow::Result<ChatStream> {
        let response = match self.send_stream_request(messages).await {
            Ok(resp) => resp,
            Err(e) if e.to_string().contains("rate_limited") => {
                self.emit_status("OpenAI rate limited, retrying in 1s");
                tracing::warn!("OpenAI rate limited, retrying in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.send_stream_request(messages).await?
            }
            Err(e) => return Err(e),
        };

        let event_stream = response.bytes_stream().eventsource();

        let mapped = event_stream.filter_map(|event| match event {
            Ok(event) => parse_sse_event(&event.data),
            Err(e) => Some(Err(anyhow::anyhow!("SSE parse error: {e}"))),
        });

        Ok(Box::pin(mapped))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let model = self
            .embedding_model
            .as_deref()
            .context("OpenAI embedding model not configured")?;

        let body = EmbeddingRequest { input: text, model };

        let response = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to send embedding request to OpenAI API")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read embedding response body")?;

        if !status.is_success() {
            tracing::error!("OpenAI embedding API error {status}: {text}");
            bail!("OpenAI embedding request failed (status {status})");
        }

        let resp: EmbeddingResponse =
            serde_json::from_str(&text).context("failed to parse OpenAI embedding response")?;

        resp.data
            .first()
            .map(|d| d.embedding.clone())
            .context("empty embedding response from OpenAI API")
    }

    fn supports_embeddings(&self) -> bool {
        self.embedding_model.is_some()
    }

    fn name(&self) -> &'static str {
        "openai"
    }
}

fn parse_sse_event(data: &str) -> Option<anyhow::Result<String>> {
    if data == "[DONE]" {
        return None;
    }

    match serde_json::from_str::<StreamChunk>(data) {
        Ok(chunk) => {
            let content = chunk
                .choices
                .first()
                .and_then(|c| c.delta.content.as_deref())
                .unwrap_or_default();

            if content.is_empty() {
                None
            } else {
                Some(Ok(content.to_owned()))
            }
        }
        Err(e) => Some(Err(anyhow::anyhow!("failed to parse SSE data: {e}"))),
    }
}

fn convert_messages(messages: &[Message]) -> Vec<ApiMessage<'_>> {
    messages
        .iter()
        .map(|msg| {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            ApiMessage {
                role,
                content: msg.to_llm_content(),
            }
        })
        .collect()
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ApiMessage<'a>],
    max_tokens: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
}

#[derive(Serialize)]
struct Reasoning<'a> {
    effort: &'a str,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    input: &'a str,
    model: &'a str,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_provider() -> OpenAiProvider {
        OpenAiProvider::new(
            "sk-test-key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-5.2".into(),
            4096,
            Some("text-embedding-3-small".into()),
            None,
        )
    }

    #[test]
    fn context_window_gpt4o() {
        let p = OpenAiProvider::new(
            "k".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
            1024,
            None,
            None,
        );
        assert_eq!(p.context_window(), Some(128_000));
    }

    #[test]
    fn context_window_gpt5() {
        assert_eq!(test_provider().context_window(), Some(1_000_000));
    }

    #[test]
    fn context_window_unknown() {
        let p = OpenAiProvider::new(
            "k".into(),
            "https://api.openai.com/v1".into(),
            "custom-model".into(),
            1024,
            None,
            None,
        );
        assert!(p.context_window().is_none());
    }

    fn test_provider_no_embed() -> OpenAiProvider {
        OpenAiProvider::new(
            "sk-test-key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-5.2".into(),
            4096,
            None,
            None,
        )
    }

    #[test]
    fn new_stores_fields() {
        let p = test_provider();
        assert_eq!(p.api_key, "sk-test-key");
        assert_eq!(p.base_url, "https://api.openai.com/v1");
        assert_eq!(p.model, "gpt-5.2");
        assert_eq!(p.max_tokens, 4096);
        assert_eq!(p.embedding_model.as_deref(), Some("text-embedding-3-small"));
        assert!(p.reasoning_effort.is_none());
    }

    #[test]
    fn new_with_reasoning_effort() {
        let p = OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-5.2".into(),
            4096,
            None,
            Some("high".into()),
        );
        assert_eq!(p.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn clone_preserves_fields() {
        let p = test_provider();
        let c = p.clone();
        assert_eq!(c.api_key, p.api_key);
        assert_eq!(c.base_url, p.base_url);
        assert_eq!(c.model, p.model);
        assert_eq!(c.max_tokens, p.max_tokens);
        assert_eq!(c.embedding_model, p.embedding_model);
    }

    #[test]
    fn debug_redacts_api_key() {
        let p = test_provider();
        let debug = format!("{p:?}");
        assert!(!debug.contains("sk-test-key"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("gpt-5.2"));
        assert!(debug.contains("api.openai.com"));
    }

    #[test]
    fn supports_streaming_returns_true() {
        assert!(test_provider().supports_streaming());
    }

    #[test]
    fn supports_embeddings_with_model() {
        assert!(test_provider().supports_embeddings());
    }

    #[test]
    fn supports_embeddings_without_model() {
        assert!(!test_provider_no_embed().supports_embeddings());
    }

    #[test]
    fn name_returns_openai() {
        assert_eq!(test_provider().name(), "openai");
    }

    #[test]
    fn chat_request_serialization() {
        let msgs = [ApiMessage {
            role: "user",
            content: "hello",
        }];
        let body = ChatRequest {
            model: "gpt-5.2",
            messages: &msgs,
            max_tokens: 1024,
            stream: false,
            reasoning: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"model\":\"gpt-5.2\""));
        assert!(json.contains("\"max_tokens\":1024"));
        assert!(json.contains("\"role\":\"user\""));
        assert!(!json.contains("\"stream\""));
        assert!(!json.contains("\"reasoning\""));
    }

    #[test]
    fn chat_request_with_stream_flag() {
        let msgs = [];
        let body = ChatRequest {
            model: "gpt-5.2",
            messages: &msgs,
            max_tokens: 100,
            stream: true,
            reasoning: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"stream\":true"));
    }

    #[test]
    fn chat_request_with_reasoning_effort() {
        let msgs = [];
        let body = ChatRequest {
            model: "gpt-5.2",
            messages: &msgs,
            max_tokens: 100,
            stream: false,
            reasoning: Some(Reasoning { effort: "medium" }),
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"reasoning\":{\"effort\":\"medium\"}"));
    }

    #[test]
    fn parse_chat_response() {
        let json = r#"{"choices":[{"message":{"content":"Hello!"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content, "Hello!");
    }

    #[test]
    fn parse_stream_chunk_with_content() {
        let data = r#"{"choices":[{"delta":{"content":"Hello"}}]}"#;
        let result = parse_sse_event(data);
        assert_eq!(result.unwrap().unwrap(), "Hello");
    }

    #[test]
    fn parse_stream_chunk_empty_content() {
        let data = r#"{"choices":[{"delta":{"content":""}}]}"#;
        let result = parse_sse_event(data);
        assert!(result.is_none());
    }

    #[test]
    fn parse_stream_chunk_null_content() {
        let data = r#"{"choices":[{"delta":{}}]}"#;
        let result = parse_sse_event(data);
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_done_signal() {
        let result = parse_sse_event("[DONE]");
        assert!(result.is_none());
    }

    #[test]
    fn parse_sse_invalid_json() {
        let result = parse_sse_event("not json");
        let err = result.unwrap().unwrap_err();
        assert!(err.to_string().contains("failed to parse SSE data"));
    }

    #[test]
    fn parse_embedding_response() {
        let json = r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#;
        let resp: EmbeddingResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].embedding, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn embedding_request_serialization() {
        let body = EmbeddingRequest {
            input: "hello world",
            model: "text-embedding-3-small",
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"input\":\"hello world\""));
        assert!(json.contains("\"model\":\"text-embedding-3-small\""));
    }

    #[test]
    fn convert_messages_maps_roles() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "system prompt".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "user msg".into(),
                parts: vec![],
            },
            Message {
                role: Role::Assistant,
                content: "assistant reply".into(),
                parts: vec![],
            },
        ];
        let api_msgs = convert_messages(&messages);
        assert_eq!(api_msgs.len(), 3);
        assert_eq!(api_msgs[0].role, "system");
        assert_eq!(api_msgs[0].content, "system prompt");
        assert_eq!(api_msgs[1].role, "user");
        assert_eq!(api_msgs[2].role, "assistant");
    }

    #[tokio::test]
    async fn chat_unreachable_endpoint_errors() {
        let p = OpenAiProvider::new(
            "key".into(),
            "http://127.0.0.1:1".into(),
            "model".into(),
            100,
            None,
            None,
        );
        let messages = vec![Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
        }];
        assert!(p.chat(&messages).await.is_err());
    }

    #[tokio::test]
    async fn stream_unreachable_endpoint_errors() {
        let p = OpenAiProvider::new(
            "key".into(),
            "http://127.0.0.1:1".into(),
            "model".into(),
            100,
            None,
            None,
        );
        let messages = vec![Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
        }];
        assert!(p.chat_stream(&messages).await.is_err());
    }

    #[tokio::test]
    async fn embed_unreachable_endpoint_errors() {
        let p = OpenAiProvider::new(
            "key".into(),
            "http://127.0.0.1:1".into(),
            "model".into(),
            100,
            Some("embed-model".into()),
            None,
        );
        assert!(p.embed("test").await.is_err());
    }

    #[tokio::test]
    async fn embed_without_model_returns_error() {
        let p = test_provider_no_embed();
        let result = p.embed("test").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("embedding model not configured")
        );
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        let p = OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1/".into(),
            "m".into(),
            100,
            None,
            None,
        );
        assert_eq!(p.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn convert_messages_empty() {
        let msgs = convert_messages(&[]);
        assert!(msgs.is_empty());
    }

    #[test]
    fn api_message_serializes() {
        let msg = ApiMessage {
            role: "user",
            content: "hello",
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello\""));
    }

    #[test]
    fn stream_delta_deserializes_without_content() {
        let json = r#"{}"#;
        let delta: StreamDelta = serde_json::from_str(json).unwrap();
        assert!(delta.content.is_none());
    }

    #[test]
    fn chat_response_empty_choices() {
        let json = r#"{"choices":[]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.choices.is_empty());
    }

    #[test]
    fn embedding_response_empty_data() {
        let json = r#"{"data":[]}"#;
        let resp: EmbeddingResponse = serde_json::from_str(json).unwrap();
        assert!(resp.data.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires ZEPH_OPENAI_API_KEY env var"]
    async fn integration_openai_chat() {
        let api_key =
            std::env::var("ZEPH_OPENAI_API_KEY").expect("ZEPH_OPENAI_API_KEY must be set");
        let provider = OpenAiProvider::new(
            api_key,
            "https://api.openai.com/v1".into(),
            "gpt-5.2".into(),
            256,
            None,
            None,
        );

        let messages = vec![Message {
            role: Role::User,
            content: "Reply with exactly: pong".into(),
            parts: vec![],
        }];

        let response = provider.chat(&messages).await.unwrap();
        assert!(response.to_lowercase().contains("pong"));
    }

    #[tokio::test]
    #[ignore = "requires ZEPH_OPENAI_API_KEY env var"]
    async fn integration_openai_chat_stream() {
        let api_key =
            std::env::var("ZEPH_OPENAI_API_KEY").expect("ZEPH_OPENAI_API_KEY must be set");
        let provider = OpenAiProvider::new(
            api_key,
            "https://api.openai.com/v1".into(),
            "gpt-5.2".into(),
            256,
            None,
            None,
        );

        let messages = vec![Message {
            role: Role::User,
            content: "Reply with exactly: pong".into(),
            parts: vec![],
        }];

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut chunks = Vec::new();

        while let Some(result) = stream.next().await {
            chunks.push(result.unwrap());
        }

        let full_response: String = chunks.concat();
        assert!(!full_response.is_empty());
        assert!(full_response.to_lowercase().contains("pong"));
    }

    #[test]
    fn context_window_gpt35() {
        let p = OpenAiProvider::new(
            "k".into(),
            "https://api.openai.com/v1".into(),
            "gpt-3.5-turbo".into(),
            1024,
            None,
            None,
        );
        assert_eq!(p.context_window(), Some(16_385));
    }

    #[test]
    fn context_window_gpt4_turbo() {
        let p = OpenAiProvider::new(
            "k".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4-turbo".into(),
            1024,
            None,
            None,
        );
        assert_eq!(p.context_window(), Some(128_000));
    }

    #[tokio::test]
    #[ignore = "requires ZEPH_OPENAI_API_KEY env var"]
    async fn integration_openai_embed() {
        let api_key =
            std::env::var("ZEPH_OPENAI_API_KEY").expect("ZEPH_OPENAI_API_KEY must be set");
        let provider = OpenAiProvider::new(
            api_key,
            "https://api.openai.com/v1".into(),
            "gpt-5.2".into(),
            256,
            Some("text-embedding-3-small".into()),
            None,
        );

        let embedding = provider.embed("Hello world").await.unwrap();
        assert!(!embedding.is_empty());
    }
}
