//! SKILL.md loader, skill registry, and prompt formatter.

pub mod loader;
pub mod matcher;
pub mod prompt;
#[cfg(feature = "qdrant")]
pub mod qdrant_matcher;
pub mod registry;
pub mod watcher;
