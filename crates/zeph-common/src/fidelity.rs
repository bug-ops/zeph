// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fidelity types for Context-Adaptive Memory (CAM).
//!
//! [`ContextFidelity`] is a three-level representation that replaces the binary
//! keep/discard approach used by compaction. [`PlannedToolHint`] carries lookahead
//! hints from the orchestration DAG so the fidelity scorer can bias toward messages
//! that are relevant to upcoming tool calls.

use serde::{Deserialize, Serialize};

/// Fidelity level assigned to a message in the context window.
///
/// Determines how a historical message is rendered before sending to the LLM.
/// Assigned by `FidelityScorer` based on relevance signals; stored in
/// `MessageMetadata.fidelity_tag` for debug tracing and compaction filtering.
///
/// # Examples
///
/// ```
/// use zeph_common::fidelity::ContextFidelity;
///
/// let level = ContextFidelity::default();
/// assert_eq!(level, ContextFidelity::Full);
///
/// let compressed: u8 = ContextFidelity::Compressed as u8;
/// assert_eq!(compressed, 1);
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum ContextFidelity {
    /// Original message content, unchanged.
    #[default]
    Full = 0,
    /// Content truncated to `compressed_max_tokens` tokens (or replaced by
    /// `deferred_summary` when available).
    Compressed = 1,
    /// Content replaced by a compact placeholder tag; no semantic content
    /// survives.
    Placeholder = 2,
}

/// Hint about an upcoming tool call derived from the orchestration DAG.
///
/// Used by `FidelityScorer` to bias relevance scores toward messages that
/// contain context useful for the next planned operations. In the v0.21 MVP
/// the hints are populated by callers that have access to the DAG lookahead;
/// an empty slice is always safe and disables the plan signal.
///
/// # Examples
///
/// ```
/// use zeph_common::fidelity::PlannedToolHint;
///
/// let hint = PlannedToolHint::new("shell", vec!["cargo".to_string(), "build".to_string()], 1);
/// assert_eq!(hint.tool_name, "shell");
/// assert_eq!(hint.distance_from_current, 1);
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedToolHint {
    /// Name of the planned tool.
    pub tool_name: String,
    /// Keywords extracted from the tool's planned arguments (best-effort).
    pub keywords: Vec<String>,
    /// Steps until this tool is scheduled. 1 = immediately next, capped at 5.
    pub distance_from_current: u8,
}

impl PlannedToolHint {
    /// Creates a new [`PlannedToolHint`].
    pub fn new(
        tool_name: impl Into<String>,
        keywords: Vec<String>,
        distance_from_current: u8,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            keywords,
            distance_from_current,
        }
    }
}
