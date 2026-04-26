// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Object-safe adapter for [`LlmProvider`].
//!
//! [`LlmProviderDyn`] mirrors every method of [`LlmProvider`] but returns
//! [`BoxFuture`] instead of `impl Future + Send`. A blanket implementation
//! over any `T: LlmProvider + Send + Sync + 'static` wires the two traits
//! together automatically.
//!
//! ## Usage
//!
//! Use [`LlmProvider`] as the *implementation* surface (concrete types, monomorphic
//! call sites). Use `Arc<dyn LlmProviderDyn>` as the *storage* type wherever runtime
//! polymorphism is required (router, cascade, dependency injection).
//!
//! Implementors never need to implement [`LlmProviderDyn`] directly — the blanket impl
//! handles it. Implement [`LlmProvider`] instead.
//!
//! ## Generic methods
//!
//! `LlmProvider::chat_typed<T: DeserializeOwned>` cannot be part of a dyn-safe trait
//! because it carries a generic type parameter. Use the free function
//! [`chat_typed_dyn`] instead when working with `dyn LlmProviderDyn`.
//!
//! ## Examples
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use zeph_llm::provider::{LlmProvider, Message, Role};
//! use zeph_llm::provider_dyn::LlmProviderDyn;
//! use zeph_llm::ollama::OllamaProvider;
//!
//! # async fn example() -> Result<(), zeph_llm::LlmError> {
//! let provider = OllamaProvider::new(
//!     "http://localhost:11434",
//!     "llama3.2".into(),
//!     "nomic-embed-text".into(),
//! );
//!
//! // Erase the concrete type for storage in a router or DI container.
//! let dyn_provider: Arc<dyn LlmProviderDyn> = Arc::new(provider);
//!
//! let messages = vec![Message::from_legacy(Role::User, "Hello!")];
//! let response = dyn_provider.chat(&messages).await?;
//! println!("{response}");
//! # Ok(())
//! # }
//! ```

use futures::future::BoxFuture;
use serde::de::DeserializeOwned;

use crate::error::LlmError;
use crate::provider::{
    ChatExtras, ChatResponse, ChatStream, LlmProvider, Message, Role, ToolDefinition,
    cached_schema, short_type_name,
};

mod private {
    pub trait Sealed {}
    impl<T: super::LlmProvider> Sealed for T {}
}

/// Object-safe shadow of [`LlmProvider`].
///
/// Sealed — only the blanket `impl<T: LlmProvider + Send + Sync + 'static>` exists.
/// External crates cannot implement this trait directly; implement [`LlmProvider`] instead
/// and the blanket impl wires everything up automatically.
///
/// All async methods return [`BoxFuture`] rather than `impl Future + Send`, making this
/// trait dyn-compatible and usable behind `Arc<dyn LlmProviderDyn>`.
pub trait LlmProviderDyn: private::Sealed + std::fmt::Debug + Send + Sync {
    /// Report the model's context window size in tokens. `None` if unknown.
    fn context_window(&self) -> Option<usize>;

    /// Send messages to the LLM and return the assistant response.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to communicate or the response is invalid.
    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, LlmError>>;

    /// Send messages and return a stream of response chunks.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to communicate or the response is invalid.
    fn chat_stream<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<ChatStream, LlmError>>;

    /// Whether this provider supports native streaming.
    fn supports_streaming(&self) -> bool;

    /// Generate an embedding vector from text.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider does not support embeddings or the request fails.
    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, LlmError>>;

    /// Embed multiple texts in a single API call.
    ///
    /// # Errors
    ///
    /// Returns an error if any embedding fails.
    fn embed_batch<'a>(
        &'a self,
        texts: &'a [&'a str],
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>, LlmError>>;

    /// Whether this provider supports embedding generation.
    fn supports_embeddings(&self) -> bool;

    /// Provider name for logging and identification.
    fn name(&self) -> &str;

    /// Model identifier string (e.g. `gpt-4o-mini`, `claude-sonnet-4-6`).
    fn model_identifier(&self) -> &str;

    /// Whether this provider supports image input (vision).
    fn supports_vision(&self) -> bool;

    /// Whether this provider supports native `tool_use` / function calling.
    fn supports_tool_use(&self) -> bool;

    /// Send messages with tool definitions, returning a structured response.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider fails to communicate or the response is invalid.
    fn chat_with_tools<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
    ) -> BoxFuture<'a, Result<ChatResponse, LlmError>>;

    /// Return the cache usage from the last API call, if available.
    /// Returns `(cache_creation_tokens, cache_read_tokens)`.
    fn last_cache_usage(&self) -> Option<(u64, u64)>;

    /// Return token counts from the last API call, if available.
    /// Returns `(input_tokens, output_tokens)`.
    fn last_usage(&self) -> Option<(u64, u64)>;

    /// Return the compaction summary from the most recent API call, if available.
    fn take_compaction_summary(&self) -> Option<String>;

    /// Send messages and return the assistant response together with per-call extras.
    ///
    /// # Errors
    ///
    /// Same as [`chat`](Self::chat).
    fn chat_with_extras<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<(String, ChatExtras), LlmError>>;

    /// Return the request payload that will be sent to the provider, for debug dumps.
    #[must_use]
    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value;

    /// Return the list of model identifiers this provider can serve.
    fn list_models(&self) -> Vec<String>;

    /// Whether this provider supports native structured output.
    fn supports_structured_output(&self) -> bool;
}

impl<T: LlmProvider + std::fmt::Debug + Send + Sync + 'static> LlmProviderDyn for T {
    fn context_window(&self) -> Option<usize> {
        LlmProvider::context_window(self)
    }

    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, LlmError>> {
        Box::pin(LlmProvider::chat(self, messages))
    }

    fn chat_stream<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<ChatStream, LlmError>> {
        Box::pin(LlmProvider::chat_stream(self, messages))
    }

    fn supports_streaming(&self) -> bool {
        LlmProvider::supports_streaming(self)
    }

    fn embed<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, LlmError>> {
        Box::pin(LlmProvider::embed(self, text))
    }

    fn embed_batch<'a>(
        &'a self,
        texts: &'a [&'a str],
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>, LlmError>> {
        Box::pin(LlmProvider::embed_batch(self, texts))
    }

    fn supports_embeddings(&self) -> bool {
        LlmProvider::supports_embeddings(self)
    }

    fn name(&self) -> &str {
        LlmProvider::name(self)
    }

    fn model_identifier(&self) -> &str {
        LlmProvider::model_identifier(self)
    }

    fn supports_vision(&self) -> bool {
        LlmProvider::supports_vision(self)
    }

    fn supports_tool_use(&self) -> bool {
        LlmProvider::supports_tool_use(self)
    }

    fn chat_with_tools<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
    ) -> BoxFuture<'a, Result<ChatResponse, LlmError>> {
        Box::pin(LlmProvider::chat_with_tools(self, messages, tools))
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        LlmProvider::last_cache_usage(self)
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        LlmProvider::last_usage(self)
    }

    fn take_compaction_summary(&self) -> Option<String> {
        LlmProvider::take_compaction_summary(self)
    }

    fn chat_with_extras<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<(String, ChatExtras), LlmError>> {
        Box::pin(LlmProvider::chat_with_extras(self, messages))
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        LlmProvider::debug_request_json(self, messages, tools, stream)
    }

    fn list_models(&self) -> Vec<String> {
        LlmProvider::list_models(self)
    }

    fn supports_structured_output(&self) -> bool {
        LlmProvider::supports_structured_output(self)
    }
}

/// Send messages and parse the response into a typed value `T`.
///
/// This is the dyn-compatible equivalent of [`LlmProvider::chat_typed`]. Because
/// `chat_typed` carries a generic type parameter, it cannot be part of a dyn-safe
/// trait. Use this free function when working with `&dyn LlmProviderDyn` or
/// `Arc<dyn LlmProviderDyn>`.
///
/// The default implementation injects the JSON schema into the system prompt and
/// retries once on parse failure, matching the behaviour of the trait method.
///
/// # Errors
///
/// Returns [`LlmError::StructuredParse`] when the response cannot be parsed as `T`
/// after one retry. Propagates any underlying [`LlmError`] from the provider.
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use schemars::JsonSchema;
/// use serde::Deserialize;
/// use zeph_llm::provider::{Message, Role};
/// use zeph_llm::provider_dyn::{LlmProviderDyn, chat_typed_dyn};
/// use zeph_llm::ollama::OllamaProvider;
///
/// #[derive(Debug, Deserialize, JsonSchema)]
/// struct Answer {
///     value: String,
/// }
///
/// # async fn example() -> Result<(), zeph_llm::LlmError> {
/// let provider = OllamaProvider::new(
///     "http://localhost:11434",
///     "llama3.2".into(),
///     "nomic-embed-text".into(),
/// );
/// let dyn_provider: Arc<dyn LlmProviderDyn> = Arc::new(provider);
/// let messages = vec![Message::from_legacy(Role::User, "What is 2+2?")];
/// let answer: Answer = chat_typed_dyn(&*dyn_provider, &messages).await?;
/// println!("{}", answer.value);
/// # Ok(())
/// # }
/// ```
pub async fn chat_typed_dyn<T, P>(provider: &P, messages: &[Message]) -> Result<T, LlmError>
where
    T: DeserializeOwned + schemars::JsonSchema + 'static,
    P: ?Sized + LlmProviderDyn,
{
    let (_, schema_json) = cached_schema::<T>()?;
    let type_name = short_type_name::<T>();

    let instruction = format!(
        "Respond with a valid JSON object matching this schema. \
         Output ONLY the JSON, no markdown fences or extra text.\n\n\
         Type: {type_name}\nSchema:\n```json\n{schema_json}\n```"
    );

    let mut augmented = messages.to_vec();
    augmented.insert(0, Message::from_legacy(Role::System, instruction));

    let raw = provider.chat(&augmented).await?;
    let cleaned = strip_json_fences(&raw);
    match serde_json::from_str::<T>(cleaned) {
        Ok(val) => Ok(val),
        Err(first_err) => {
            augmented.push(Message::from_legacy(Role::Assistant, &raw));
            augmented.push(Message::from_legacy(
                Role::User,
                format!(
                    "Your response was not valid JSON. Error: {first_err}. \
                     Please output ONLY valid JSON matching the schema."
                ),
            ));
            let retry_raw = provider.chat(&augmented).await?;
            let retry_cleaned = strip_json_fences(&retry_raw);
            serde_json::from_str::<T>(retry_cleaned)
                .map_err(|e| LlmError::StructuredParse(format!("parse failed after retry: {e}")))
        }
    }
}

/// Strip markdown code fences from LLM output.
fn strip_json_fences(s: &str) -> &str {
    s.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::provider::{ChatStream, StreamChunk};

    #[derive(Debug)]
    struct StubProvider {
        response: String,
    }

    impl LlmProvider for StubProvider {
        async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
            Ok(self.response.clone())
        }

        async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
            let response = LlmProvider::chat(self, messages).await?;
            Ok(Box::pin(tokio_stream::once(Ok(StreamChunk::Content(
                response,
            )))))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
            Ok(vec![0.1, 0.2, 0.3])
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "stub"
        }
    }

    #[tokio::test]
    async fn dyn_chat_works() {
        let provider: Arc<dyn LlmProviderDyn> = Arc::new(StubProvider {
            response: "hello".into(),
        });
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let result = provider.chat(&msgs).await.unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn dyn_embed_works() {
        let provider: Arc<dyn LlmProviderDyn> = Arc::new(StubProvider {
            response: String::new(),
        });
        let result = provider.embed("hello").await.unwrap();
        assert_eq!(result, vec![0.1_f32, 0.2, 0.3]);
    }

    #[test]
    fn dyn_sync_methods_forward_correctly() {
        let provider: Arc<dyn LlmProviderDyn> = Arc::new(StubProvider {
            response: String::new(),
        });
        assert_eq!(provider.name(), "stub");
        assert!(!provider.supports_streaming());
        assert!(!provider.supports_embeddings());
        assert!(provider.context_window().is_none());
        assert!(provider.last_cache_usage().is_none());
        assert!(provider.last_usage().is_none());
    }

    #[derive(Debug, serde::Deserialize, schemars::JsonSchema, PartialEq)]
    struct TestOutput {
        value: String,
    }

    #[tokio::test]
    async fn chat_typed_dyn_happy_path() {
        let provider: Arc<dyn LlmProviderDyn> = Arc::new(StubProvider {
            response: r#"{"value": "hello"}"#.into(),
        });
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let result: TestOutput = chat_typed_dyn(&*provider, &msgs).await.unwrap();
        assert_eq!(
            result,
            TestOutput {
                value: "hello".into()
            }
        );
    }

    #[tokio::test]
    async fn chat_typed_dyn_strips_fences() {
        let provider: Arc<dyn LlmProviderDyn> = Arc::new(StubProvider {
            response: "```json\n{\"value\": \"fenced\"}\n```".into(),
        });
        let msgs = vec![Message::from_legacy(Role::User, "test")];
        let result: TestOutput = chat_typed_dyn(&*provider, &msgs).await.unwrap();
        assert_eq!(
            result,
            TestOutput {
                value: "fenced".into()
            }
        );
    }
}
