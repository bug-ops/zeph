// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[derive(Debug, thiserror::Error)]
pub enum SubAgentError {
    #[error("parse error in {path}: {reason}")]
    Parse { path: String, reason: String },

    #[error("invalid definition: {0}")]
    Invalid(String),

    #[error("agent not found: {0}")]
    NotFound(String),

    #[error("spawn failed: {0}")]
    Spawn(String),

    #[error("cancelled")]
    Cancelled,

    #[error("invalid command: {0}")]
    InvalidCommand(String),

    #[error("memory error for agent '{name}': {reason}")]
    Memory { name: String, reason: String },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
