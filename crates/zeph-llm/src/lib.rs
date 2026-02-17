//! LLM provider abstraction and backend implementations.

pub mod any;
#[cfg(feature = "candle")]
pub mod candle_provider;
pub mod claude;
pub mod compatible;
pub mod error;
#[cfg(feature = "mock")]
pub mod mock;
pub mod ollama;
pub mod openai;
pub mod orchestrator;
pub mod provider;
pub mod router;

pub use error::LlmError;
pub use provider::LlmProvider;
