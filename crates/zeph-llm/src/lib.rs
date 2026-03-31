// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LLM provider abstraction and backend implementations.

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
pub mod error;
pub mod extractor;
pub mod gemini;
pub mod http;
pub mod mock;
pub mod model_cache;
pub mod ollama;
pub mod openai;
pub mod provider;
pub(crate) mod retry;
pub mod router;
pub(crate) mod schema;
pub(crate) mod sse;
pub mod stt;
#[cfg(test)]
pub mod testing;
pub(crate) mod usage;
#[cfg(feature = "stt")]
pub mod whisper;

pub use classifier::metrics::{ClassifierMetrics, ClassifierMetricsSnapshot, TaskMetricsSnapshot};
pub use claude::{ThinkingConfig, ThinkingEffort};
pub use error::LlmError;
pub use extractor::Extractor;
pub use gemini::ThinkingLevel as GeminiThinkingLevel;
pub use provider::{ChatStream, LlmProvider, StreamChunk, ThinkingBlock};
pub use stt::{SpeechToText, Transcription};
