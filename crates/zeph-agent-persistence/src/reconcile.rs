// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! INV-SP-3: projection reconcile-from-log on open (spec-068 §13, #5343).
//!
//! When a session is opened (resume, load, fork) and the `SQLite` `messages` projection trails
//! the durable JSONL event log's validated content, [`reconcile_projection`] rebuilds the missing
//! rows forward from the log.

use zeph_memory::semantic::SemanticMemory;
use zeph_memory::types::ConversationId;
use zeph_session::{SessionEvent, SessionEventEnvelope};

use crate::error::PersistenceError;

/// Rebuild the `SQLite` `messages` projection forward from the event log when it trails the
/// log's validated content.
///
/// The watermark is the real `SQLite` `messages` row count for `conversation_id`
/// ([`zeph_memory::store::SqliteStore::count_messages`]), **not** `acp_sessions.event_count`:
/// [`crate::SessionSink::record_message`] advances `event_count` immediately after the durable
/// log append, deliberately *before* the caller's separate `SQLite` `messages` write runs
/// (INV-SP-1's log-first ordering) — so `event_count` races ahead of `SQLite` on every turn, not
/// just a crashed one, and can never observe the gap this function exists to close. Reading the
/// actual row count is the only watermark that reflects what `SQLite` has really committed.
///
/// Deliberately conservative: only reconciles a gap that contains **exclusively**
/// `UserMessage`/`AssistantMessage` events from the first un-committed message onward — if a
/// `ToolCall`/`ToolResult`/`Condensation`/`Compaction`/`ForkPoint` event appears anywhere at or
/// after that point, the whole reconcile is skipped rather than guessed at. An
/// incorrectly-reconstructed row in the `messages` table is worse than a stale projection, and
/// the resume/load/fork hydration path itself does not depend on this projection being current
/// (it sources conversation history from the JSONL log directly when the log has content — see
/// [`crate::hydrate_from_event_log`]).
///
/// `events` must already be the full, INV-SP-2-validated event list for the session (as returned
/// by [`zeph_session::SessionEventLog::read_all`]).
///
/// # Errors
///
/// Returns an error if a `SQLite` read or write fails. Rows written before a mid-loop failure
/// are not rolled back — matches [`crate::service::PersistenceService::load_history`]'s existing
/// non-transactional pattern for the same table.
#[tracing::instrument(
    name = "persistence.reconcile.run",
    skip_all,
    level = "debug",
    fields(session_id, event_count = events.len())
)]
pub async fn reconcile_projection(
    memory: &SemanticMemory,
    conversation_id: ConversationId,
    session_id: &str,
    events: &[SessionEventEnvelope],
) -> Result<(), PersistenceError> {
    let is_message = |kind: &SessionEvent| {
        matches!(
            kind,
            SessionEvent::UserMessage { .. } | SessionEvent::AssistantMessage { .. }
        )
    };

    let committed =
        usize::try_from(memory.sqlite().count_messages(conversation_id).await?).unwrap_or(0);

    let message_events: Vec<&SessionEventEnvelope> =
        events.iter().filter(|e| is_message(&e.kind)).collect();
    if message_events.len() <= committed {
        return Ok(());
    }
    let missing = &message_events[committed..];

    // Bail if any non-message event appears at or after the first un-committed message's
    // position — a mixed gap (tool call, condensation, fork, ...) is too complex to safely
    // auto-reconcile from message events alone.
    let first_missing_seq = missing[0].seq;
    if events
        .iter()
        .any(|e| e.seq >= first_missing_seq && !is_message(&e.kind))
    {
        tracing::warn!(
            session_id,
            gap = missing.len(),
            "INV-SP-3 reconciliation skipped: gap contains a tool call, condensation, or fork \
             event — too complex to auto-reconcile safely; leaving the SQLite projection stale"
        );
        return Ok(());
    }

    for envelope in missing {
        match &envelope.kind {
            SessionEvent::UserMessage { text, .. } => {
                memory
                    .sqlite()
                    .save_message(conversation_id, "user", text)
                    .await?;
            }
            SessionEvent::AssistantMessage { parts } => {
                let flattened = zeph_llm::provider::Message::from_parts(
                    zeph_llm::provider::Role::Assistant,
                    parts.clone(),
                );
                let parts_json = serde_json::to_string(parts)?;
                memory
                    .sqlite()
                    .save_message_with_parts(
                        conversation_id,
                        "assistant",
                        &flattened.content,
                        &parts_json,
                    )
                    .await?;
            }
            // SessionStarted/ModelChanged/SessionEnded carry no `messages` row; excluded from
            // `message_events` above so they never appear here.
            _ => {}
        }
    }

    // acp_sessions.event_count/last_seq already reflect the full log (SessionSink keeps them
    // current on every append, ahead of SQLite by design) — nothing to update here; this
    // function's only job is catching SQLite up to what the log and acp_sessions already agree
    // on.

    tracing::info!(
        session_id,
        reconciled = missing.len(),
        "INV-SP-3: SQLite projection reconciled forward from the session event log"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::provider::MessagePart;
    use zeph_session::SessionEventLog;

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
    async fn reconciles_trailing_plain_messages() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "hello".to_owned(),
                image_refs: vec![],
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
        let events = log.read_all().await.unwrap();

        // Simulates the INV-SP-1 crash window: the log has 2 events, but SQLite `messages` has
        // none yet (a real `SessionSink` would have already bumped `acp_sessions.event_count`
        // to 2 at this point too — irrelevant here since reconcile no longer reads that column).
        reconcile_projection(&memory, cid, "s1", &events)
            .await
            .unwrap();

        let history = memory
            .sqlite()
            .load_history_filtered(cid, 50, Some(true), None)
            .await
            .unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "hello");
        assert_eq!(history[1].content, "hi there");
    }

    #[tokio::test]
    async fn skips_when_no_gap() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "hello".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        let events = log.read_all().await.unwrap();

        // SQLite already has the one message the log records — nothing to reconcile.
        memory
            .sqlite()
            .save_message(cid, "user", "hello")
            .await
            .unwrap();

        reconcile_projection(&memory, cid, "s1", &events)
            .await
            .unwrap();

        // No duplicate row was written.
        let history = memory
            .sqlite()
            .load_history_filtered(cid, 50, Some(true), None)
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
    }

    #[tokio::test]
    async fn skips_gap_containing_a_tool_call() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "run ls".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::ToolCall {
                id: "t1".to_owned(),
                name: "shell".to_owned(),
                input: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
        let events = log.read_all().await.unwrap();

        reconcile_projection(&memory, cid, "s1", &events)
            .await
            .unwrap();

        // Bailed out safely: no rows written.
        let history = memory
            .sqlite()
            .load_history_filtered(cid, 50, Some(true), None)
            .await
            .unwrap();
        assert!(history.is_empty());
    }

    /// A tool call that precedes the un-committed message gap must not block reconciliation —
    /// only complex events AT OR AFTER the first missing message matter.
    #[tokio::test]
    async fn tool_call_before_the_gap_does_not_block_reconciliation() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "run ls".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::ToolCall {
                id: "t1".to_owned(),
                name: "shell".to_owned(),
                input: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::AssistantMessage {
                parts: vec![MessagePart::Text {
                    text: "done".to_owned(),
                }],
            },
        )
        .await
        .unwrap();
        let events = log.read_all().await.unwrap();

        // Only the first message ("run ls") is already in SQLite; the tool call has no
        // `messages` row and sits entirely before the un-committed "done" message.
        memory
            .sqlite()
            .save_message(cid, "user", "run ls")
            .await
            .unwrap();

        reconcile_projection(&memory, cid, "s1", &events)
            .await
            .unwrap();

        let history = memory
            .sqlite()
            .load_history_filtered(cid, 50, Some(true), None)
            .await
            .unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, "done");
    }
}
