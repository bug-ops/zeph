// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared session-open hydration pipeline (spec-068 §12.3/§13, architect ruling D-10).
//!
//! Before D-10, three call sites (ACP resume/load/fork, CLI `sessions resume`, `/conv resume`)
//! each open the event log and decide independently whether to run
//! [`crate::bootstrap_legacy_session`], fold it via [`zeph_session::ReplayEngine`], and reconcile
//! the `SQLite` projection. The CLI copy never did the fold/reconcile steps at all — a real
//! regression (impl-critic finding C1: `sessions resume` silently fell back to stale `SQLite`
//! history instead of the durable log). [`hydrate_from_event_log`] is the single pipeline every
//! session-open path must route through, so "which pipeline does resume use" cannot diverge
//! again.

use std::path::Path;
use std::sync::Arc;

use zeph_llm::provider::Message;
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::types::ConversationId;
use zeph_session::{ReplayEngine, SessionEventEnvelope, SessionEventLog, SessionStore};

use crate::error::PersistenceError;
use crate::legacy_bootstrap::bootstrap_legacy_session;
use crate::reconcile::reconcile_projection;

/// Result of [`hydrate_from_event_log`]: the replayed message history plus the opened log,
/// ready for the caller to wrap into a [`crate::SessionSink`] and/or feed to a builder.
pub struct Hydrated {
    /// Replayed message history — empty for a brand-new session or a legacy session with no
    /// recorded log yet (spec §18: legacy sessions are not retroactively synthesized here).
    pub messages: Vec<Message>,
    /// The full, unfolded event list read from the log — needed by
    /// [`crate::maybe_condense_on_resume`] (D-11), which condenses over a `seq` range rather
    /// than the already-folded `messages`. Empty for a brand-new or legacy-unbootstrapped
    /// session, matching `messages`.
    pub events: Vec<SessionEventEnvelope>,
    /// The session's opened durable event log. Returned so the caller doesn't have to reopen it
    /// to construct a [`crate::SessionSink`] — [`zeph_session::SessionEventLog`] has no
    /// multi-writer story (INV-D2), so callers must reuse this handle rather than opening a
    /// second one.
    pub log: Arc<SessionEventLog>,
}

/// Open `session_id`'s durable event log at `session_path`, run legacy bootstrap, fold it into
/// agent-ready messages, and reconcile the `SQLite` projection forward (INV-SP-3) — the single
/// pipeline every session-open path (ACP resume/load/fork, CLI `sessions resume`, `/conv
/// resume`/`fork`) routes through (spec-068 D-10).
///
/// `up_to` bounds the fold to events with `seq < up_to` (fork's partial-history case); pass
/// `None` for a full replay (the common resume/load case).
///
/// # Errors
///
/// Returns [`PersistenceError`] if the log can't be opened or read, legacy bootstrap fails, or
/// projection reconciliation fails.
///
/// # Examples
///
/// ```no_run
/// # async fn example(
/// #     session_path: &std::path::Path,
/// #     store: &zeph_session::SessionStore,
/// #     memory: &zeph_memory::semantic::SemanticMemory,
/// #     conversation_id: zeph_memory::types::ConversationId,
/// # ) -> Result<(), zeph_agent_persistence::PersistenceError> {
/// let hydrated = zeph_agent_persistence::hydrate_from_event_log(
///     session_path,
///     store,
///     "session-id",
///     conversation_id,
///     memory,
///     None,
/// )
/// .await?;
/// // hydrated.messages is ready for `AgentBuilder::with_preloaded_messages`;
/// // hydrated.log is ready for `SessionSink::new`.
/// # Ok(())
/// # }
/// ```
#[tracing::instrument(
    name = "persistence.hydrate.run",
    skip_all,
    level = "info",
    fields(session_id)
)]
pub async fn hydrate_from_event_log(
    session_path: &Path,
    store: &SessionStore,
    session_id: &str,
    conversation_id: ConversationId,
    memory: &SemanticMemory,
    up_to: Option<u64>,
) -> Result<Hydrated, PersistenceError> {
    let log = SessionEventLog::open(session_path).await?;

    // Legacy session lazy-bootstrap (spec §18): no-op unless this session predates durable
    // event-log persistence (has SQLite history but an empty log). Soft-fail like
    // `bootstrap_legacy_session`'s own doc contract: a failure here must not prevent the log
    // itself from being replayed and wrapped into a SessionSink below.
    if let Err(e) = bootstrap_legacy_session(&log, store, session_id, conversation_id, memory).await
    {
        tracing::warn!(error = %e, session_id, "legacy session lazy-bootstrap failed");
    }

    let events = log.read_all().await?;
    let messages = if events.is_empty() {
        Vec::new()
    } else {
        let state = ReplayEngine::fold(events.clone(), up_to);
        // INV-SP-3 (spec-068 §13): rebuild the SQLite `messages` projection forward from the log
        // when it trails the log's validated content (e.g. a crash between the JSONL append and
        // the SQLite write of the same turn). Not correctness-critical for `messages` (already
        // sourced from the log directly) — keeps other features that read `messages` directly
        // (semantic search, history displays) in sync. Soft-fail: a reconcile error must not
        // throw away the successfully-replayed `messages`.
        if let Err(e) = reconcile_projection(memory, conversation_id, session_id, &events).await {
            tracing::warn!(error = %e, session_id, "INV-SP-3 projection reconciliation failed");
        }
        state.messages
    };

    Ok(Hydrated {
        messages,
        events,
        log: Arc::new(log),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::provider::{MessagePart, Role};
    use zeph_session::SessionEvent;

    async fn make_memory() -> SemanticMemory {
        SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn empty_log_returns_no_messages() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let store = SessionStore::new(memory.sqlite().pool().clone());
        store.create("s1").await.unwrap();
        let dir = tempfile::tempdir().unwrap();

        let hydrated = hydrate_from_event_log(dir.path(), &store, "s1", cid, &memory, None)
            .await
            .unwrap();

        assert!(hydrated.messages.is_empty());
        assert!(hydrated.log.last_seq().is_none());
    }

    /// D-10's mandated end-to-end regression test: `SessionSink::record_message` (the real
    /// production write primitive `Agent::persist_message` calls, not a raw `log.append`) →
    /// simulate a crash in the INV-SP-1 window by never running the corresponding `SQLite`
    /// `messages` write → resume via `hydrate_from_event_log` → confirm both the replayed
    /// `messages` AND the reconciled `SQLite` projection reflect the durable log. Matches the
    /// #5419 lesson (green units + unexercised integration = this bug class) by going through
    /// `SessionSink` itself instead of directly appending to the log.
    #[tokio::test]
    async fn crash_after_session_sink_write_is_reconciled_on_resume() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let store = SessionStore::new(memory.sqlite().pool().clone());
        store.create("s1").await.unwrap();
        let dir = tempfile::tempdir().unwrap();

        // Simulates the live agent loop up to (and including) the durable log write, then a
        // crash before `PersistenceService::persist_message`'s SQLite write for the same turn
        // — the exact INV-SP-1 gap the reconcile mechanism exists to close.
        let log = Arc::new(SessionEventLog::open(dir.path()).await.unwrap());
        let sink = crate::SessionSink::new(
            Arc::clone(&log),
            SessionStore::new(memory.sqlite().pool().clone()),
            zeph_common::SessionId::new("s1"),
        );
        sink.record_message(Role::User, "hello", &[]).await.unwrap();
        sink.record_message(Role::Assistant, "hi there", &[])
            .await
            .unwrap();
        drop(sink);
        drop(log);

        // No SQLite messages exist yet for this conversation — confirms the crash was
        // faithfully simulated (only the log + acp_sessions bookkeeping advanced).
        let before = memory.sqlite().load_history(cid, 10).await.unwrap();
        assert!(before.is_empty(), "SQLite must be empty before reconcile");

        let hydrated = hydrate_from_event_log(dir.path(), &store, "s1", cid, &memory, None)
            .await
            .unwrap();

        assert_eq!(
            hydrated.messages.len(),
            2,
            "replay must recover both turns from the log"
        );

        let after = memory.sqlite().load_history(cid, 10).await.unwrap();
        assert_eq!(
            after.len(),
            2,
            "INV-SP-3 reconcile must rebuild the SQLite projection from the log on resume"
        );
    }

    #[tokio::test]
    async fn replays_events_and_reconciles_projection() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let store = SessionStore::new(memory.sqlite().pool().clone());
        store.create("s1").await.unwrap();
        let dir = tempfile::tempdir().unwrap();

        // Seed the log directly, as SessionSink would in production, WITHOUT updating
        // acp_sessions.event_count — simulating the exact crash-recovery gap INV-SP-3 covers.
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "hello".to_owned(),
                image_refs: Vec::new(),
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::AssistantMessage {
                parts: vec![MessagePart::Text {
                    text: "hi there".to_owned(),
                }],
            },
        )
        .await
        .unwrap();
        drop(log);

        let hydrated = hydrate_from_event_log(dir.path(), &store, "s1", cid, &memory, None)
            .await
            .unwrap();

        assert_eq!(hydrated.messages.len(), 2);
        assert_eq!(hydrated.messages[0].role, Role::User);
        assert_eq!(hydrated.messages[1].role, Role::Assistant);

        // INV-SP-3: the SQLite projection was reconciled forward even though this call never
        // wrote through SessionSink/PersistenceService.
        let history = memory.sqlite().load_history(cid, 10).await.unwrap();
        assert_eq!(history.len(), 2);
    }
}
