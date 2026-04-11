// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared utility functions and security primitives for Zeph crates.
//!
//! This crate provides pure utility functions (text manipulation, network helpers,
//! sanitization primitives), security primitives (`Secret`, `VaultError`), and
//! strongly-typed identifiers (`ToolName`, `SessionId`) that are needed by multiple crates.
//! It has no `zeph-*` dependencies. The optional `treesitter` feature adds tree-sitter
//! query constants and helpers.

pub mod config;
pub mod error_taxonomy;
pub mod hash;
pub mod math;
pub mod net;
pub mod patterns;
pub mod quarantine;
pub mod sanitize;
pub mod secret;
pub mod text;
pub mod trust_level;
pub mod types;

pub use trust_level::SkillTrustLevel;
pub use types::{SessionId, ToolName};

#[cfg(feature = "treesitter")]
pub mod treesitter;
