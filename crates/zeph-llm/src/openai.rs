use std::fmt;
use std::time::Duration;

use crate::error::LlmError;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::provider::{
    ChatResponse, ChatStream, LlmProvider, Message, MessagePart, Role, StatusTx, ToolDefinition,
    ToolUseRequest,
};
use crate::sse::openai_sse_to_stream;

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: u32,
    embedding_model: Option<String>,
    reasoning_effort: Option<String>,
    pub(crate) status_tx: Option<StatusTx>,
    last_cache: std::sync::Mutex<Option<(u64, u64)>>,
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
            .field("last_cache", &self.last_cache.lock().ok())
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
            last_cache: std::sync::Mutex::new(None),
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
            client: crate::http::default_client(),
            api_key,
            base_url,
            model,
            max_tokens,
            embedding_model,
            reasoning_effort,
            status_tx: None,
            last_cache: std::sync::Mutex::new(None),
        }
    }

    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    #[must_use]
    pub fn with_status_tx(mut self, tx: StatusTx) -> Self {
        self.status_tx = Some(tx);
        self
    }

    fn store_cache_usage(&self, usage: &OpenAiUsage) {
        let cached = usage
            .prompt_tokens_details
            .as_ref()
            .map_or(0, |d| d.cached_tokens);
        if cached > 0 {
            if let Ok(mut guard) = self.last_cache.lock() {
                *guard = Some((0, cached));
            }
            tracing::debug!(
                prompt_tokens = usage.prompt_tokens,
                cached_tokens = cached,
                completion_tokens = usage.completion_tokens,
                "OpenAI API usage"
            );
        }
    }

    fn emit_status(&self, msg: impl Into<String>) {
        if let Some(ref tx) = self.status_tx {
            let _ = tx.send(msg.into());
        }
    }

    async fn send_request(&self, messages: &[Message]) -> Result<String, LlmError> {
        let reasoning = self
            .reasoning_effort
            .as_deref()
            .map(|effort| Reasoning { effort });

        let response = if has_image_parts(messages) {
            let vision_messages = convert_messages_vision(messages);
            let body = VisionChatRequest {
                model: &self.model,
                messages: vision_messages,
                max_tokens: self.max_tokens,
                stream: false,
                reasoning,
            };
            self.client
                .post(format!("{}/chat/completions", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await?
        } else {
            let api_messages = convert_messages(messages);
            let body = ChatRequest {
                model: &self.model,
                messages: &api_messages,
                max_tokens: self.max_tokens,
                stream: false,
                reasoning,
            };
            self.client
                .post(format!("{}/chat/completions", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await?
        };

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(LlmError::RateLimited);
        }

        if !status.is_success() {
            tracing::error!("OpenAI API error {status}: {text}");
            return Err(LlmError::Other(format!(
                "OpenAI API request failed (status {status})"
            )));
        }

        let resp: OpenAiChatResponse = serde_json::from_str(&text)?;

        if let Some(ref usage) = resp.usage {
            self.store_cache_usage(usage);
        }

        resp.choices
            .first()
            .map(|c| c.message.content.clone())
            .ok_or(LlmError::EmptyResponse {
                provider: "openai".into(),
            })
    }

    async fn send_stream_request(
        &self,
        messages: &[Message],
    ) -> Result<reqwest::Response, LlmError> {
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
            .await?;

        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(LlmError::RateLimited);
        }

        if !status.is_success() {
            let text = response.text().await.map_err(LlmError::Http)?;
            tracing::error!("OpenAI API streaming request error {status}: {text}");
            return Err(LlmError::Other(format!(
                "OpenAI API streaming request failed (status {status})"
            )));
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

    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        match self.send_request(messages).await {
            Ok(text) => Ok(text),
            Err(LlmError::RateLimited) => {
                self.emit_status("OpenAI rate limited, retrying in 1s");
                tracing::warn!("OpenAI rate limited, retrying in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.send_request(messages).await
            }
            Err(e) => Err(e),
        }
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let response = match self.send_stream_request(messages).await {
            Ok(resp) => resp,
            Err(LlmError::RateLimited) => {
                self.emit_status("OpenAI rate limited, retrying in 1s");
                tracing::warn!("OpenAI rate limited, retrying in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.send_stream_request(messages).await?
            }
            Err(e) => return Err(e),
        };

        Ok(openai_sse_to_stream(response))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let model = self
            .embedding_model
            .as_deref()
            .ok_or(LlmError::EmbedUnsupported {
                provider: "openai".into(),
            })?;

        let body = EmbeddingRequest { input: text, model };

        let response = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            tracing::error!("OpenAI embedding API error {status}: {text}");
            return Err(LlmError::Other(format!(
                "OpenAI embedding request failed (status {status})"
            )));
        }

        let resp: EmbeddingResponse = serde_json::from_str(&text)?;

        resp.data
            .first()
            .map(|d| d.embedding.clone())
            .ok_or(LlmError::EmptyResponse {
                provider: "openai".into(),
            })
    }

    fn supports_embeddings(&self) -> bool {
        self.embedding_model.is_some()
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "openai"
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        self.last_cache.lock().ok().and_then(|g| *g)
    }

    fn supports_structured_output(&self) -> bool {
        true
    }

    async fn chat_typed<T>(&self, messages: &[Message]) -> Result<T, LlmError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
        Self: Sized,
    {
        let (schema_value, _) = crate::provider::cached_schema::<T>()?;
        let type_name = std::any::type_name::<T>()
            .rsplit("::")
            .next()
            .unwrap_or("Output");

        let api_messages = convert_messages(messages);
        let body = TypedChatRequest {
            model: &self.model,
            messages: &api_messages,
            max_tokens: self.max_tokens,
            response_format: ResponseFormat {
                r#type: "json_schema",
                json_schema: JsonSchemaFormat {
                    name: type_name,
                    schema: schema_value,
                    strict: true,
                },
            },
        };

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            return Err(LlmError::Other(format!(
                "OpenAI API request failed (status {status})"
            )));
        }

        let resp: OpenAiChatResponse = serde_json::from_str(&text)?;
        let content = resp
            .choices
            .first()
            .map(|c| c.message.content.as_str())
            .ok_or(LlmError::EmptyResponse {
                provider: "openai".into(),
            })?;

        serde_json::from_str::<T>(content).map_err(|e| LlmError::StructuredParse(e.to_string()))
    }

    fn supports_vision(&self) -> bool {
        true
    }

    fn supports_tool_use(&self) -> bool {
        true
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let api_messages = convert_messages_structured(messages);
        let reasoning = self
            .reasoning_effort
            .as_deref()
            .map(|effort| Reasoning { effort });

        let api_tools: Vec<OpenAiTool> = tools
            .iter()
            .map(|t| OpenAiTool {
                r#type: "function",
                function: OpenAiFunction {
                    name: &t.name,
                    description: &t.description,
                    parameters: &t.parameters,
                },
            })
            .collect();

        let body = ToolChatRequest {
            model: &self.model,
            messages: &api_messages,
            max_tokens: self.max_tokens,
            tools: &api_tools,
            reasoning,
        };

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(LlmError::RateLimited);
        }

        if !status.is_success() {
            tracing::error!("OpenAI API error {status}: {text}");
            return Err(LlmError::Other(format!(
                "OpenAI API request failed (status {status})"
            )));
        }

        let resp: ToolChatResponse = serde_json::from_str(&text)?;

        if let Some(ref usage) = resp.usage {
            self.store_cache_usage(usage);
        }

        let choice = resp
            .choices
            .into_iter()
            .next()
            .ok_or(LlmError::EmptyResponse {
                provider: "openai".into(),
            })?;

        if let Some(tool_calls) = choice.message.tool_calls
            && !tool_calls.is_empty()
        {
            let text = if choice.message.content.is_empty() {
                None
            } else {
                Some(choice.message.content)
            };
            let calls = tool_calls
                .into_iter()
                .map(|tc| {
                    let input = serde_json::from_str(&tc.function.arguments)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                    ToolUseRequest {
                        id: tc.id,
                        name: tc.function.name,
                        input,
                    }
                })
                .collect();
            return Ok(ChatResponse::ToolUse {
                text,
                tool_calls: calls,
            });
        }

        Ok(ChatResponse::Text(choice.message.content))
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlDetail },
}

#[derive(Serialize)]
struct ImageUrlDetail {
    url: String,
}

#[derive(Serialize)]
struct VisionApiMessage {
    role: String,
    content: Vec<OpenAiContentPart>,
}

#[derive(Serialize)]
struct VisionChatRequest<'a> {
    model: &'a str,
    messages: Vec<VisionApiMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
}

fn has_image_parts(messages: &[Message]) -> bool {
    messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Image { .. }))
    })
}

fn convert_messages_vision(messages: &[Message]) -> Vec<VisionApiMessage> {
    messages
        .iter()
        .map(|msg| {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            let has_images = msg
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::Image { .. }));
            if has_images {
                let mut parts = Vec::new();
                let text_str: String = msg
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        MessagePart::Text { text }
                        | MessagePart::Recall { text }
                        | MessagePart::CodeContext { text }
                        | MessagePart::Summary { text }
                        | MessagePart::CrossSession { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !text_str.is_empty() {
                    parts.push(OpenAiContentPart::Text { text: text_str });
                }
                for part in &msg.parts {
                    if let MessagePart::Image { data, mime_type } = part {
                        let b64 = STANDARD.encode(data);
                        parts.push(OpenAiContentPart::ImageUrl {
                            image_url: ImageUrlDetail {
                                url: format!("data:{mime_type};base64,{b64}"),
                            },
                        });
                    }
                }
                if parts.is_empty() {
                    parts.push(OpenAiContentPart::Text {
                        text: msg.to_llm_content().to_owned(),
                    });
                }
                VisionApiMessage {
                    role: role.to_owned(),
                    content: parts,
                }
            } else {
                VisionApiMessage {
                    role: role.to_owned(),
                    content: vec![OpenAiContentPart::Text {
                        text: msg.to_llm_content().to_owned(),
                    }],
                }
            }
        })
        .collect()
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
struct OpenAiChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

#[derive(Serialize)]
struct OpenAiTool<'a> {
    r#type: &'a str,
    function: OpenAiFunction<'a>,
}

#[derive(Serialize)]
struct OpenAiFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Serialize)]
struct ToolChatRequest<'a> {
    model: &'a str,
    messages: &'a [StructuredApiMessage],
    max_tokens: u32,
    tools: &'a [OpenAiTool<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
}

#[derive(Serialize)]
struct StructuredApiMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiToolCallOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct OpenAiToolCallOut {
    id: String,
    r#type: String,
    function: OpenAiFunctionCall,
}

#[derive(Serialize)]
struct OpenAiFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ToolChatResponse {
    choices: Vec<ToolChatChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct ToolChatChoice {
    message: ToolChatMessage,
}

#[derive(Deserialize)]
struct ToolChatMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Deserialize)]
struct OpenAiToolCall {
    id: String,
    function: OpenAiToolCallFunction,
}

#[derive(Deserialize)]
struct OpenAiToolCallFunction {
    name: String,
    arguments: String,
}

fn convert_messages_structured(messages: &[Message]) -> Vec<StructuredApiMessage> {
    let mut result = Vec::new();

    for msg in messages {
        let has_tool_parts = msg.parts.iter().any(|p| {
            matches!(
                p,
                MessagePart::ToolUse { .. } | MessagePart::ToolResult { .. }
            )
        });

        if has_tool_parts {
            // Assistant messages with ToolUse parts → tool_calls field
            if msg.role == Role::Assistant {
                let text_content: String = msg
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        MessagePart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");

                let tool_calls: Vec<OpenAiToolCallOut> = msg
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        MessagePart::ToolUse { id, name, input } => Some(OpenAiToolCallOut {
                            id: id.clone(),
                            r#type: "function".to_owned(),
                            function: OpenAiFunctionCall {
                                name: name.clone(),
                                arguments: serde_json::to_string(input).unwrap_or_default(),
                            },
                        }),
                        _ => None,
                    })
                    .collect();

                result.push(StructuredApiMessage {
                    role: "assistant".to_owned(),
                    content: text_content,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                });
            } else {
                // User messages with ToolResult parts → role: "tool" messages
                for part in &msg.parts {
                    match part {
                        MessagePart::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            result.push(StructuredApiMessage {
                                role: "tool".to_owned(),
                                content: content.clone(),
                                tool_calls: None,
                                tool_call_id: Some(tool_use_id.clone()),
                            });
                        }
                        MessagePart::Text { text } if !text.is_empty() => {
                            result.push(StructuredApiMessage {
                                role: "user".to_owned(),
                                content: text.clone(),
                                tool_calls: None,
                                tool_call_id: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
        } else {
            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            result.push(StructuredApiMessage {
                role: role.to_owned(),
                content: msg.to_llm_content().to_owned(),
                tool_calls: None,
                tool_call_id: None,
            });
        }
    }

    result
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

#[derive(Serialize)]
struct TypedChatRequest<'a> {
    model: &'a str,
    messages: &'a [ApiMessage<'a>],
    max_tokens: u32,
    response_format: ResponseFormat<'a>,
}

#[derive(Serialize)]
struct ResponseFormat<'a> {
    r#type: &'a str,
    json_schema: JsonSchemaFormat<'a>,
}

#[derive(Serialize)]
struct JsonSchemaFormat<'a> {
    name: &'a str,
    schema: serde_json::Value,
    strict: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

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
        let resp: OpenAiChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content, "Hello!");
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
                .contains("embedding not supported")
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
    fn chat_response_empty_choices() {
        let json = r#"{"choices":[]}"#;
        let resp: OpenAiChatResponse = serde_json::from_str(json).unwrap();
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

    #[test]
    fn supports_tool_use_returns_true() {
        assert!(test_provider().supports_tool_use());
    }

    #[test]
    fn openai_tool_serialization() {
        let tool = OpenAiTool {
            r#type: "function",
            function: OpenAiFunction {
                name: "bash",
                description: "Execute a shell command",
                parameters: &serde_json::json!({
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"]
                }),
            },
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"type\":\"function\""));
        assert!(json.contains("\"name\":\"bash\""));
        assert!(json.contains("\"parameters\""));
    }

    #[test]
    fn parse_tool_chat_response_with_tool_calls() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": "I'll run that",
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"ls\"}"
                        }
                    }]
                }
            }]
        }"#;
        let resp: ToolChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        let tc = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_123");
        assert_eq!(tc[0].function.name, "bash");
    }

    #[test]
    fn parse_tool_chat_response_text_only() {
        let json = r#"{"choices":[{"message":{"content":"Hello!"}}]}"#;
        let resp: ToolChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.choices[0].message.tool_calls.is_none());
    }

    #[test]
    fn convert_messages_structured_with_tool_parts() {
        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![
                    MessagePart::Text {
                        text: "Running command".into(),
                    },
                    MessagePart::ToolUse {
                        id: "call_1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                ],
            ),
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "file1.rs".into(),
                    is_error: false,
                }],
            ),
        ];
        let result = convert_messages_structured(&messages);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].role, "assistant");
        assert!(result[0].tool_calls.is_some());
        assert_eq!(result[1].role, "tool");
        assert_eq!(result[1].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn convert_messages_structured_plain_messages() {
        let messages = vec![Message::from_legacy(Role::User, "hello")];
        let result = convert_messages_structured(&messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[0].content, "hello");
        assert!(result[0].tool_calls.is_none());
    }

    #[test]
    fn parse_usage_with_cached_tokens() {
        let json = r#"{
            "prompt_tokens": 2006,
            "completion_tokens": 300,
            "prompt_tokens_details": {
                "cached_tokens": 1920
            }
        }"#;
        let usage: OpenAiUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.prompt_tokens, 2006);
        assert_eq!(usage.completion_tokens, 300);
        assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, 1920);
    }

    #[test]
    fn parse_usage_without_cached_tokens() {
        let json = r#"{"prompt_tokens": 100, "completion_tokens": 50}"#;
        let usage: OpenAiUsage = serde_json::from_str(json).unwrap();
        assert!(usage.prompt_tokens_details.is_none());
    }

    #[test]
    fn parse_chat_response_with_usage() {
        let json = r#"{
            "choices": [{"message": {"content": "Hello!"}}],
            "usage": {
                "prompt_tokens": 500,
                "completion_tokens": 100,
                "prompt_tokens_details": {"cached_tokens": 400}
            }
        }"#;
        let resp: OpenAiChatResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 500);
        assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, 400);
    }

    #[test]
    fn parse_chat_response_without_usage() {
        let json = r#"{"choices":[{"message":{"content":"Hi"}}]}"#;
        let resp: OpenAiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.usage.is_none());
    }

    #[test]
    fn last_cache_usage_initially_none() {
        let p = test_provider();
        assert!(p.last_cache_usage().is_none());
    }

    #[test]
    fn store_and_retrieve_cache_usage() {
        let p = test_provider();
        let usage = OpenAiUsage {
            prompt_tokens: 1000,
            completion_tokens: 200,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 800 }),
        };
        p.store_cache_usage(&usage);
        let (creation, read) = p.last_cache_usage().unwrap();
        assert_eq!(creation, 0);
        assert_eq!(read, 800);
    }

    #[test]
    fn store_cache_usage_zero_cached_tokens_not_stored() {
        let p = test_provider();
        let usage = OpenAiUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 0 }),
        };
        p.store_cache_usage(&usage);
        assert!(p.last_cache_usage().is_none());
    }

    #[test]
    fn clone_resets_last_cache() {
        let p = test_provider();
        let usage = OpenAiUsage {
            prompt_tokens: 500,
            completion_tokens: 100,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 400 }),
        };
        p.store_cache_usage(&usage);
        assert!(p.last_cache_usage().is_some());
        let cloned = p.clone();
        assert!(cloned.last_cache_usage().is_none());
    }

    #[test]
    fn has_image_parts_detects_image() {
        let msg_with_image = Message::from_parts(
            Role::User,
            vec![
                MessagePart::Text {
                    text: "look".into(),
                },
                MessagePart::Image {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                },
            ],
        );
        let msg_text_only = Message::from_legacy(Role::User, "plain");
        assert!(has_image_parts(&[msg_with_image]));
        assert!(!has_image_parts(&[msg_text_only]));
        assert!(!has_image_parts(&[]));
    }

    #[test]
    fn convert_messages_vision_produces_data_uri() {
        let data = vec![0xFFu8, 0xD8, 0xFF]; // JPEG magic bytes
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::Text {
                    text: "describe this".into(),
                },
                MessagePart::Image {
                    data: data.clone(),
                    mime_type: "image/jpeg".into(),
                },
            ],
        );
        let converted = convert_messages_vision(&[msg]);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
        // Should have text part + image_url part
        assert_eq!(converted[0].content.len(), 2);
        match &converted[0].content[0] {
            OpenAiContentPart::Text { text } => assert_eq!(text, "describe this"),
            _ => panic!("expected Text part first"),
        }
        match &converted[0].content[1] {
            OpenAiContentPart::ImageUrl { image_url } => {
                use base64::{Engine, engine::general_purpose::STANDARD};
                let expected = format!("data:image/jpeg;base64,{}", STANDARD.encode(&data));
                assert_eq!(image_url.url, expected);
            }
            _ => panic!("expected ImageUrl part second"),
        }
    }

    #[test]
    fn convert_messages_vision_text_only_message() {
        let msg = Message::from_legacy(Role::System, "system prompt");
        let converted = convert_messages_vision(&[msg]);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "system");
        assert_eq!(converted[0].content.len(), 1);
        match &converted[0].content[0] {
            OpenAiContentPart::Text { text } => assert_eq!(text, "system prompt"),
            _ => panic!("expected Text part"),
        }
    }

    #[test]
    fn convert_messages_vision_image_only_no_text_part() {
        let msg = Message::from_parts(
            Role::User,
            vec![MessagePart::Image {
                data: vec![1],
                mime_type: "image/png".into(),
            }],
        );
        let converted = convert_messages_vision(&[msg]);
        // No text parts collected → only image_url
        assert_eq!(converted[0].content.len(), 1);
        assert!(matches!(
            &converted[0].content[0],
            OpenAiContentPart::ImageUrl { .. }
        ));
    }
}
