// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SKILL.md loader, skill registry, and prompt formatter.

pub mod bm25;
#[cfg(feature = "bundled-skills")]
pub mod bundled;
pub mod error;
pub mod evolution;
pub mod loader;
pub mod manager;
pub mod matcher;
pub mod prompt;
pub mod qdrant_matcher;
pub mod registry;
pub mod resource;
pub mod scanner;
pub mod trust;
pub mod trust_score;
pub mod watcher;

pub use error::SkillError;
pub use matcher::{IntentClassification, ScoredMatch};
pub use trust::{SkillSource, SkillTrust, TrustLevel, compute_skill_hash};
