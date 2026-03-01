// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LLM provider abstraction and backend implementations.

pub mod any;
#[cfg(feature = "candle")]
pub mod candle_provider;
#[cfg(feature = "candle")]
pub mod candle_whisper;
pub mod claude;
pub mod compatible;
pub mod ema;
pub mod error;
#[cfg(feature = "schema")]
pub mod extractor;
pub mod http;
#[cfg(feature = "mock")]
pub mod mock;
pub mod model_cache;
pub mod ollama;
pub mod openai;
pub mod orchestrator;
pub mod provider;
pub(crate) mod retry;
pub mod router;
pub(crate) mod sse;
pub mod stt;
#[cfg(test)]
pub mod testing;
#[cfg(feature = "stt")]
pub mod whisper;

pub use claude::{ThinkingConfig, ThinkingEffort};
pub use error::LlmError;
#[cfg(feature = "schema")]
pub use extractor::Extractor;
pub use provider::{LlmProvider, ThinkingBlock};
pub use stt::{SpeechToText, Transcription};
