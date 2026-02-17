//! SKILL.md loader, skill registry, and prompt formatter.

pub mod error;
#[cfg(feature = "self-learning")]
pub mod evolution;
pub mod loader;
pub mod matcher;
pub mod prompt;
#[cfg(feature = "qdrant")]
pub mod qdrant_matcher;
pub mod registry;
pub(crate) mod resource;
pub mod trust;
pub mod watcher;

pub use error::SkillError;
pub use trust::{SkillSource, SkillTrust, TrustLevel, compute_skill_hash};
