// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared environment types: `ActionTrace` and `RecordedToolCall`.
//!
//! # Invariant
//!
//! The `ActionTrace` returned from `*Env::new_from_seed` MUST be the same `Arc` the
//! env stores internally. The env uses `Arc::clone` at construction time and returns
//! one clone to the caller (evaluator). Tests verify this by recording one call and
//! asserting `Arc::strong_count(&trace) >= 2`.
//!
//! # Mutex poison policy
//!
//! All code paths inside the locked region are pure `Vec::push` / `Vec::clone` — they
//! cannot panic without a programming bug. A poisoned mutex therefore indicates a bug
//! elsewhere; we propagate panic via `.expect("trace mutex poisoned")`. We do NOT use
//! `.unwrap_or_else(|p| p.into_inner())` — silent recovery hides bugs.
//!
//! # No await while locked
//!
//! `std::sync::Mutex` guards must never be held across `.await`. This is enforced via
//! the deny below.
#![deny(clippy::await_holding_lock)]

use std::sync::Arc;

pub mod airline;
pub mod retail;
pub mod tools;

/// Shared arc-mutex handle to the recorded tool call log.
///
/// Returned alongside the env from `*Env::new_from_seed`. Passed to
/// [`crate::loaders::tau2_bench::eval::TauBenchEvaluator`] which reads it after the
/// agent run completes.
pub type ActionTrace = Arc<std::sync::Mutex<Vec<RecordedToolCall>>>;

/// One tool call recorded by the environment executor.
///
/// Captured before the call is dispatched so the evaluator sees the exact arguments
/// the agent sent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RecordedToolCall {
    /// Tool name as provided by the agent.
    pub name: String,
    /// Arguments as provided by the agent (raw JSON map).
    pub arguments: serde_json::Map<String, serde_json::Value>,
}

impl RecordedToolCall {
    /// Construct from a [`zeph_tools::executor::ToolCall`].
    #[must_use]
    pub fn from_tool_call(call: &zeph_tools::executor::ToolCall) -> Self {
        Self {
            name: call.tool_id.as_str().to_owned(),
            arguments: call.params.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_tool_call_strong_count_invariant() {
        let trace: ActionTrace = Arc::new(std::sync::Mutex::new(Vec::new()));
        let trace2 = trace.clone();
        assert!(Arc::strong_count(&trace) >= 2);
        trace2.lock().expect("poisoned").push(RecordedToolCall {
            name: "test".into(),
            arguments: serde_json::Map::new(),
        });
        assert_eq!(trace.lock().expect("poisoned").len(), 1);
    }
}
