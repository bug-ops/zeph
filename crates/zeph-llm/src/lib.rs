//! LLM provider abstraction and backend implementations.

pub mod any;
#[cfg(feature = "candle")]
pub mod candle_provider;
pub mod claude;
#[cfg(feature = "compatible")]
pub mod compatible;
pub mod error;
pub mod ollama;
#[cfg(feature = "openai")]
pub mod openai;
#[cfg(feature = "orchestrator")]
pub mod orchestrator;
pub mod provider;
#[cfg(feature = "router")]
pub mod router;

pub use error::LlmError;
pub use provider::LlmProvider;
