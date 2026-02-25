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
pub mod error;
#[cfg(feature = "schema")]
pub mod extractor;
pub(crate) mod http;
#[cfg(feature = "mock")]
pub mod mock;
pub mod ollama;
pub mod openai;
pub mod orchestrator;
pub mod provider;
pub(crate) mod retry;
pub mod router;
pub(crate) mod sse;
pub mod stt;
#[cfg(feature = "stt")]
pub mod whisper;

pub use error::LlmError;
#[cfg(feature = "schema")]
pub use extractor::Extractor;
pub use provider::LlmProvider;
pub use stt::{SpeechToText, Transcription};
