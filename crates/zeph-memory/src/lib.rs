//! SQLite-backed conversation persistence with Qdrant vector search.

pub mod qdrant;
pub mod semantic;
pub mod sqlite;

// Re-export commonly used token estimation function
pub use semantic::estimate_tokens;
