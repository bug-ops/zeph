// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-turn state carrier for the agent loop.
//!
//! A [`Turn`] is created at the start of each `process_user_message` call, lives on the call
//! stack for the duration of the turn, and is consumed at the end via `Agent::end_turn`.
//! It is never stored on the `Agent` struct.
//!
//! # Phase 1 scope
//!
//! Only input, ID, metrics (timings only), and the cancellation token are tracked.
//! Fields `context`, `response_text`, and `tool_results` are deferred to Phase 2.

use tokio_util::sync::CancellationToken;
use zeph_llm::provider::MessagePart;

use crate::metrics::TurnTimings;

/// Monotonically increasing per-conversation turn identifier.
///
/// Wraps `debug_state.iteration_counter` as a proper newtype, enabling turn IDs to be passed
/// through the turn lifecycle and recorded in metrics and traces.
///
/// `TurnId(0)` is the first turn in a conversation. Values are strictly increasing by 1.
/// The counter resets to 0 when a new conversation starts (e.g., via `/new`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TurnId(pub u64);

impl TurnId {
    /// Return the next turn ID in sequence.
    #[allow(dead_code)]
    pub(crate) fn next(self) -> TurnId {
        TurnId(self.0 + 1)
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Resolved input for a single agent turn.
///
/// Built from a `ChannelMessage` after attachment resolution and image extraction.
/// Separates input parsing from input processing and bundles the two values previously
/// passed as separate arguments to `process_user_message`.
pub struct TurnInput {
    /// Plain-text user input. May be empty for image-only messages.
    pub text: String,
    /// Decoded image parts extracted from attachments or inline data.
    pub image_parts: Vec<MessagePart>,
}

impl TurnInput {
    /// Create input from text and image parts.
    #[must_use]
    pub fn new(text: String, image_parts: Vec<MessagePart>) -> Self {
        Self { text, image_parts }
    }
}

/// Per-turn performance measurements captured during the turn lifecycle.
///
/// In Phase 1, only `timings` is populated. Token counts and cost are managed via the existing
/// `MetricsState` path and are deferred to Phase 2.
#[derive(Debug, Default, Clone)]
pub struct TurnMetrics {
    /// Turn timing breakdown populated at each lifecycle phase boundary.
    pub timings: TurnTimings,
}

/// Record of a single tool execution within a turn.
///
/// Defined in Phase 1 for forward compatibility. Populated in Phase 2 when
/// `Turn.tool_results` is wired through the tool execution chain.
#[allow(dead_code)]
pub(crate) struct ToolOutputRecord {
    /// Registered tool name (e.g., `"shell"`, `"web_scrape"`).
    pub(crate) tool_name: String,
    /// Short human-readable summary of the tool output.
    pub(crate) summary: String,
    /// Wall-clock execution time in milliseconds.
    pub(crate) latency_ms: u64,
}

/// Complete state of a single agent turn from input through metrics capture.
///
/// Created at the start of `process_user_message`, populated through the turn lifecycle,
/// and consumed at the end for metrics emission and trace completion.
///
/// **Ownership**: `Turn` is stack-owned — created in `begin_turn`, passed through sub-methods
/// by `&mut Turn`, and consumed in `end_turn`. It is never stored on the `Agent` struct.
///
/// **Phase 1 scope**: carries `id`, `input`, `metrics`, and `cancel_token` only.
pub struct Turn {
    /// Monotonically increasing identifier for this turn within the conversation.
    pub id: TurnId,
    /// Resolved user input for this turn.
    pub input: TurnInput,
    /// Per-turn metrics accumulated during the turn lifecycle.
    pub metrics: TurnMetrics,
    /// Per-turn cancellation token. Cancelled when the user aborts the turn or the agent shuts
    /// down. Created fresh in [`Turn::new`] so each turn has an independent token.
    pub cancel_token: CancellationToken,
}

impl Turn {
    /// Create a new turn with the given ID and input.
    ///
    /// A fresh [`CancellationToken`] is created for each turn so that cancelling one turn
    /// does not affect subsequent turns.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zeph_core::agent::turn::{Turn, TurnId, TurnInput};
    /// # use zeph_llm::provider::MessagePart;
    /// let input = TurnInput::new("hello".to_owned(), vec![]);
    /// let turn = Turn::new(TurnId(0), input);
    /// assert_eq!(turn.id, TurnId(0));
    /// ```
    #[must_use]
    pub fn new(id: TurnId, input: TurnInput) -> Self {
        Self {
            id,
            input,
            metrics: TurnMetrics::default(),
            cancel_token: CancellationToken::new(),
        }
    }

    /// Return the turn ID.
    #[must_use]
    pub fn id(&self) -> TurnId {
        self.id
    }

    /// Return an immutable reference to the turn metrics.
    #[must_use]
    pub fn metrics_snapshot(&self) -> &TurnMetrics {
        &self.metrics
    }

    /// Return a mutable reference to the turn metrics.
    pub fn metrics_mut(&mut self) -> &mut TurnMetrics {
        &mut self.metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_new_sets_id() {
        let input = TurnInput::new("hello".to_owned(), vec![]);
        let turn = Turn::new(TurnId(7), input);
        assert_eq!(turn.id, TurnId(7));
        assert_eq!(turn.id(), TurnId(7));
    }

    #[test]
    fn turn_id_display() {
        assert_eq!(TurnId(42).to_string(), "42");
    }

    #[test]
    fn turn_id_next() {
        assert_eq!(TurnId(3).next(), TurnId(4));
    }

    #[test]
    fn turn_input_fields() {
        let input = TurnInput::new("hi".to_owned(), vec![]);
        assert_eq!(input.text, "hi");
        assert!(input.image_parts.is_empty());
    }

    #[test]
    fn turn_metrics_default_timings_are_zero() {
        let m = TurnMetrics::default();
        assert_eq!(m.timings.prepare_context_ms, 0);
        assert_eq!(m.timings.llm_chat_ms, 0);
    }

    #[test]
    fn turn_cancel_token_not_cancelled_on_new() {
        let input = TurnInput::new("x".to_owned(), vec![]);
        let turn = Turn::new(TurnId(0), input);
        assert!(!turn.cancel_token.is_cancelled());
    }

    #[test]
    fn turn_cancel_token_cancel() {
        let input = TurnInput::new("x".to_owned(), vec![]);
        let turn = Turn::new(TurnId(0), input);
        turn.cancel_token.cancel();
        assert!(turn.cancel_token.is_cancelled());
    }

    #[test]
    fn turn_metrics_mut_allows_write() {
        let input = TurnInput::new("x".to_owned(), vec![]);
        let mut turn = Turn::new(TurnId(0), input);
        turn.metrics_mut().timings.prepare_context_ms = 99;
        assert_eq!(turn.metrics_snapshot().timings.prepare_context_ms, 99);
    }
}
