// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`LlmCondenser`]: the default [`Condenser`] implementation, reusing
//! `zeph_context::summarization::summarize_structured` for durable, replayable condensation
//! (spec §8).

use zeph_context::summarization::{SummarizationDeps, summarize_structured};
use zeph_llm::provider::{Message, MessagePart, Role};

use crate::condenser::{CondensationResult, Condenser, validate_non_overlap};
use crate::error::SessionError;
use crate::event::{SessionEvent, SessionEventEnvelope};
use crate::replay::{ReconstructedState, ReplayEngine};

const CONDENSATION_GUIDELINES: &str = "Summarize the conversation so far, preserving the \
    session's intent, files modified, decisions made, open questions, and next steps. This \
    summary durably replaces the condensed portion of the event log — be precise, do not invent \
    details.";

/// Default [`Condenser`] implementation: summarizes a session's tail via an LLM, reusing
/// `zeph_context::summarization::summarize_structured`. Distinct from live in-memory compaction
/// (owned by `zeph-context`) — this operates on the durable event log and is recorded as a
/// [`crate::event::SessionEvent::Condensation`] event so replay can fold the same summary
/// deterministically (spec §8, AC-6).
pub struct LlmCondenser {
    deps: SummarizationDeps,
    /// Trigger threshold: [`Condenser::should_condense`] returns `true` once
    /// `budget_used_fraction` reaches this value (`0.0..=1.0`).
    threshold: f64,
    /// Number of trailing messages (`UserMessage`/`AssistantMessage`-initiated) to always keep
    /// un-condensed.
    keep_recent: usize,
}

impl LlmCondenser {
    /// Construct a new condenser with the given LLM dependencies, trigger threshold (fraction of
    /// context budget used, `0.0..=1.0`), and number of trailing messages to always keep.
    #[must_use]
    pub fn new(deps: SummarizationDeps, threshold: f64, keep_recent: usize) -> Self {
        Self {
            deps,
            threshold,
            keep_recent,
        }
    }
}

impl Condenser for LlmCondenser {
    fn should_condense(
        &self,
        state: &ReconstructedState,
        budget_used_fraction: f64,
    ) -> impl std::future::Future<Output = bool> + Send {
        std::future::ready(
            budget_used_fraction >= self.threshold && state.messages.len() > self.keep_recent,
        )
    }

    #[tracing::instrument(
        name = "session.condenser.condense",
        skip_all,
        level = "info",
        fields(event_count = events.len(), last_condensed_seq)
    )]
    async fn condense(
        &self,
        events: &[SessionEventEnvelope],
        last_condensed_seq: u64,
    ) -> Result<CondensationResult, SessionError> {
        // N2 (impl-critic re-verify finding): restrict to events strictly after the INV-SP-4
        // watermark before computing boundaries. Without this, a *second* condensation call
        // re-includes the first's already-condensed range (callers pass the full log, not a
        // pre-sliced tail), `to_condense` starts back near seq 0, and `validate_non_overlap`
        // rejects every condensation after the first — durable condensation degrades to
        // single-shot per session even though the caller keeps invoking it on every resume.
        let uncondensed: Vec<SessionEventEnvelope> = events
            .iter()
            .filter(|e| e.seq > last_condensed_seq)
            .cloned()
            .collect();

        // Indices of events that start a new agent-ready message; `ToolCall` attaches to the
        // preceding `AssistantMessage` boundary and `ToolResult` to its own open tool-result
        // batch (see `ReplayEngine::fold`) rather than starting a counted boundary of its own.
        let boundaries: Vec<usize> = uncondensed
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    e.kind,
                    SessionEvent::UserMessage { .. } | SessionEvent::AssistantMessage { .. }
                )
            })
            .map(|(i, _)| i)
            .collect();

        if boundaries.len() <= self.keep_recent {
            return Err(SessionError::CondensationOverlap(format!(
                "not enough events to condense: {} message(s) available, keep_recent={}",
                boundaries.len(),
                self.keep_recent
            )));
        }

        let cutoff = boundaries[boundaries.len() - self.keep_recent];
        let to_condense = &uncondensed[..cutoff];
        let lo = to_condense
            .first()
            .map_or(last_condensed_seq + 1, |e| e.seq);
        let hi = to_condense.last().map_or(last_condensed_seq, |e| e.seq);
        validate_non_overlap(last_condensed_seq, (lo, hi))?;

        let folded = ReplayEngine::fold(to_condense.to_vec(), None);
        let tokens_before: usize = folded
            .messages
            .iter()
            .map(|m| self.deps.token_counter.count_message_tokens(m))
            .sum();

        let summary = summarize_structured(&self.deps, &folded.messages, CONDENSATION_GUIDELINES)
            .await
            .map_err(SessionError::Llm)?;

        let summary_message = Message::from_parts(
            Role::System,
            vec![MessagePart::Summary {
                text: summary.to_markdown(),
            }],
        );
        let tokens_after = self
            .deps
            .token_counter
            .count_message_tokens(&summary_message);

        Ok(CondensationResult {
            replaced_range: (lo, hi),
            summary,
            tokens_before: u32::try_from(tokens_before).unwrap_or(u32::MAX),
            tokens_after: u32::try_from(tokens_after).unwrap_or(u32::MAX),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    use super::*;
    use crate::event::SessionEventEnvelope;

    struct WordCountTokenCounter;
    impl zeph_context::summarization::MessageTokenCounter for WordCountTokenCounter {
        fn count_message_tokens(&self, msg: &Message) -> usize {
            msg.content.split_whitespace().count().max(1)
        }
    }

    fn deps() -> SummarizationDeps {
        let summary_json = serde_json::json!({
            "session_intent": "implement feature X",
            "files_modified": ["src/lib.rs"],
            "decisions_made": ["used approach A"],
            "open_questions": [],
            "next_steps": ["write tests"],
        })
        .to_string();
        SummarizationDeps {
            provider: AnyProvider::Mock(MockProvider::with_responses(vec![summary_json])),
            llm_timeout: Duration::from_secs(5),
            token_counter: std::sync::Arc::new(WordCountTokenCounter),
            structured_summaries: true,
            on_anchored_summary: None,
        }
    }

    fn envelope(seq: u64, kind: SessionEvent) -> SessionEventEnvelope {
        SessionEventEnvelope::new(seq, None, None, kind)
    }

    fn user_msg(seq: u64, text: &str) -> SessionEventEnvelope {
        envelope(
            seq,
            SessionEvent::UserMessage {
                text: text.to_owned(),
                image_refs: vec![],
            },
        )
    }

    fn assistant_msg(seq: u64, text: &str) -> SessionEventEnvelope {
        envelope(
            seq,
            SessionEvent::AssistantMessage {
                parts: vec![MessagePart::Text {
                    text: text.to_owned(),
                }],
            },
        )
    }

    #[tokio::test]
    async fn should_condense_respects_threshold_and_keep_recent() {
        let condenser = LlmCondenser::new(deps(), 0.8, 4);
        let mut state = ReconstructedState::default();
        assert!(
            !condenser.should_condense(&state, 0.9).await,
            "too few messages"
        );

        state.messages = vec![
            Message::from_legacy(Role::User, "a"),
            Message::from_legacy(Role::User, "b"),
            Message::from_legacy(Role::User, "c"),
            Message::from_legacy(Role::User, "d"),
            Message::from_legacy(Role::User, "e"),
        ];
        assert!(
            !condenser.should_condense(&state, 0.5).await,
            "below threshold"
        );
        assert!(
            condenser.should_condense(&state, 0.8).await,
            "at threshold, enough messages"
        );
    }

    #[tokio::test]
    async fn condense_rejects_when_not_enough_events() {
        let condenser = LlmCondenser::new(deps(), 0.8, 4);
        let events = vec![user_msg(0, "a"), assistant_msg(1, "b")];
        let err = condenser.condense(&events, 0).await.unwrap_err();
        assert!(matches!(err, SessionError::CondensationOverlap(_)));
    }

    #[tokio::test]
    async fn condense_computes_replaced_range_keeping_recent_tail() {
        let condenser = LlmCondenser::new(deps(), 0.8, 1);
        // seq 0 is conventionally `SessionStarted` (never condensable — `validate_non_overlap`
        // treats `last_condensed_seq=0` as "nothing condensed yet", so a range cannot start at
        // seq 0); real logs always have a non-message event there, so start message seqs at 1.
        let events = vec![
            user_msg(1, "first question"),
            assistant_msg(2, "first answer"),
            user_msg(3, "second question"),
        ];
        // keep_recent=1 keeps the last message-starting event (seq 3); condenses [1, 2].
        let result = condenser.condense(&events, 0).await.unwrap();
        assert_eq!(result.replaced_range, (1, 2));
        assert!(result.tokens_before > 0);
    }

    /// N2 regression (impl-critic re-verify finding): a second condensation on a *growing* log
    /// — the full event history so far, not a pre-sliced tail, matching how
    /// `maybe_condense_on_resume` actually calls this in production — must succeed and cover a
    /// range strictly after the first condensation's, not re-attempt the already-condensed
    /// range and fail `validate_non_overlap`. Two fresh `LlmCondenser` instances (matching
    /// production: a new condenser is built per resume) each carry one mock LLM response.
    #[tokio::test]
    async fn condense_twice_on_growing_log_advances_past_prior_range() {
        let first_condenser = LlmCondenser::new(deps(), 0.8, 1);
        let mut events = vec![
            user_msg(1, "q1"),
            assistant_msg(2, "a1"),
            user_msg(3, "q2"),
            assistant_msg(4, "a2"),
            user_msg(5, "q3"),
            assistant_msg(6, "a3"),
            user_msg(7, "q4"),
        ];
        let first_result = first_condenser.condense(&events, 0).await.unwrap();
        assert_eq!(first_result.replaced_range, (1, 6));

        // The session keeps growing after the first condensation — a resume some turns later
        // sees the FULL log (seq 1..10), not just the tail since last_condensed_seq.
        events.push(assistant_msg(8, "a4"));
        events.push(user_msg(9, "q5"));
        events.push(assistant_msg(10, "a5"));

        let second_condenser = LlmCondenser::new(deps(), 0.8, 1);
        let second_result = second_condenser
            .condense(&events, first_result.replaced_range.1)
            .await
            .expect("second condensation on a growing log must not re-attempt the first's range");
        assert_eq!(
            second_result.replaced_range,
            (7, 9),
            "second condensation must start strictly after the first's replaced_range.1"
        );
    }
}
