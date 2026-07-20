// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `AgentError` <-> `CommandError` conversion for the [`zeph_commands::AgentAccess`] trait
//! boundary.
//!
//! The 15 `AgentAccess` sub-trait implementations for [`Agent<C>`] live in the per-domain
//! `*_commands.rs` modules alongside this file (see `crate::agent::mod` for the full list),
//! not here.
//!
//! [`Agent<C>`]: super::Agent

use zeph_commands::CommandError;

use super::error::AgentError;

/// Convert `AgentError` to `CommandError` for the trait boundary.
impl From<AgentError> for CommandError {
    fn from(e: AgentError) -> Self {
        Self(e.to_string())
    }
}
