// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared utility functions for Zeph crates.
//!
//! This crate provides pure utility functions (text manipulation, network helpers,
//! sanitization primitives) that are needed by multiple leaf crates. It has no
//! `zeph-*` dependencies. The optional `treesitter` feature adds tree-sitter query
//! constants and helpers; all other modules are dependency-free.

pub mod net;
pub mod sanitize;
pub mod text;

#[cfg(feature = "treesitter")]
pub mod treesitter;
