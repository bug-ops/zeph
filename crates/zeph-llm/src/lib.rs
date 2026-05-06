// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LLM provider abstraction and backend implementations for the Zeph agent.
//!
//! # Overview
//!
//! `zeph-llm` is the inference layer of the Zeph agent stack. It defines the
//! [`LlmProvider`] trait and supplies concrete backends for every supported
//! inference provider. All providers are composable via `AnyProvider` and the
//! [`router`] module, so callers never need to depend on a specific backend.
//!
//! # Core Abstraction
//!
//! [`LlmProvider`] is the central trait. Every backend implements:
//! - [`LlmProvider::chat`] — single-turn, blocking response
//! - [`LlmProvider::chat_stream`] — streaming response as a [`ChatStream`]
//! - [`LlmProvider::embed`] — embedding generation
//! - [`LlmProvider::chat_with_tools`] — structured tool-call protocol
//! - [`LlmProvider::chat_typed`] — schema-driven structured JSON extraction
//!
//! # Backends
//!
//! | Module | Backend | Feature flag |
//! |---|---|---|
//! | [`ollama`] | `Ollama` local models | always |
//! | [`claude`] | `Anthropic` Claude API | always |
//! | [`openai`] | `OpenAI` API | always |
//! | [`gemini`] | `Google` Gemini API | always |
//! | [`compatible`] | `OpenAI`-compatible endpoints | always |
//! | `candle_provider` | `HuggingFace` Candle local inference | `candle` |
//!
//! # Provider Routing
//!
//! The [`router`] module provides [`router::RouterProvider`], which wraps a list
//! of backends and selects among them using one of four strategies:
//!
//! - **EMA** — exponential moving average latency-aware ordering (default)
//! - **Thompson** — Bayesian Beta-distribution sampling
//! - **Cascade** — cheapest-first with automatic escalation on degenerate output
//! - **Bandit** — contextual `LinUCB` with online learning (PILOT)
//!
//! # Structured Extraction
//!
//! [`Extractor`] wraps any provider and exposes a typed `extract::<T>()` method
//! that injects a JSON schema into the prompt and parses the response. Use it for
//! entity extraction, classification, and any structured LLM output.
//!
//! # Error Handling
//!
//! All fallible operations return [`LlmError`]. Callers can inspect the error type
//! to distinguish retriable failures (rate limiting, transient HTTP errors) from
//! permanent failures (invalid input, context length exceeded).
//!
//! # Examples
//!
//! ```rust,no_run
//! use zeph_llm::provider::{LlmProvider, Message, Role};
//! use zeph_llm::ollama::OllamaProvider;
//!
//! # async fn example() -> Result<(), zeph_llm::LlmError> {
//! let provider = OllamaProvider::new("http://localhost:11434", "llama3.2".into(), "nomic-embed-text".into());
//! let messages = vec![Message::from_legacy(Role::User, "Hello!")];
//! let response = provider.chat(&messages).await?;
//! println!("{response}");
//! # Ok(())
//! # }
//! ```

pub mod any;
#[cfg(feature = "candle")]
pub mod candle_provider;
#[cfg(feature = "candle")]
pub mod candle_whisper;
pub mod classifier;
pub mod claude;
pub mod compatible;
#[cfg(feature = "candle")]
pub(crate) mod device;
pub mod ema;
pub(crate) mod embed;
pub mod error;
pub mod extractor;
pub mod gemini;
#[cfg(feature = "gonka")]
pub mod gonka;
pub mod http;
#[cfg(any(test, feature = "testing"))]
pub mod mock;
pub mod model_cache;
pub mod ollama;
pub mod openai;
pub mod provider;
pub mod provider_dyn;
pub(crate) mod retry;
pub mod router;
pub(crate) mod schema;
pub mod sse;
pub mod stt;
#[cfg(test)]
pub mod testing;
pub(crate) mod usage;
pub mod whisper;

pub use classifier::metrics::{ClassifierMetrics, ClassifierMetricsSnapshot, TaskMetricsSnapshot};
pub use error::LlmError;
pub use extractor::Extractor;
pub use provider::{ChatExtras, ChatStream, LlmProvider, StreamChunk, ThinkingBlock};
pub use provider_dyn::LlmProviderDyn;
pub use router::aware::RouterAware;
pub use router::coe::{CoeConfig, CoeMetrics, CoeRouter};
pub use stt::{SpeechToText, Transcription};
pub use zeph_config::{CacheTtl, GeminiThinkingLevel, ThinkingConfig, ThinkingEffort};
