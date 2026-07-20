// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Doom-loop detection for the Zeph tool dispatch loop.
//!
//! This crate provides [`doom_loop_hash`], used by `zeph-core` to detect repeated tool-call
//! cycles during the agent turn loop.
//!
//! # Crate status
//!
//! This crate previously carried a sealed `AgentChannel` dispatcher-extraction trait (issue
//! #3516) with no implementors anywhere in the workspace; it was removed as dead code (issue
//! #6480). Reviving that dispatcher-extraction plan needs a fresh tracking issue.

pub mod doom_loop;

pub use doom_loop::doom_loop_hash;
