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
//!
//! The `messages` fold goes through [`zeph_session::ReplayEngine::replay`]'s bounded/chunked
//! reader (spec-068 §6.2, ≤ 100 envelopes in memory at once) rather than
//! [`zeph_session::ReplayEngine::fold`] on a cloned `Vec` (#5851) — before this fix,
//! `hydrate_from_event_log` was not among the sanctioned whole-file-`Vec` exceptions the spec's
//! §6.2 implementation note documents (`ForkEngine::fork`, `llm_condenser.rs`), so its
//! `fold(events.clone(), up_to)` call defeated #5844's memory-bounding work for every resume.
//! `events` itself is still fully materialized via [`zeph_session::SessionEventLog::read_all`],
//! independently of the bounded fold: [`crate::reconcile::reconcile_projection`] and
//! [`crate::maybe_condense_on_resume`] both need the full, owned event list rather than
//! `messages`, so this second full read is a deliberate tradeoff (one extra sequential I/O
//! pass, not a second in-memory copy) rather than a regression back to the pre-fix shape.

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
    // #5487 fix 3: this is the single shared choke point every writer-owning session-open
    // path (ACP resume, CLI `sessions resume`/default continuation, `/conv resume`, `zeph
    // serve` reactivation) routes through (D-10) — taking the exclusive advisory lock here
    // activates INV-D2 enforcement for all of them at once. Read-only tooling (`sessions
    // show`/`export`, the ACP HTTP inspection endpoint) does not call this function; it opens
    // the log directly via the still-lockless `SessionEventLog::open`.
    let log = SessionEventLog::open_exclusive(session_path).await?;

    // Legacy session lazy-bootstrap (spec §18): no-op unless this session predates durable
    // event-log persistence (has SQLite history but an empty log). Soft-fail like
    // `bootstrap_legacy_session`'s own doc contract: a failure here must not prevent the log
    // itself from being replayed and wrapped into a SessionSink below.
    if let Err(e) = bootstrap_legacy_session(&log, store, session_id, conversation_id, memory).await
    {
        tracing::warn!(error = %e, session_id, "legacy session lazy-bootstrap failed");
    }

    // #5851 fix: fold via the bounded/chunked `ReplayEngine::replay` path (spec-068 §6.2, ≤ 100
    // envelopes in memory at once) instead of `ReplayEngine::fold(events.clone(), up_to)`, which
    // doubled peak memory here — the clone plus the `events` Vec below were both live at once.
    // `replay` re-opens the log lockless (`SessionEventLog::open`, no `flock`), so it never
    // contends with the exclusive-locked `log` handle already held above; this trades one extra
    // sequential file read for eliminating the clone, which is the right tradeoff for NFR-P3
    // (latency-bound, not read-count-bound).
    let state = ReplayEngine::replay(session_path, up_to).await?;
    // `events` is still fully materialized here — unlike `messages`, it cannot be produced from
    // the bounded chunked path: `reconcile_projection` below and `Hydrated::events` (consumed by
    // `crate::maybe_condense_on_resume`) both need the full, owned event list, not just the
    // folded messages.
    let events = log.read_all().await?;
    let messages = if events.is_empty() {
        Vec::new()
    } else {
        // INV-SP-3 (spec-068 §13): rebuild the SQLite `messages` projection forward from the log
        // when it trails the log's validated content (e.g. a crash between the JSONL append and
        // the SQLite write of the same turn). Not correctness-critical for `messages` (already
        // sourced from the log directly) — keeps other features that read `messages` directly
        // (semantic search, history displays) in sync. Soft-fail: a reconcile error must not
        // throw away the successfully-replayed `messages`.
        if let Err(e) = reconcile_projection(memory, conversation_id, session_id, &events).await {
            tracing::warn!(error = %e, session_id, "INV-SP-3 projection reconciliation failed");
        }
        let mut messages = state.messages;
        // Messages folded here never carry `metadata.db_id` (populated only when loading from
        // SQLite) — replay is in-memory-only and the durable log itself is not mutated, so any
        // ids returned by `sanitize_tool_pairs` are safe to drop rather than soft-delete.
        let (removed, _db_ids) = crate::sanitize::sanitize_tool_pairs(&mut messages);
        if removed > 0 {
            tracing::warn!(
                session_id,
                removed,
                "sanitized orphaned tool_use/tool_result messages on hydrate"
            );
        }
        messages
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

    /// #5851 regression: `up_to` must still bound `messages` when the fold goes through the
    /// bounded/chunked `ReplayEngine::replay` path (post-fix) exactly as it did through
    /// `ReplayEngine::fold(events.clone(), up_to)` (pre-fix) — the two prior tests in this module
    /// only ever exercise `up_to = None`. Also pins down the pre-existing (unchanged by #5851)
    /// asymmetry that `Hydrated::events` is always the *full*, unbounded log — only `messages` is
    /// `up_to`-bounded — since `reconcile_projection`/`maybe_condense_on_resume` need the whole
    /// event list regardless of the fold's fork-partial-history cutoff.
    #[tokio::test]
    async fn up_to_bounds_messages_but_not_events() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let store = SessionStore::new(memory.sqlite().pool().clone());
        store.create("s1").await.unwrap();
        let dir = tempfile::tempdir().unwrap();

        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "first".to_owned(),
                image_refs: Vec::new(),
            },
        )
        .await
        .unwrap(); // seq 0
        log.append(
            None,
            None,
            SessionEvent::AssistantMessage {
                parts: vec![MessagePart::Text {
                    text: "first reply".to_owned(),
                }],
            },
        )
        .await
        .unwrap(); // seq 1
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "second".to_owned(),
                image_refs: Vec::new(),
            },
        )
        .await
        .unwrap(); // seq 2
        log.append(
            None,
            None,
            SessionEvent::AssistantMessage {
                parts: vec![MessagePart::Text {
                    text: "second reply".to_owned(),
                }],
            },
        )
        .await
        .unwrap(); // seq 3
        drop(log);

        // up_to = 2 excludes events with seq >= 2, so only the first turn (seq 0, 1) is folded.
        let hydrated = hydrate_from_event_log(dir.path(), &store, "s1", cid, &memory, Some(2))
            .await
            .unwrap();

        assert_eq!(
            hydrated.messages.len(),
            2,
            "up_to must bound messages to the first turn only, through the bounded replay path"
        );
        assert_eq!(hydrated.messages[0].role, Role::User);
        assert_eq!(hydrated.messages[1].role, Role::Assistant);

        assert_eq!(
            hydrated.events.len(),
            4,
            "Hydrated::events is always the full, unbounded log — up_to only bounds the fold, \
             matching pre-#5851 behavior (reconcile_projection/maybe_condense_on_resume need the \
             whole event list)"
        );
    }

    /// #5646 regression: a session killed mid-tool-call (no `ToolResult` event ever appended)
    /// must have its orphaned assistant `ToolUse` sanitized away on the very next resume,
    /// instead of being replayed verbatim into a live agent — which previously produced a
    /// `tool_calls` message with no matching `tool` response and a 400 from `OpenAI`.
    #[tokio::test]
    async fn orphaned_tool_use_from_killed_session_is_sanitized_on_resume() {
        use zeph_llm::provider::{MessagePart, Role};
        use zeph_session::SessionEvent;

        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let store = SessionStore::new(memory.sqlite().pool().clone());
        store.create("s1").await.unwrap();
        let dir = tempfile::tempdir().unwrap();

        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "run a slow tool".to_owned(),
                image_refs: Vec::new(),
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::AssistantMessage {
                parts: vec![MessagePart::ToolUse {
                    id: "call_1".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::json!({}),
                }],
            },
        )
        .await
        .unwrap();
        // No ToolResult event: simulates the process being killed mid-tool-call.
        drop(log);

        let hydrated = hydrate_from_event_log(dir.path(), &store, "s1", cid, &memory, None)
            .await
            .unwrap();

        assert_eq!(
            hydrated.messages.len(),
            1,
            "the orphaned assistant ToolUse message must be stripped, leaving only the user turn"
        );
        assert_eq!(hydrated.messages[0].role, Role::User);
        assert!(
            hydrated
                .messages
                .iter()
                .flat_map(|m| m.parts.iter())
                .all(|p| !matches!(p, MessagePart::ToolUse { .. })),
            "no ToolUse part may survive into the replayed history unpaired"
        );
    }
}
