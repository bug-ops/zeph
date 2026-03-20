// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared utility functions and security primitives for Zeph crates.
//!
//! This crate provides pure utility functions (text manipulation, network helpers,
//! sanitization primitives) and security primitives (`Secret`, `VaultError`) that are
//! needed by multiple crates. It has no `zeph-*` dependencies. The optional `treesitter`
//! feature adds tree-sitter query constants and helpers.

pub mod math;
pub mod net;
pub mod sanitize;
pub mod secret;
pub mod text;

#[cfg(feature = "treesitter")]
pub mod treesitter;
