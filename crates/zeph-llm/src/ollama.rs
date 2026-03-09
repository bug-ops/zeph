// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ollama_rs::Ollama;

use crate::error::LlmError;
use base64::{Engine, engine::general_purpose::STANDARD};
use ollama_rs::generation::chat::ChatMessage;
use ollama_rs::generation::chat::request::ChatMessageRequest;
use ollama_rs::generation::embeddings::request::{EmbeddingsInput, GenerateEmbeddingsRequest};
use ollama_rs::generation::images::Image as OllamaImage;
use ollama_rs::generation::tools::{ToolFunctionInfo, ToolInfo, ToolType};
use ollama_rs::models::ModelOptions;
use tokio_stream::StreamExt;

use crate::provider::{
    ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, MessagePart, Role,
    ToolDefinition, ToolUseRequest,
};

#[derive(Debug)]
pub struct ModelInfo {
    pub context_length: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct OllamaProvider {
    client: Ollama,
    model: String,
    embedding_model: String,
    context_window_size: Option<usize>,
    vision_model: Option<String>,
    tool_use: bool,
    generation_overrides: Option<GenerationOverrides>,
}

#[allow(clippy::cast_possible_truncation)]
fn apply_generation_overrides(
    request: ChatMessageRequest,
    overrides: &GenerationOverrides,
) -> ChatMessageRequest {
    let mut opts = ModelOptions::default();
    if let Some(t) = overrides.temperature {
        tracing::debug!(temperature = t, "applying generation override: temperature");
        opts = opts.temperature(t as f32);
    }
    if let Some(tp) = overrides.top_p {
        tracing::debug!(top_p = tp, "applying generation override: top_p");
        opts = opts.top_p(tp as f32);
    }
    if let Some(tk) = overrides.top_k {
        tracing::debug!(top_k = tk, "applying generation override: top_k");
        opts = opts.top_k(tk as u32);
    }
    // frequency_penalty and presence_penalty are not supported by ollama-rs ModelOptions.
    request.options(opts)
}

impl OllamaProvider {
    #[must_use]
    pub fn new(base_url: &str, model: String, embedding_model: String) -> Self {
        let (host, port) = parse_host_port(base_url);
        Self {
            client: Ollama::new(host, port),
            model,
            embedding_model,
            context_window_size: None,
            vision_model: None,
            tool_use: false,
            generation_overrides: None,
        }
    }

    #[must_use]
    pub fn with_generation_overrides(mut self, overrides: GenerationOverrides) -> Self {
        self.generation_overrides = Some(overrides);
        self
    }

    #[must_use]
    pub fn with_vision_model(mut self, model: String) -> Self {
        self.vision_model = Some(model);
        self
    }

    #[must_use]
    pub fn with_tool_use(mut self, enabled: bool) -> Self {
        self.tool_use = enabled;
        self
    }

    /// Set context window size (typically from /api/show response).
    pub fn set_context_window(&mut self, size: usize) {
        self.context_window_size = Some(size);
    }

    /// Query Ollama /api/show for model metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the request fails.
    pub async fn fetch_model_info(&self) -> Result<ModelInfo, LlmError> {
        let info = self
            .client
            .show_model_info(self.model.clone())
            .await
            .map_err(|e| LlmError::Other(format!("failed to fetch model info from Ollama: {e}")))?;

        // Try model_info map first (newer ollama versions)
        let ctx = info
            .model_info
            .iter()
            .find_map(|(k, v)| {
                if k.ends_with(".context_length") {
                    v.as_u64().and_then(|n| usize::try_from(n).ok())
                } else {
                    None
                }
            })
            .or_else(|| parse_num_ctx(&info.parameters));

        Ok(ModelInfo {
            context_length: ctx,
        })
    }

    /// Check if Ollama is reachable.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection to Ollama fails.
    pub async fn health_check(&self) -> Result<(), LlmError> {
        self.client.list_local_models().await.map_err(|e| {
            LlmError::Other(format!("failed to connect to Ollama — is it running? {e}"))
        })?;
        Ok(())
    }

    /// Fetch the list of locally available models from Ollama and cache them on disk.
    ///
    /// On error the existing cache is preserved and the error is returned.
    ///
    /// # Errors
    ///
    /// Returns an error if the Ollama API request fails.
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        let local_models = self
            .client
            .list_local_models()
            .await
            .map_err(|e| LlmError::Other(format!("ollama list_local_models: {e}")))?;

        let models: Vec<crate::model_cache::RemoteModelInfo> = local_models
            .into_iter()
            .map(|m| crate::model_cache::RemoteModelInfo {
                id: m.name.clone(),
                display_name: m.name,
                context_window: None,
                created_at: None,
            })
            .collect();

        let cache = crate::model_cache::ModelCache::for_slug("ollama");
        cache.save(&models)?;
        Ok(models)
    }

    /// Send a minimal chat request to force Ollama to load the model into memory.
    ///
    /// # Errors
    ///
    /// Returns an error if the warmup request fails.
    pub async fn warmup(&self) -> Result<(), LlmError> {
        let request =
            ChatMessageRequest::new(self.model.clone(), vec![ChatMessage::user("hi".to_owned())]);
        self.client
            .send_chat_messages(request)
            .await
            .map_err(|e| LlmError::Other(format!("Ollama warmup failed: {e}")))?;
        Ok(())
    }
}

impl LlmProvider for OllamaProvider {
    fn context_window(&self) -> Option<usize> {
        self.context_window_size
    }

    fn supports_vision(&self) -> bool {
        true
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        let has_images = messages
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, MessagePart::Image(_))));
        let model = if has_images {
            self.vision_model.as_deref().unwrap_or(&self.model)
        } else {
            &self.model
        };
        let ollama_messages: Vec<ChatMessage> = messages.iter().map(convert_message).collect();

        let mut request = ChatMessageRequest::new(model.to_owned(), ollama_messages);
        if let Some(ref ov) = self.generation_overrides {
            request = apply_generation_overrides(request, ov);
        }

        let response = self
            .client
            .send_chat_messages(request)
            .await
            .map_err(|e| LlmError::Other(format!("Ollama chat request failed: {e}")))?;

        Ok(response.message.content)
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let has_images = messages
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, MessagePart::Image(_))));
        let model = if has_images {
            self.vision_model.as_deref().unwrap_or(&self.model)
        } else {
            &self.model
        };
        let ollama_messages: Vec<ChatMessage> = messages.iter().map(convert_message).collect();
        let mut request = ChatMessageRequest::new(model.to_owned(), ollama_messages);
        if let Some(ref ov) = self.generation_overrides {
            request = apply_generation_overrides(request, ov);
        }

        let stream = self
            .client
            .send_chat_messages_stream(request)
            .await
            .map_err(|e| LlmError::Other(format!("Ollama streaming request failed: {e}")))?;

        let mapped = stream.map(|item| match item {
            Ok(response) => Ok(crate::provider::StreamChunk::Content(
                response.message.content,
            )),
            Err(()) => Err(LlmError::Other("Ollama stream chunk failed".into())),
        });

        Ok(Box::pin(mapped))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_tool_use(&self) -> bool {
        self.tool_use
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let ollama_tools: Vec<ToolInfo> = tools
            .iter()
            .map(|t| ToolInfo {
                tool_type: ToolType::Function,
                function: ToolFunctionInfo {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: serde_json::from_value(t.parameters.clone()).unwrap_or_default(),
                },
            })
            .collect();

        let ollama_messages: Vec<ChatMessage> =
            messages.iter().map(convert_message_structured).collect();

        let mut request =
            ChatMessageRequest::new(self.model.clone(), ollama_messages).tools(ollama_tools);
        if let Some(ref ov) = self.generation_overrides {
            request = apply_generation_overrides(request, ov);
        }

        let response =
            self.client.send_chat_messages(request).await.map_err(|e| {
                LlmError::Other(format!("Ollama chat_with_tools request failed: {e}"))
            })?;

        if response.message.tool_calls.is_empty() {
            return Ok(ChatResponse::Text(response.message.content));
        }

        let tool_calls: Vec<ToolUseRequest> = response
            .message
            .tool_calls
            .into_iter()
            .enumerate()
            .map(|(i, tc)| ToolUseRequest {
                id: format!("call_{i}"),
                name: tc.function.name,
                input: tc.function.arguments,
            })
            .collect();

        let text = if response.message.content.is_empty() {
            None
        } else {
            Some(response.message.content)
        };

        Ok(ChatResponse::ToolUse {
            text,
            tool_calls,
            thinking_blocks: vec![],
        })
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let request = GenerateEmbeddingsRequest::new(
            self.embedding_model.clone(),
            EmbeddingsInput::from(text),
        );

        let response = self
            .client
            .generate_embeddings(request)
            .await
            .map_err(|e| LlmError::Other(format!("Ollama embedding request failed: {e}")))?;

        response
            .embeddings
            .into_iter()
            .next()
            .ok_or(LlmError::EmptyResponse {
                provider: "ollama".into(),
            })
    }

    fn supports_embeddings(&self) -> bool {
        true
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "ollama"
    }
}

/// Convert a message for tool-aware requests. Handles `ToolUse` and `ToolResult` parts.
fn convert_message_structured(msg: &Message) -> ChatMessage {
    // If the message contains ToolResult parts, emit them as role:tool messages.
    // ollama-rs represents tool results as a single ChatMessage with role Tool.
    // We concatenate all tool result contents (Ollama expects one message per turn).
    let tool_results: Vec<&MessagePart> = msg
        .parts
        .iter()
        .filter(|p| matches!(p, MessagePart::ToolResult { .. }))
        .collect();
    if !tool_results.is_empty() {
        let content = tool_results
            .iter()
            .filter_map(|p| {
                if let MessagePart::ToolResult { content, .. } = p {
                    Some(content.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        return ChatMessage::tool(content);
    }
    convert_message(msg)
}

fn convert_message(msg: &Message) -> ChatMessage {
    let images: Vec<OllamaImage> = msg
        .parts
        .iter()
        .filter_map(|p| match p {
            MessagePart::Image(img) => Some(OllamaImage::from_base64(STANDARD.encode(&img.data))),
            _ => None,
        })
        .collect();

    let text = msg.to_llm_content().to_string();

    match msg.role {
        Role::System => ChatMessage::system(text),
        Role::Assistant => ChatMessage::assistant(text),
        Role::User => {
            if images.is_empty() {
                ChatMessage::user(text)
            } else {
                ChatMessage::user(text).with_images(images)
            }
        }
    }
}

fn parse_num_ctx(parameters: &str) -> Option<usize> {
    for line in parameters.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("num_ctx")
            && let Ok(val) = rest.trim().parse::<usize>()
        {
            return Some(val);
        }
    }
    None
}

fn parse_host_port(url: &str) -> (String, u16) {
    let url = url.trim_end_matches('/');
    if let Some(colon_pos) = url.rfind(':') {
        let port_str = &url[colon_pos + 1..];
        if let Ok(port) = port_str.parse::<u16>() {
            let host = url[..colon_pos].to_string();
            return (host, port);
        }
    }
    (url.to_string(), 11434)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ImageData;
    use crate::provider::MessageMetadata;

    #[test]
    fn context_window_none_by_default() {
        let provider = OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into());
        assert!(provider.context_window().is_none());
    }

    #[test]
    fn context_window_after_set() {
        let mut provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into());
        provider.set_context_window(32768);
        assert_eq!(provider.context_window(), Some(32768));
    }

    #[test]
    fn parse_num_ctx_from_parameters() {
        assert_eq!(parse_num_ctx("num_ctx 4096"), Some(4096));
        assert_eq!(
            parse_num_ctx("num_ctx                    32768"),
            Some(32768)
        );
        assert_eq!(parse_num_ctx("other_param 123\nnum_ctx 8192"), Some(8192));
        assert!(parse_num_ctx("no match here").is_none());
        assert!(parse_num_ctx("").is_none());
    }

    #[test]
    fn parse_host_port_with_port() {
        let (host, port) = parse_host_port("http://localhost:11434");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_without_port() {
        let (host, port) = parse_host_port("http://localhost");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn convert_message_roles() {
        let msg = Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "hello");
    }

    #[test]
    fn supports_streaming_returns_true() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into());
        assert!(provider.supports_streaming());
    }

    #[test]
    fn supports_embeddings_returns_true() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into());
        assert!(provider.supports_embeddings());
    }

    #[test]
    fn name_returns_ollama() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into());
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn new_stores_model_and_embedding_model() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "qwen3:8b".into(),
            "nomic-embed".into(),
        );
        assert_eq!(provider.model, "qwen3:8b");
        assert_eq!(provider.embedding_model, "nomic-embed");
    }

    #[test]
    fn clone_preserves_fields() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "llama3".into(),
            "embed-model".into(),
        );
        let cloned = provider.clone();
        assert_eq!(cloned.model, provider.model);
        assert_eq!(cloned.embedding_model, provider.embedding_model);
    }

    #[test]
    fn debug_format() {
        let provider =
            OllamaProvider::new("http://localhost:11434", "test".into(), "test-embed".into());
        let debug = format!("{provider:?}");
        assert!(debug.contains("OllamaProvider"));
        assert!(debug.contains("test"));
    }

    #[test]
    fn parse_host_port_custom_port() {
        let (host, port) = parse_host_port("http://example.com:8080");
        assert_eq!(host, "http://example.com");
        assert_eq!(port, 8080);
    }

    #[test]
    fn parse_host_port_trailing_slash() {
        let (host, port) = parse_host_port("http://localhost:11434/");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_no_scheme() {
        let (host, port) = parse_host_port("localhost:9999");
        assert_eq!(host, "localhost");
        assert_eq!(port, 9999);
    }

    #[test]
    fn parse_host_port_invalid_port_falls_back() {
        let (host, port) = parse_host_port("http://localhost:notaport");
        assert_eq!(host, "http://localhost:notaport");
        assert_eq!(port, 11434);
    }

    #[test]
    fn convert_message_system_role() {
        let msg = Message {
            role: Role::System,
            content: "system instruction".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "system instruction");
    }

    #[test]
    fn convert_message_assistant_role() {
        let msg = Message {
            role: Role::Assistant,
            content: "reply text".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "reply text");
    }

    #[test]
    fn parse_host_port_empty_string() {
        let (host, port) = parse_host_port("");
        assert_eq!(host, "");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_only_scheme() {
        let (host, port) = parse_host_port("http://localhost");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_port_zero() {
        let (host, port) = parse_host_port("http://localhost:0");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 0);
    }

    #[test]
    fn parse_host_port_max_port() {
        let (host, port) = parse_host_port("http://localhost:65535");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 65535);
    }

    #[test]
    fn parse_host_port_port_overflow_falls_back() {
        let (host, port) = parse_host_port("http://localhost:99999");
        assert_eq!(host, "http://localhost:99999");
        assert_eq!(port, 11434);
    }

    #[test]
    fn parse_host_port_ipv4() {
        let (host, port) = parse_host_port("http://192.168.1.1:8080");
        assert_eq!(host, "http://192.168.1.1");
        assert_eq!(port, 8080);
    }

    #[test]
    fn parse_host_port_multiple_trailing_slashes() {
        let (host, port) = parse_host_port("http://localhost:11434///");
        assert_eq!(host, "http://localhost");
        assert_eq!(port, 11434);
    }

    #[test]
    fn convert_message_preserves_content() {
        let msg = Message {
            role: Role::User,
            content: "multi\nline\ncontent".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let cm = convert_message(&msg);
        assert_eq!(cm.content, "multi\nline\ncontent");
    }

    #[test]
    fn convert_message_empty_content() {
        let msg = Message {
            role: Role::User,
            content: String::new(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let cm = convert_message(&msg);
        assert!(cm.content.is_empty());
    }

    #[tokio::test]
    async fn chat_with_unreachable_endpoint_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn embed_with_unreachable_endpoint_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let result = provider.embed("test text").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_stream_with_unreachable_endpoint_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat_stream(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn warmup_with_unreachable_endpoint_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let result = provider.warmup().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("warmup failed"));
    }

    #[tokio::test]
    async fn health_check_unreachable_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into());
        let result = provider.health_check().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Ollama"));
    }

    #[test]
    fn new_with_different_urls() {
        let p1 = OllamaProvider::new("http://host1:1234", "m1".into(), "e1".into());
        let p2 = OllamaProvider::new("http://host2:5678", "m2".into(), "e2".into());
        assert_eq!(p1.model, "m1");
        assert_eq!(p2.model, "m2");
        assert_eq!(p1.embedding_model, "e1");
        assert_eq!(p2.embedding_model, "e2");
    }

    #[tokio::test]
    #[ignore = "requires running Ollama instance"]
    async fn integration_ollama_chat_stream() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "qwen3:8b".into(),
            "qwen3-embedding".into(),
        );

        let messages = vec![Message {
            role: Role::User,
            content: "Reply with exactly: pong".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut chunk_count = 0;

        let mut full_response = String::new();
        while let Some(result) = stream.next().await {
            if let crate::provider::StreamChunk::Content(text) = result.unwrap() {
                full_response.push_str(&text);
            }
            chunk_count += 1;
        }

        assert!(!full_response.is_empty());
        assert!(full_response.to_lowercase().contains("pong"));
        assert!(chunk_count >= 1);
    }

    #[tokio::test]
    #[ignore = "requires running Ollama instance"]
    async fn integration_ollama_stream_matches_chat() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "qwen3:8b".into(),
            "qwen3-embedding".into(),
        );

        let messages = vec![Message {
            role: Role::User,
            content: "What is 2+2? Reply with just the number.".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let chat_response = provider.chat(&messages).await.unwrap();

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut stream_response = String::new();
        while let Some(result) = stream.next().await {
            if let crate::provider::StreamChunk::Content(text) = result.unwrap() {
                stream_response.push_str(&text);
            }
        }

        assert!(chat_response.contains('4'));
        assert!(stream_response.contains('4'));
    }

    #[tokio::test]
    #[ignore = "requires running Ollama instance with qwen3-embedding model"]
    async fn integration_ollama_embed() {
        let provider = OllamaProvider::new(
            "http://localhost:11434",
            "qwen3:8b".into(),
            "qwen3-embedding".into(),
        );

        let embedding = provider.embed("hello world").await.unwrap();
        assert!(!embedding.is_empty());
        assert!(embedding.len() > 100);
        assert!(embedding.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn with_vision_model_sets_field() {
        let provider = OllamaProvider::new("http://localhost:11434", "main".into(), "embed".into())
            .with_vision_model("llava:13b".into());
        assert_eq!(provider.vision_model.as_deref(), Some("llava:13b"));
    }

    #[test]
    fn with_vision_model_builder_returns_self() {
        let provider = OllamaProvider::new("http://localhost:11434", "main".into(), "embed".into())
            .with_vision_model("llava:7b".into());
        assert_eq!(provider.model, "main");
        assert_eq!(provider.vision_model.as_deref(), Some("llava:7b"));
    }

    #[test]
    fn convert_message_text_only_has_no_images() {
        let msg = Message::from_legacy(Role::User, "hello");
        let chat_msg = convert_message(&msg);
        // No images attached — role should be User, content non-empty
        assert_eq!(
            chat_msg.role,
            ollama_rs::generation::chat::MessageRole::User
        );
        assert!(!chat_msg.content.is_empty());
    }

    #[test]
    fn convert_message_with_image_encodes_base64() {
        use base64::{Engine, engine::general_purpose::STANDARD};

        let data = vec![0xFFu8, 0xD8, 0xFF];
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::Text {
                    text: "describe".into(),
                },
                MessagePart::Image(Box::new(ImageData {
                    data: data.clone(),
                    mime_type: "image/jpeg".into(),
                })),
            ],
        );
        let chat_msg = convert_message(&msg);
        let images = chat_msg.images.unwrap_or_default();
        assert_eq!(images.len(), 1);
        // OllamaImage stores the base64 string internally — verify via Debug/format
        let img_debug = format!("{:?}", images[0]);
        assert!(img_debug.contains(&STANDARD.encode(&data)));
    }

    #[test]
    fn model_selection_uses_vision_model_when_images_present() {
        let provider = OllamaProvider::new("http://localhost:11434", "main".into(), "embed".into())
            .with_vision_model("llava:13b".into());

        let has_images = true;
        let selected = if has_images {
            provider.vision_model.as_deref().unwrap_or(&provider.model)
        } else {
            &provider.model
        };
        assert_eq!(selected, "llava:13b");

        let has_images = false;
        let selected = if has_images {
            provider.vision_model.as_deref().unwrap_or(&provider.model)
        } else {
            &provider.model
        };
        assert_eq!(selected, "main");
    }

    #[test]
    fn model_selection_falls_back_to_main_without_vision_model() {
        let provider = OllamaProvider::new("http://localhost:11434", "main".into(), "embed".into());
        let selected = provider.vision_model.as_deref().unwrap_or(&provider.model);
        assert_eq!(selected, "main");
    }

    #[test]
    fn supports_tool_use_default_false() {
        let provider = OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into());
        assert!(!provider.supports_tool_use());
    }

    #[test]
    fn supports_tool_use_enabled_via_builder() {
        let provider = OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into())
            .with_tool_use(true);
        assert!(provider.supports_tool_use());
    }

    #[test]
    fn with_tool_use_false_disables() {
        let provider = OllamaProvider::new("http://localhost:11434", "test".into(), "embed".into())
            .with_tool_use(true)
            .with_tool_use(false);
        assert!(!provider.supports_tool_use());
    }

    #[test]
    fn convert_message_structured_tool_result_emits_tool_role() {
        let msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "id1".into(),
                content: "file list".into(),
                is_error: false,
            }],
        );
        let chat_msg = convert_message_structured(&msg);
        assert_eq!(
            chat_msg.role,
            ollama_rs::generation::chat::MessageRole::Tool
        );
        assert_eq!(chat_msg.content, "file list");
    }

    #[test]
    fn convert_message_structured_multiple_tool_results_joined() {
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::ToolResult {
                    tool_use_id: "id1".into(),
                    content: "result_a".into(),
                    is_error: false,
                },
                MessagePart::ToolResult {
                    tool_use_id: "id2".into(),
                    content: "result_b".into(),
                    is_error: false,
                },
            ],
        );
        let chat_msg = convert_message_structured(&msg);
        assert_eq!(
            chat_msg.role,
            ollama_rs::generation::chat::MessageRole::Tool
        );
        assert!(chat_msg.content.contains("result_a"));
        assert!(chat_msg.content.contains("result_b"));
    }

    #[test]
    fn convert_message_structured_no_tool_results_delegates_to_convert_message() {
        let msg = Message::from_legacy(Role::Assistant, "response");
        let chat_msg = convert_message_structured(&msg);
        assert_eq!(
            chat_msg.role,
            ollama_rs::generation::chat::MessageRole::Assistant
        );
        assert_eq!(chat_msg.content, "response");
    }

    #[test]
    fn with_generation_overrides_stores_overrides() {
        let provider = OllamaProvider::new("http://127.0.0.1:11434", "m".into(), "e".into());
        assert!(provider.generation_overrides.is_none());
        let overrides = GenerationOverrides {
            temperature: Some(0.5),
            top_p: Some(0.9),
            top_k: Some(40),
            frequency_penalty: None,
            presence_penalty: None,
        };
        let patched = provider.with_generation_overrides(overrides);
        let ov = patched
            .generation_overrides
            .as_ref()
            .expect("overrides set");
        assert_eq!(ov.temperature, Some(0.5));
        assert_eq!(ov.top_p, Some(0.9));
        assert_eq!(ov.top_k, Some(40));
    }

    #[tokio::test]
    async fn chat_with_tools_unreachable_endpoint_errors() {
        let provider =
            OllamaProvider::new("http://127.0.0.1:1", "test-model".into(), "embed".into())
                .with_tool_use(true);
        let messages = vec![Message::from_legacy(Role::User, "hello")];
        let tools = vec![ToolDefinition {
            name: "test_tool".into(),
            description: "A test tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let result = provider.chat_with_tools(&messages, &tools).await;
        assert!(result.is_err());
    }
}
