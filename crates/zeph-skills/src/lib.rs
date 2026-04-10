// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Skill subsystem: SKILL.md parser, registry, embedding matcher, hot-reload, and self-learning.
//!
//! # Overview
//!
//! `zeph-skills` is the skills runtime for the Zeph AI agent. Skills are Markdown documents
//! (`SKILL.md`) with a YAML frontmatter header that describe what an agent can do, which tools
//! it may invoke, and how to use them. At runtime the agent matches user intent to skills via
//! embedding similarity and BM25 lexical ranking, then injects the matching skill body as
//! prompt context.
//!
//! # Module Map
//!
//! | Module | Responsibility |
//! |--------|---------------|
//! | [`loader`] | Parse and validate `SKILL.md` frontmatter and body |
//! | [`registry`] | In-process skill index with lazy body loading |
//! | [`matcher`] | Async embedding-based skill matching with two-stage category filtering |
//! | [`bm25`] | In-memory BM25 inverted index with Reciprocal Rank Fusion |
//! | [`watcher`] | Filesystem hot-reload via `notify` debouncer |
//! | [`bundled`] | Compile-time embedded bundled skills and startup provisioning |
//! | [`manager`] | Install / uninstall / verify skills from git URLs or local paths |
//! | [`trust`] | Trust levels and source tracking for installed skills |
//! | [`trust_score`] | Bayesian Wilson-score re-ranking of skill match candidates |
//! | [`scanner`] | Advisory prompt-injection pattern scanner |
//! | [`prompt`] | Format skills into prompt XML blocks |
//! | [`generator`] | NL-to-SKILL.md generation via LLM |
//! | [`evolution`] | Self-learning: failure classification, step corrections, outcome tracking |
//! | [`erl`] | Experiential Reflective Learning: heuristic extraction from completed tasks |
//! | [`stem`] | STEM: automatic detection of recurring tool-use patterns |
//! | [`rl_head`] | 2-layer MLP routing head trained with REINFORCE for skill re-ranking |
//! | [`resource`] | Skill-local resource file discovery and loading |
//! | [`qdrant_matcher`] | Qdrant-backed vector store for skill matching at scale |
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use zeph_skills::registry::SkillRegistry;
//!
//! # fn try_main() -> Result<(), zeph_skills::SkillError> {
//! // Load all skills from a directory (metadata only — bodies are lazy).
//! let registry = SkillRegistry::load(&["/path/to/skills"]);
//! println!("loaded {} skills", registry.all_meta().len());
//!
//! // Look up a skill body by name.
//! let body = registry.get_body("my-skill")?;
//! println!("{}", body);
//! # Ok(())
//! # }
//! ```

pub mod bm25;
pub mod bundled;
pub mod erl;
pub mod error;
pub mod evolution;
pub mod generator;
pub mod loader;
pub mod manager;
pub mod matcher;
pub mod miner;
pub mod prompt;
pub mod qdrant_matcher;
pub mod registry;
pub mod resource;
pub mod rl_head;
pub mod scanner;
pub mod stem;
pub mod trust;
pub mod trust_score;
pub mod watcher;

pub use error::SkillError;
pub use generator::{GeneratedSkill, SkillGenerationRequest, SkillGenerator};
pub use matcher::{IntentClassification, ScoredMatch};
pub use trust::{SkillSource, SkillTrust, SkillTrustLevel, compute_skill_hash};
