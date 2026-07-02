// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Legacy session lazy-bootstrap (spec-068 §18, #5343, P4).
//!
//! Existing installs upgrading to #5343 have `messages` rows (the `SQLite` projection) for
//! conversations that predate durable event-log persistence. Event logs are **not**
//! retroactively synthesized from that projection — lossy, since no tool-call/result granularity
//! survives in the projection. Instead, on the first resume of such a session, this writes a
//! [`SessionEvent::SessionStarted`] header plus a single [`SessionEvent::Condensation`]-style
//! "imported history" event summarizing the pre-existing history, after which new turns append
//! to the log normally. Old sessions cannot be forked or replayed at an arbitrary historical
//! `seq` predating this import boundary — documented in `sessions show --events` output.

use zeph_common::memory::AnchoredSummary;
use zeph_memory::ConversationId;
use zeph_memory::semantic::SemanticMemory;
use zeph_session::{SessionEvent, SessionEventLog, SessionStore};

use crate::error::PersistenceError;

/// Bootstraps a legacy session's durable event log if it doesn't have one yet and has
/// pre-existing `SQLite` message history for `conversation_id`.
///
/// No-op (returns `Ok(())` without writing anything) when:
/// - the session's event log already has events (`log.last_seq().is_some()`) — already
///   bootstrapped, or a session that was always #5343-native.
/// - the linked conversation has zero `SQLite` messages — a genuinely new session, not a legacy
///   one; the caller's own [`crate::SessionSink`] writes its own `SessionStarted` on the first
///   real turn instead.
///
/// # Errors
///
/// Returns [`PersistenceError`] if the message count query, log append, or [`SessionStore`]
/// update fails.
#[tracing::instrument(
    name = "persistence.legacy_bootstrap.run",
    skip_all,
    level = "debug",
    fields(session_id)
)]
pub async fn bootstrap_legacy_session(
    log: &SessionEventLog,
    store: &SessionStore,
    session_id: &str,
    conversation_id: ConversationId,
    memory: &SemanticMemory,
) -> Result<(), PersistenceError> {
    if log.last_seq().is_some() {
        return Ok(());
    }

    let message_count = memory.sqlite().count_messages(conversation_id).await?;
    if message_count <= 0 {
        return Ok(());
    }

    log.append(
        None,
        None,
        SessionEvent::SessionStarted {
            session_id: session_id.to_owned(),
            cwd: String::new(),
            provider_name: String::new(),
            model: String::new(),
            forked_from: None,
        },
    )
    .await?;

    let summary = AnchoredSummary {
        session_intent: format!(
            "Imported from pre-existing conversation history ({message_count} message(s)) that \
             predates durable session-log persistence (spec-068, #5343). Granular tool-call/\
             result replay is not available for this range — only the SQLite projection existed."
        ),
        files_modified: Vec::new(),
        decisions_made: Vec::new(),
        open_questions: Vec::new(),
        next_steps: Vec::new(),
    };
    log.append(
        None,
        None,
        SessionEvent::Condensation {
            replaced_seq_range: (0, 0),
            summary,
            tokens_before: 0,
            tokens_after: 0,
        },
    )
    .await?;

    let last_seq = log.last_seq().unwrap_or(1);
    store.update_seq(session_id, last_seq, last_seq + 1).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::any::AnyProvider;

    async fn make_env() -> (SemanticMemory, SessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap();
        let store = SessionStore::new(memory.sqlite().pool().clone());
        (memory, store, dir)
    }

    #[tokio::test]
    async fn noop_when_log_already_has_events() {
        let (memory, store, dir) = make_env().await;
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::UserMessage {
                text: "hi".to_owned(),
                image_refs: Vec::new(),
            },
        )
        .await
        .unwrap();
        store.create("s1").await.unwrap();

        let cid = memory.sqlite().create_conversation().await.unwrap();
        bootstrap_legacy_session(&log, &store, "s1", cid, &memory)
            .await
            .unwrap();

        // Still just the one UserMessage — no bootstrap events appended.
        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn noop_when_conversation_has_no_messages() {
        let (memory, store, dir) = make_env().await;
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        store.create("s1").await.unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();

        bootstrap_legacy_session(&log, &store, "s1", cid, &memory)
            .await
            .unwrap();

        assert!(log.read_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn bootstraps_when_legacy_messages_exist() {
        let (memory, store, dir) = make_env().await;
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        store.create("s1").await.unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        memory
            .sqlite()
            .save_message(cid, "user", "legacy message")
            .await
            .unwrap();

        bootstrap_legacy_session(&log, &store, "s1", cid, &memory)
            .await
            .unwrap();

        let events = log.read_all().await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].kind,
            SessionEvent::SessionStarted { .. }
        ));
        let SessionEvent::Condensation {
            replaced_seq_range,
            ref summary,
            ..
        } = events[1].kind
        else {
            panic!("expected Condensation");
        };
        assert_eq!(replaced_seq_range, (0, 0));
        assert!(summary.session_intent.contains("Imported"));

        let meta = store.get("s1").await.unwrap().unwrap();
        assert_eq!(meta.event_count, 2);

        // Idempotent: calling again is a no-op since the log now has events.
        bootstrap_legacy_session(&log, &store, "s1", cid, &memory)
            .await
            .unwrap();
        assert_eq!(log.read_all().await.unwrap().len(), 2);
    }
}
