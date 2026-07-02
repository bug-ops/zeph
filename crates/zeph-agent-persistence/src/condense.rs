// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Resume-time durable condensation trigger (spec-068 §8.1, architect rulings D-11/D-13).
//!
//! #3102 durable condensation is one of spec §1's three co-equal headline features. Before D-11,
//! [`zeph_session::LlmCondenser`] was fully built but never invoked from any production call
//! site — `[session.condense]` config was dead and `last_condensed_seq` never advanced.
//! [`maybe_condense_on_resume`] is the missing wiring: called immediately after
//! [`crate::hydrate_from_event_log`] on a resume path, it checks whether the replayed state
//! exceeds the condenser's threshold and, if so, durably condenses the session's tail.
//!
//! [`hydrate_and_condense`] (D-13) is the single entry point every session-open path should use
//! instead: it wraps [`crate::hydrate_from_event_log`] and folds in the
//! `maybe_condense_on_resume` call automatically, so condensation cannot be wired into some
//! resume paths and silently forgotten on others.

use zeph_context::summarization::MessageTokenCounter;
use zeph_llm::provider::Message;
use zeph_session::{
    Condenser, ReconstructedState, SessionEvent, SessionEventEnvelope, SessionEventLog,
    SessionStore,
};

use crate::error::PersistenceError;

/// Run resume-time durable condensation (spec §8.1) if `condenser.should_condense` returns
/// `true` for the replayed `state` at `budget_used_fraction`. No-op otherwise.
///
/// On trigger: reads the session's current `last_condensed_seq` (the INV-SP-4 non-overlap
/// watermark), asks `condenser` to summarize everything up to its `keep_recent` cutoff, appends
/// the resulting [`SessionEvent::Condensation`] event to `log`, and advances
/// `acp_sessions.last_condensed_seq` to match.
///
/// # Errors
///
/// Returns [`PersistenceError`] if reading session metadata, condensing, appending the
/// `Condensation` event, or advancing `last_condensed_seq` fails. A condensation failure (e.g.
/// the LLM call errors, or `events` is too short) must not fail the resume itself — callers
/// should log and continue, matching [`crate::bootstrap_legacy_session`]'s established
/// soft-fail contract for optional resume-time work.
#[tracing::instrument(
    name = "persistence.condense.maybe_run",
    skip_all,
    level = "debug",
    fields(session_id, budget_used_fraction)
)]
pub async fn maybe_condense_on_resume<C: Condenser>(
    condenser: &C,
    log: &SessionEventLog,
    store: &SessionStore,
    session_id: &str,
    state: &ReconstructedState,
    events: &[SessionEventEnvelope],
    budget_used_fraction: f64,
) -> Result<(), PersistenceError> {
    if !condenser.should_condense(state, budget_used_fraction).await {
        return Ok(());
    }

    let Some(meta) = store.get(session_id).await? else {
        return Ok(());
    };

    let result = condenser.condense(events, meta.last_condensed_seq).await?;

    log.append(
        None,
        None,
        SessionEvent::Condensation {
            replaced_seq_range: result.replaced_range,
            summary: result.summary,
            tokens_before: result.tokens_before,
            tokens_after: result.tokens_after,
        },
    )
    .await?;

    // M2 (spec §8.3 INV-SP-4): advance the same non-overlap ledger live Compaction
    // (`SessionSink::record_compaction`) also participates in, so a later condensation or
    // compaction pass cannot overlap the range just condensed.
    store
        .set_condensed_seq(session_id, result.replaced_range.1)
        .await?;

    tracing::info!(
        session_id,
        replaced_range = ?result.replaced_range,
        tokens_before = result.tokens_before,
        tokens_after = result.tokens_after,
        "spec-068 §8.1: durable condensation ran on resume"
    );

    Ok(())
}

/// Fraction of `window` tokens `messages` consumes — the `budget_used_fraction` input
/// [`Condenser::should_condense`] expects (spec §8.1, architect ruling D-13).
///
/// Deliberately Agent-free: needs only a token counter and a window size, both resolvable at
/// agent-construction time (before a live `Agent`'s `ContextBudget` exists) — see
/// [`hydrate_and_condense`]'s module-level rationale for why this was the one piece D-11
/// mistakenly assumed required a live `Agent`.
///
/// Returns `0.0` if `window == 0` (a misconfigured or not-yet-resolved budget) rather than
/// dividing by zero — this only suppresses condensation from firing, never panics.
#[must_use]
pub fn resume_budget_fraction(
    messages: &[Message],
    token_counter: &dyn MessageTokenCounter,
    window: usize,
) -> f64 {
    if window == 0 {
        return 0.0;
    }
    let tokens: usize = messages
        .iter()
        .map(|m| token_counter.count_message_tokens(m))
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let fraction = tokens as f64 / window as f64;
    fraction
}

/// [`crate::hydrate_from_event_log`] (D-10) plus resume-time durable condensation (D-11),
/// centralized so all four session-open paths (ACP, CLI `sessions resume`, `/conv resume`/
/// `fork`, `zeph serve` reactivation) get condensation automatically instead of each needing
/// its own inline `maybe_condense_on_resume` call — the same "one shared pipeline, not N
/// diverging copies" principle D-10 already established for hydration itself (architect ruling
/// D-13, spec-068 N3).
///
/// D-13's key correction: [`maybe_condense_on_resume`] takes no `Agent` parameter — `condenser`
/// is buildable from config + the provider registry (both resolved before agent construction at
/// every call site) and `budget_used_fraction` is `resume_budget_fraction`'s plain arithmetic
/// over the just-replayed messages. There is no path that structurally *cannot* call this; the
/// three non-`/conv` sites simply didn't have a live `Agent`'s convenience accessors
/// (`ContextBudget`, `resolve_background_provider`) to reach for and mistakenly concluded
/// condensation needed one.
///
/// Soft-fails condensation exactly like [`maybe_condense_on_resume`] itself — a condensation
/// failure never fails the resume; only [`crate::hydrate_from_event_log`]'s own hard-error
/// conditions (log open/read failure) propagate.
///
/// # Errors
///
/// Returns [`PersistenceError`] under the same conditions as
/// [`crate::hydrate_from_event_log`] — condensation failures are logged and swallowed, not
/// propagated.
#[allow(clippy::too_many_arguments)] // hydrate_from_event_log's 6 params + condenser/token_counter/context_window
#[tracing::instrument(
    name = "persistence.condense.hydrate_and_condense",
    skip_all,
    level = "info",
    fields(session_id, context_window)
)]
pub async fn hydrate_and_condense<C: Condenser>(
    session_path: &std::path::Path,
    store: &SessionStore,
    session_id: &str,
    conversation_id: zeph_memory::types::ConversationId,
    memory: &zeph_memory::semantic::SemanticMemory,
    up_to: Option<u64>,
    condenser: &C,
    token_counter: &dyn MessageTokenCounter,
    context_window: usize,
) -> Result<crate::hydrate::Hydrated, PersistenceError> {
    let hydrated = crate::hydrate::hydrate_from_event_log(
        session_path,
        store,
        session_id,
        conversation_id,
        memory,
        up_to,
    )
    .await?;

    if !hydrated.messages.is_empty() {
        let budget_used_fraction =
            resume_budget_fraction(&hydrated.messages, token_counter, context_window);
        let state = ReconstructedState {
            messages: hydrated.messages.clone(),
            ..Default::default()
        };
        if let Err(e) = maybe_condense_on_resume(
            condenser,
            &hydrated.log,
            store,
            session_id,
            &state,
            &hydrated.events,
            budget_used_fraction,
        )
        .await
        {
            tracing::warn!(error = %e, session_id, "spec-068 §8.1: resume-time condensation failed");
        }
    }

    Ok(hydrated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use zeph_context::summarization::{MessageTokenCounter, SummarizationDeps};
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{Message, MessagePart, Role};
    use zeph_session::LlmCondenser;

    struct WordCountTokenCounter;
    impl MessageTokenCounter for WordCountTokenCounter {
        fn count_message_tokens(&self, msg: &Message) -> usize {
            msg.content.split_whitespace().count().max(1)
        }
    }

    fn make_condenser(response: &str) -> LlmCondenser {
        let deps = SummarizationDeps {
            provider: AnyProvider::Mock(MockProvider::with_responses(vec![response.to_owned()])),
            llm_timeout: Duration::from_secs(5),
            token_counter: std::sync::Arc::new(WordCountTokenCounter),
            structured_summaries: true,
            on_anchored_summary: None,
        };
        LlmCondenser::new(deps, 0.5, 1)
    }

    fn summary_json() -> String {
        serde_json::json!({
            "session_intent": "implement feature X",
            "files_modified": ["src/lib.rs"],
            "decisions_made": ["used approach A"],
            "open_questions": [],
            "next_steps": ["write tests"],
        })
        .to_string()
    }

    async fn make_store(pool: zeph_db::DbPool) -> SessionStore {
        let store = SessionStore::new(pool);
        store.create("s1").await.unwrap();
        store
    }

    #[tokio::test]
    async fn below_threshold_is_a_noop() {
        let condenser = make_condenser(&summary_json());
        let config = zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        };
        let pool = config.connect().await.unwrap();
        zeph_db::run_migrations(&pool).await.unwrap();
        let store = make_store(pool).await;
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();

        let state = ReconstructedState {
            messages: vec![Message::from_parts(
                Role::User,
                vec![MessagePart::Text {
                    text: "hi".to_owned(),
                }],
            )],
            ..Default::default()
        };

        maybe_condense_on_resume(&condenser, &log, &store, "s1", &state, &[], 0.1)
            .await
            .unwrap();

        let meta = store.get("s1").await.unwrap().unwrap();
        assert_eq!(meta.last_condensed_seq, 0);
        assert!(log.read_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn above_threshold_condenses_and_advances_watermark() {
        let condenser = make_condenser(&summary_json());
        let config = zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        };
        let pool = config.connect().await.unwrap();
        zeph_db::run_migrations(&pool).await.unwrap();
        let store = make_store(pool).await;
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();

        // 3 message events; keep_recent=1 means the first 2 are condensed.
        let mut events = Vec::new();
        for i in 0..3 {
            let envelope = log
                .append(
                    None,
                    None,
                    SessionEvent::UserMessage {
                        text: format!("message {i}"),
                        image_refs: Vec::new(),
                    },
                )
                .await
                .unwrap();
            events.push(envelope);
        }

        let state = ReconstructedState {
            messages: vec![
                Message::from_parts(
                    Role::User,
                    vec![MessagePart::Text {
                        text: "a".to_owned(),
                    }],
                ),
                Message::from_parts(
                    Role::User,
                    vec![MessagePart::Text {
                        text: "b".to_owned(),
                    }],
                ),
                Message::from_parts(
                    Role::User,
                    vec![MessagePart::Text {
                        text: "c".to_owned(),
                    }],
                ),
            ],
            ..Default::default()
        };

        maybe_condense_on_resume(&condenser, &log, &store, "s1", &state, &events, 0.9)
            .await
            .unwrap();

        let meta = store.get("s1").await.unwrap().unwrap();
        assert_eq!(
            meta.last_condensed_seq, 1,
            "condensed through seq 1 (2 of 3 events)"
        );

        let all_events = log.read_all().await.unwrap();
        assert_eq!(
            all_events.len(),
            4,
            "3 original events + 1 Condensation event"
        );
        assert!(matches!(
            all_events.last().unwrap().kind,
            SessionEvent::Condensation { .. }
        ));
    }

    /// D-13 regression (spec-068 §8.1, N3): `hydrate_and_condense` must actually condense, not
    /// just hydrate — this is the exact wiring CLI/ACP/`zeph serve` construction sites now
    /// depend on (previously only `/conv resume`'s hand-inlined copy was exercised, and that
    /// copy is gone post-D-13 refactor). Drives the real production write path
    /// (`SessionSink::record_message`, not raw `log.append`) through `hydrate_and_condense` in
    /// one call and asserts both effects: the durable log gained a `Condensation` event and the
    /// returned `Hydrated.messages` still reflects the full pre-condensation replay (callers
    /// preload the complete history; condensation is a log-level side effect, not a truncation
    /// of what's handed back here).
    #[tokio::test]
    async fn hydrate_and_condense_triggers_condensation_end_to_end() {
        let memory = zeph_memory::semantic::SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            AnyProvider::Mock(MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let store = SessionStore::new(memory.sqlite().pool().clone());
        store.create("s1").await.unwrap();
        let dir = tempfile::tempdir().unwrap();

        let log = std::sync::Arc::new(SessionEventLog::open(dir.path()).await.unwrap());
        let sink = crate::SessionSink::new(
            std::sync::Arc::clone(&log),
            SessionStore::new(memory.sqlite().pool().clone()),
            zeph_common::SessionId::new("s1"),
        );
        // keep_recent=1 (make_condenser's fixed value): 3 messages condenses the first 2.
        sink.record_message(Role::User, "message zero", &[])
            .await
            .unwrap();
        sink.record_message(Role::User, "message one", &[])
            .await
            .unwrap();
        sink.record_message(Role::User, "message two", &[])
            .await
            .unwrap();
        drop(sink);
        drop(log);

        let condenser = make_condenser(&summary_json());
        let token_counter = WordCountTokenCounter;
        // 3 messages x 2 words = 6 tokens; window=10 => budget_used_fraction=0.6, over the 0.5
        // threshold `make_condenser` sets.
        let hydrated = hydrate_and_condense(
            dir.path(),
            &store,
            "s1",
            cid,
            &memory,
            None,
            &condenser,
            &token_counter,
            10,
        )
        .await
        .unwrap();

        assert_eq!(
            hydrated.messages.len(),
            3,
            "hydrate_and_condense must still return the full pre-condensation replay"
        );

        let meta = store.get("s1").await.unwrap().unwrap();
        assert_eq!(
            meta.last_condensed_seq, 1,
            "condensation must have fired and advanced the watermark through seq 1"
        );

        let all_events = hydrated.log.read_all().await.unwrap();
        assert_eq!(
            all_events.len(),
            4,
            "3 original message events + 1 Condensation event appended by hydrate_and_condense"
        );
        assert!(matches!(
            all_events.last().unwrap().kind,
            SessionEvent::Condensation { .. }
        ));
    }
}
