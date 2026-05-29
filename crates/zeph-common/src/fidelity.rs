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
///
/// assert_eq!(ContextFidelity::from_u8(0), ContextFidelity::Full);
/// assert_eq!(ContextFidelity::from_u8(1), ContextFidelity::Compressed);
/// assert_eq!(ContextFidelity::from_u8(255), ContextFidelity::Full); // unknown → Full
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

impl ContextFidelity {
    /// Convert a database integer to a fidelity level.
    ///
    /// Unknown values map to [`Full`](ContextFidelity::Full) as the safe default,
    /// preserving forward compatibility when new variants are added.
    ///
    /// **Note for variant authors:** update this match when adding new variants,
    /// or old code will silently upgrade persisted values to `Full`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_common::fidelity::ContextFidelity;
    ///
    /// assert_eq!(ContextFidelity::from_u8(0), ContextFidelity::Full);
    /// assert_eq!(ContextFidelity::from_u8(1), ContextFidelity::Compressed);
    /// assert_eq!(ContextFidelity::from_u8(2), ContextFidelity::Placeholder);
    /// assert_eq!(ContextFidelity::from_u8(99), ContextFidelity::Full);
    /// ```
    #[must_use]
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Compressed,
            2 => Self::Placeholder,
            _ => Self::Full, // 0 = Full (default); unknown values also map to Full
        }
    }
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

#[cfg(test)]
mod tests {
    use super::ContextFidelity;

    #[test]
    fn from_u8_round_trip() {
        assert_eq!(ContextFidelity::from_u8(0), ContextFidelity::Full);
        assert_eq!(ContextFidelity::from_u8(1), ContextFidelity::Compressed);
        assert_eq!(ContextFidelity::from_u8(2), ContextFidelity::Placeholder);
    }

    #[test]
    fn from_u8_unknown_defaults_to_full() {
        assert_eq!(ContextFidelity::from_u8(3), ContextFidelity::Full);
        assert_eq!(ContextFidelity::from_u8(255), ContextFidelity::Full);
    }

    #[test]
    fn as_u8_matches_from_u8() {
        for level in [
            ContextFidelity::Full,
            ContextFidelity::Compressed,
            ContextFidelity::Placeholder,
        ] {
            assert_eq!(ContextFidelity::from_u8(level as u8), level);
        }
    }
}
