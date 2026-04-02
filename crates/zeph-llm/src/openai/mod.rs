// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;
use std::time::Duration;

use crate::error::LlmError;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::provider::{
    ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, MessagePart, Role,
    StatusTx, ToolDefinition, ToolUseRequest,
};
use crate::sse::openai_sse_to_stream;
use crate::usage::UsageTracker;

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: u32,
    embedding_model: Option<String>,
    reasoning_effort: Option<String>,
    pub(crate) status_tx: Option<StatusTx>,
    usage: UsageTracker,
    generation_overrides: Option<GenerationOverrides>,
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
            .field("usage", &self.usage)
            .field("generation_overrides", &self.generation_overrides)
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
            usage: UsageTracker::default(),
            generation_overrides: self.generation_overrides.clone(),
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
            client: crate::http::llm_client(600),
            api_key,
            base_url,
            model,
            max_tokens,
            embedding_model,
            reasoning_effort,
            status_tx: None,
            usage: UsageTracker::default(),
            generation_overrides: None,
        }
    }

    #[must_use]
    pub fn with_generation_overrides(mut self, overrides: GenerationOverrides) -> Self {
        self.generation_overrides = Some(overrides);
        self
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

    /// Derive a filesystem-safe cache slug from the provider's base URL hostname.
    ///
    /// Only ASCII alphanumeric characters and underscores are kept to prevent
    /// path traversal via unusual base URLs.
    #[must_use]
    pub fn cache_slug(&self) -> String {
        let host = self
            .base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("openai")
            .split(':')
            .next()
            .unwrap_or("openai");
        let slug: String = host
            .chars()
            .map(|c| if c == '.' || c == '-' { '_' } else { c })
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if slug.is_empty() {
            "openai".to_string()
        } else {
            slug
        }
    }

    /// Fetch the list of available models from GET `{base_url}/models` and cache them.
    ///
    /// # Errors
    ///
    /// Returns an error if the API request fails.
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        let url = format!("{}/models", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::debug!(status = %status, body = %body, "OpenAI list_models_remote error body");
            return Err(LlmError::Other(format!(
                "OpenAI list models failed: {status}"
            )));
        }

        let page: serde_json::Value = resp.json().await?;
        let models: Vec<crate::model_cache::RemoteModelInfo> = page
            .get("data")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let id = item.get("id")?.as_str()?.to_string();
                        let created_at = item.get("created").and_then(serde_json::Value::as_i64);
                        Some(crate::model_cache::RemoteModelInfo {
                            display_name: id.clone(),
                            id,
                            context_window: None,
                            created_at,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let slug = self.cache_slug();
        let cache = crate::model_cache::ModelCache::for_slug(&slug);
        cache.save(&models)?;
        Ok(models)
    }

    fn store_cache_usage(&self, usage: &OpenAiUsage) {
        self.usage
            .record_usage(usage.prompt_tokens, usage.completion_tokens);
        let cached = usage
            .prompt_tokens_details
            .as_ref()
            .map_or(0, |d| d.cached_tokens);
        if cached > 0 {
            self.usage.record_cache(0, cached);
        }
        tracing::debug!(
            prompt_tokens = usage.prompt_tokens,
            cached_tokens = cached,
            completion_tokens = usage.completion_tokens,
            "OpenAI API usage"
        );
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

        let (temperature, top_p, frequency_penalty, presence_penalty) =
            if let Some(ref ov) = self.generation_overrides {
                (
                    ov.temperature,
                    ov.top_p,
                    ov.frequency_penalty,
                    ov.presence_penalty,
                )
            } else {
                (None, None, None, None)
            };

        let response = if has_image_parts(messages) {
            let vision_messages = convert_messages_vision(messages);
            let body = VisionChatRequest {
                model: &self.model,
                messages: vision_messages,
                completion_tokens: CompletionTokens::for_model(&self.model, self.max_tokens),
                stream: false,
                reasoning,
                temperature,
                top_p,
                frequency_penalty,
                presence_penalty,
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
                completion_tokens: CompletionTokens::for_model(&self.model, self.max_tokens),
                stream: false,
                reasoning,
                temperature,
                top_p,
                frequency_penalty,
                presence_penalty,
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

        let (temperature, top_p, frequency_penalty, presence_penalty) =
            if let Some(ref ov) = self.generation_overrides {
                (
                    ov.temperature,
                    ov.top_p,
                    ov.frequency_penalty,
                    ov.presence_penalty,
                )
            } else {
                (None, None, None, None)
            };

        let body = ChatRequest {
            model: &self.model,
            messages: &api_messages,
            completion_tokens: CompletionTokens::for_model(&self.model, self.max_tokens),
            stream: true,
            reasoning,
            temperature,
            top_p,
            frequency_penalty,
            presence_penalty,
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
        use crate::embed::truncate_for_embed;

        let model = self
            .embedding_model
            .as_deref()
            .ok_or(LlmError::EmbedUnsupported {
                provider: "openai".into(),
            })?;

        let text = truncate_for_embed(text);
        let body = EmbeddingRequest {
            input: &text,
            model,
        };

        let response = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let body_text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            tracing::error!("OpenAI embedding API error {status}: {body_text}");
            if status == reqwest::StatusCode::BAD_REQUEST {
                return Err(LlmError::InvalidInput {
                    provider: "openai".into(),
                    message: body_text,
                });
            }
            return Err(LlmError::Other(format!(
                "OpenAI embedding request failed (status {status})"
            )));
        }

        let resp: EmbeddingResponse = serde_json::from_str(&body_text)?;

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

    fn model_identifier(&self) -> &str {
        &self.model
    }

    fn list_models(&self) -> Vec<String> {
        vec![self.model.clone()]
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        self.usage.last_cache_usage()
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        self.usage.last_usage()
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        let reasoning = self
            .reasoning_effort
            .as_deref()
            .map(|effort| Reasoning { effort });
        let (temperature, top_p, frequency_penalty, presence_penalty) = self
            .generation_overrides
            .as_ref()
            .map(|ov| {
                (
                    ov.temperature,
                    ov.top_p,
                    ov.frequency_penalty,
                    ov.presence_penalty,
                )
            })
            .unwrap_or_default();

        if !tools.is_empty() {
            let api_messages = convert_messages_structured(messages);
            let api_tools: Vec<OpenAiTool<'_>> = tools
                .iter()
                .map(|t| OpenAiTool {
                    r#type: "function",
                    function: OpenAiFunction {
                        name: &t.name,
                        description: &t.description,
                        parameters: prepare_tool_params(&t.parameters),
                    },
                })
                .collect();
            let body = ToolChatRequest {
                model: &self.model,
                messages: &api_messages,
                completion_tokens: CompletionTokens::for_model(&self.model, self.max_tokens),
                tools: &api_tools,
                reasoning,
                temperature,
                top_p,
                frequency_penalty,
                presence_penalty,
            };
            return serde_json::to_value(&body)
                .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }));
        }

        if has_image_parts(messages) {
            let vision_messages = convert_messages_vision(messages);
            let body = VisionChatRequest {
                model: &self.model,
                messages: vision_messages,
                completion_tokens: CompletionTokens::for_model(&self.model, self.max_tokens),
                stream,
                reasoning,
                temperature,
                top_p,
                frequency_penalty,
                presence_penalty,
            };
            return serde_json::to_value(&body)
                .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }));
        }

        let api_messages = convert_messages(messages);
        let body = ChatRequest {
            model: &self.model,
            messages: &api_messages,
            completion_tokens: CompletionTokens::for_model(&self.model, self.max_tokens),
            stream,
            reasoning,
            temperature,
            top_p,
            frequency_penalty,
            presence_penalty,
        };
        serde_json::to_value(&body)
            .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }))
    }

    fn supports_structured_output(&self) -> bool {
        true
    }

    async fn chat_typed<T>(&self, messages: &[Message]) -> Result<T, LlmError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
        Self: Sized,
    {
        let (raw_schema, _) = crate::provider::cached_schema::<T>()?;
        let mut schema_value = raw_schema;
        inline_refs_openai(&mut schema_value, 8);
        normalize_for_openai_strict(&mut schema_value, 16);
        let type_name = crate::provider::short_type_name::<T>();

        let api_messages = convert_messages(messages);
        let body = TypedChatRequest {
            model: &self.model,
            messages: &api_messages,
            completion_tokens: CompletionTokens::for_model(&self.model, self.max_tokens),
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

        if let Some(ref usage) = resp.usage {
            self.store_cache_usage(usage);
        }

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
                    parameters: prepare_tool_params(&t.parameters),
                },
            })
            .collect();

        let (temperature, top_p, frequency_penalty, presence_penalty) = self
            .generation_overrides
            .as_ref()
            .map(|ov| {
                (
                    ov.temperature,
                    ov.top_p,
                    ov.frequency_penalty,
                    ov.presence_penalty,
                )
            })
            .unwrap_or_default();
        let body = ToolChatRequest {
            model: &self.model,
            messages: &api_messages,
            completion_tokens: CompletionTokens::for_model(&self.model, self.max_tokens),
            tools: &api_tools,
            reasoning,
            temperature,
            top_p,
            frequency_penalty,
            presence_penalty,
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

        self.decode_tool_chat_response(&text)
    }
}

impl OpenAiProvider {
    fn decode_tool_chat_response(&self, text: &str) -> Result<ChatResponse, LlmError> {
        let resp: ToolChatResponse = serde_json::from_str(text)?;

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
                thinking_blocks: vec![],
            });
        }

        // Inject truncation marker when finish_reason is "length" so the agent loop
        // can detect MaxTokens stop reason without touching ChatResponse structure.
        let content = if choice.finish_reason.as_deref() == Some("length") {
            let truncation_marker = crate::provider::MAX_TOKENS_TRUNCATION_MARKER;
            if choice.message.content.is_empty() {
                format!(
                    "[Response truncated: {truncation_marker}. Please reduce the request scope.]"
                )
            } else {
                format!(
                    "{}\n[Response truncated: {truncation_marker}.]",
                    choice.message.content
                )
            }
        } else {
            choice.message.content
        };
        Ok(ChatResponse::Text(content))
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
    #[serde(flatten)]
    completion_tokens: CompletionTokens,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
}

fn has_image_parts(messages: &[Message]) -> bool {
    messages
        .iter()
        .any(|m| m.parts.iter().any(|p| matches!(p, MessagePart::Image(_))))
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
            let has_images = msg.parts.iter().any(|p| matches!(p, MessagePart::Image(_)));
            if has_images {
                let mut parts = Vec::new();
                let text_str: String = msg
                    .parts
                    .iter()
                    .filter_map(MessagePart::as_plain_text)
                    .collect::<Vec<_>>()
                    .join("");
                if !text_str.is_empty() {
                    parts.push(OpenAiContentPart::Text { text: text_str });
                }
                for part in &msg.parts {
                    if let Some(img) = part.as_image() {
                        let b64 = STANDARD.encode(&img.data);
                        parts.push(OpenAiContentPart::ImageUrl {
                            image_url: ImageUrlDetail {
                                url: format!("data:{};base64,{b64}", img.mime_type),
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
    #[serde(flatten)]
    completion_tokens: CompletionTokens,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ToolChatRequest<'a> {
    model: &'a str,
    messages: &'a [StructuredApiMessage],
    #[serde(flatten)]
    completion_tokens: CompletionTokens,
    tools: &'a [OpenAiTool<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f64>,
}

#[derive(Serialize)]
struct StructuredApiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
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
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ToolChatMessage {
    #[serde(default, deserialize_with = "deserialize_null_string_as_default")]
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

fn deserialize_null_string_as_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
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
                    .filter_map(|p| p.as_plain_text())
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
                                arguments: serde_json::to_string(input)
                                    .unwrap_or_else(|_| "{}".to_owned()),
                            },
                        }),
                        _ => None,
                    })
                    .collect();

                result.push(StructuredApiMessage {
                    role: "assistant".to_owned(),
                    content: if text_content.is_empty() {
                        None
                    } else {
                        Some(text_content)
                    },
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
                                content: Some(content.clone()),
                                tool_calls: None,
                                tool_call_id: Some(tool_use_id.clone()),
                            });
                        }
                        other => {
                            if let Some(text) = other.as_plain_text().filter(|t| !t.is_empty()) {
                                result.push(StructuredApiMessage {
                                    role: "user".to_owned(),
                                    content: Some(text.to_owned()),
                                    tool_calls: None,
                                    tool_call_id: None,
                                });
                            }
                        }
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
                content: Some(msg.to_llm_content().to_owned()),
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
    #[serde(flatten)]
    completion_tokens: CompletionTokens,
    response_format: ResponseFormat<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum CompletionTokens {
    MaxTokens { max_tokens: u32 },
    MaxCompletionTokens { max_completion_tokens: u32 },
}

impl CompletionTokens {
    fn for_model(model: &str, max_tokens: u32) -> Self {
        if model.starts_with("gpt-5") {
            Self::MaxCompletionTokens {
                max_completion_tokens: max_tokens,
            }
        } else {
            Self::MaxTokens { max_tokens }
        }
    }
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

/// Inline all `$ref` references from `$defs` into the schema tree.
fn inline_refs_openai(schema: &mut serde_json::Value, depth: u8) {
    if depth == 0 {
        return;
    }
    let defs = if let Some(obj) = schema.as_object() {
        obj.get("$defs")
            .or_else(|| obj.get("definitions"))
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::default()))
    } else {
        serde_json::Value::Object(serde_json::Map::default())
    };
    inline_refs_openai_inner(schema, &defs, depth);
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("$defs");
        obj.remove("definitions");
    }
}

fn inline_refs_openai_inner(schema: &mut serde_json::Value, defs: &serde_json::Value, depth: u8) {
    if depth == 0 {
        return;
    }
    if let Some(obj) = schema.as_object()
        && let Some(ref_val) = obj.get("$ref").and_then(|v| v.as_str())
    {
        let name = ref_val
            .trim_start_matches("#/$defs/")
            .trim_start_matches("#/definitions/");
        if let Some(resolved) = defs.get(name) {
            let mut resolved = resolved.clone();
            inline_refs_openai_inner(&mut resolved, defs, depth - 1);
            *schema = resolved;
            return;
        }
        *schema = serde_json::json!({"type": "object"});
        return;
    }
    if let Some(obj) = schema.as_object_mut() {
        for v in obj.values_mut() {
            inline_refs_openai_inner(v, defs, depth - 1);
        }
    } else if let Some(arr) = schema.as_array_mut() {
        for v in arr.iter_mut() {
            inline_refs_openai_inner(v, defs, depth - 1);
        }
    }
}

/// Returns `true` when the schema represents an object with no parameters.
///
/// Matches `{"type": "object"}` with absent or empty `properties`.
fn is_empty_params_schema(schema: &serde_json::Value) -> bool {
    schema.get("type").and_then(|t| t.as_str()) == Some("object")
        && schema
            .get("properties")
            .and_then(|p| p.as_object())
            .is_none_or(serde_json::Map::is_empty)
}

/// Prepare tool parameters schema for the `OpenAI` API.
///
/// Returns `None` for empty-parameter tools so the `parameters` field is
/// omitted entirely, avoiding strict-mode 400 errors.  For non-empty schemas,
/// inlines `$ref` definitions and normalizes for strict mode.
fn prepare_tool_params(params: &serde_json::Value) -> Option<serde_json::Value> {
    if is_empty_params_schema(params) {
        return None;
    }
    let mut schema = params.clone();
    inline_refs_openai(&mut schema, 8);
    normalize_for_openai_strict(&mut schema, 16);
    Some(schema)
}

struct OpenAiStrictVisitor;

impl crate::schema::SchemaVisitor for OpenAiStrictVisitor {
    fn visit(&mut self, schema: &mut serde_json::Value) -> bool {
        let Some(obj) = schema.as_object_mut() else {
            return false;
        };
        let remove_keys: &[&str] = &["$schema", "title", "format", "default", "examples", "$id"];
        for key in remove_keys {
            obj.remove(*key);
        }
        let is_object = obj.get("type").and_then(|t| t.as_str()) == Some("object");
        if is_object {
            obj.insert(
                "additionalProperties".to_owned(),
                serde_json::Value::Bool(false),
            );
            let prop_keys: Vec<String> = obj
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            if !prop_keys.is_empty() {
                obj.insert(
                    "required".to_owned(),
                    serde_json::Value::Array(
                        prop_keys
                            .into_iter()
                            .map(serde_json::Value::String)
                            .collect(),
                    ),
                );
            }
        }
        true
    }
}

/// Normalize a JSON Schema for `OpenAI` structured output strict mode.
///
/// Requirements:
/// - `additionalProperties: false` on every object
/// - All properties listed in `required`
/// - No `$schema`, `title`, or other non-strict keys at top level
fn normalize_for_openai_strict(schema: &mut serde_json::Value, depth: u8) {
    crate::schema::walk_schema(schema, &mut OpenAiStrictVisitor, depth);
}

#[cfg(test)]
mod tests;
