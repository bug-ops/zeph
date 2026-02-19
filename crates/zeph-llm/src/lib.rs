//! LLM provider abstraction and backend implementations.

pub mod any;
#[cfg(feature = "candle")]
pub mod candle_provider;
#[cfg(feature = "candle")]
pub mod candle_whisper;
pub mod claude;
pub mod compatible;
pub mod error;
pub mod extractor;
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
pub use extractor::Extractor;
pub use provider::LlmProvider;
pub use stt::{SpeechToText, Transcription};
