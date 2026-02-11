//! LLM provider abstraction and backend implementations.

pub mod any;
#[cfg(feature = "candle")]
pub mod candle_provider;
pub mod claude;
pub mod ollama;
#[cfg(feature = "openai")]
pub mod openai;
#[cfg(feature = "orchestrator")]
pub mod orchestrator;
pub mod provider;
