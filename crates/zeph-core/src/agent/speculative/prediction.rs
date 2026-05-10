// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Decoding-level speculation: `ToolCallPredictor`.
//!
//! Drains the LLM `ToolStream` and fires speculative dispatches when the partial
//! JSON parser reports that all required fields are present for a tool call.
//!
//! Gate invariants enforced before dispatch (in order):
//! 1. `SpeculationMode` is `Decoding` or `Both`.
//! 2. `executor.is_tool_speculatable(tool_id)` returns `true`.
//! 3. `trust_level == TrustLevel::Trusted` (forwarded from the agent's current state).
//! 4. Calling `executor.execute_tool_call(call)` would NOT return `ConfirmationRequired`
//!    — checked by attempting a dry-run classification (see `is_confirmation_required`).

#![allow(dead_code)]

use zeph_common::ToolName;
use zeph_tools::{ToolCall, ToolError};

/// Source of a tool call prediction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PredictionSource {
    /// Produced from live `ToolStream` partial tokens (issue #2290).
    StreamPartial,
    /// Produced from `SQLite` `tool_pattern_transitions` table (issue #2409).
    HistoryPattern { skill: String, rank: u8 },
}

/// A predicted tool call, ready for speculative dispatch.
#[derive(Debug, Clone)]
pub struct Prediction {
    /// Tool identifier.
    pub tool_id: ToolName,
    /// Reconstructed argument map from partial JSON or pattern history.
    pub args: serde_json::Map<String, serde_json::Value>,
    /// Confidence score in `[0.0, 1.0]`.
    pub confidence: f32,
    /// Where this prediction came from.
    pub source: PredictionSource,
}

impl Prediction {
    /// Convert to a [`ToolCall`] for dispatch through the executor.
    #[must_use]
    pub fn to_tool_call(&self, call_id: impl Into<String>) -> ToolCall {
        ToolCall {
            tool_id: self.tool_id.clone(),
            params: self.args.clone(),
            caller_id: Some(call_id.into()),
            context: None,

            tool_call_id: String::new(),
        }
    }
}

/// Check whether a `ToolError` represents a confirmation gate hit.
///
/// Used as a gate before speculative dispatch: if the executor would return
/// `ConfirmationRequired`, speculation is skipped for that call.
#[must_use]
pub fn is_confirmation_error(err: &ToolError) -> bool {
    matches!(err, ToolError::ConfirmationRequired { .. })
}
