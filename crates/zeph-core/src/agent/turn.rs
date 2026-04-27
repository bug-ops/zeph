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
//! Only input, context, and metrics (timings only) are tracked.
//! Fields `response_text` and `tool_results` are deferred to Phase 2.

use tokio_util::sync::CancellationToken;
pub use zeph_context::turn_context::{TurnContext, TurnId};
use zeph_llm::provider::MessagePart;

use crate::metrics::TurnTimings;

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
/// **Phase 1 scope**: carries `context` (id, cancel token, timeouts), `input`, and `metrics`.
pub struct Turn {
    /// Per-turn execution context (id, cancel token, timeouts).
    pub context: TurnContext,
    /// Resolved user input for this turn.
    pub input: TurnInput,
    /// Per-turn metrics accumulated during the turn lifecycle.
    pub metrics: TurnMetrics,
}

impl Turn {
    /// Create a new turn with the given context and input.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zeph_core::agent::turn::{Turn, TurnContext, TurnId, TurnInput};
    /// # use zeph_config::security::TimeoutConfig;
    /// # use tokio_util::sync::CancellationToken;
    /// # use zeph_llm::provider::MessagePart;
    /// let ctx = TurnContext::new(TurnId(0), CancellationToken::new(), TimeoutConfig::default());
    /// let input = TurnInput::new("hello".to_owned(), vec![]);
    /// let turn = Turn::new(ctx, input);
    /// assert_eq!(turn.id(), TurnId(0));
    /// ```
    #[must_use]
    pub fn new(context: TurnContext, input: TurnInput) -> Self {
        Self {
            context,
            input,
            metrics: TurnMetrics::default(),
        }
    }

    /// Return the turn ID.
    #[must_use]
    pub fn id(&self) -> TurnId {
        self.context.id
    }

    /// Return an immutable reference to the per-turn cancellation token.
    #[must_use]
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.context.cancel_token
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
    use tokio_util::sync::CancellationToken;
    use zeph_config::security::TimeoutConfig;

    use super::*;

    fn make_turn(id: u64, text: &str) -> Turn {
        let ctx = TurnContext::new(
            TurnId(id),
            CancellationToken::new(),
            TimeoutConfig::default(),
        );
        let input = TurnInput::new(text.to_owned(), vec![]);
        Turn::new(ctx, input)
    }

    #[test]
    fn turn_new_sets_id() {
        let turn = make_turn(7, "hello");
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
        let turn = make_turn(0, "x");
        assert!(!turn.cancel_token().is_cancelled());
    }

    #[test]
    fn turn_cancel_token_cancel() {
        let turn = make_turn(0, "x");
        turn.cancel_token().cancel();
        assert!(turn.cancel_token().is_cancelled());
    }

    #[test]
    fn turn_metrics_mut_allows_write() {
        let mut turn = make_turn(0, "x");
        turn.metrics_mut().timings.prepare_context_ms = 99;
        assert_eq!(turn.metrics_snapshot().timings.prepare_context_ms, 99);
    }
}
