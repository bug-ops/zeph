// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`SessionSink`]: dual-write of turn messages into the durable session event log.
//!
//! Implements INV-SP-1 (spec-068 §13): a turn's `SessionEvent`s must be appended to
//! `events.jsonl` and durably flushed **before** the `SQLite` `messages` projection or
//! `acp_sessions.last_seq` are updated — the projection must never lead the log. Callers
//! (`zeph-core`'s `Agent::persist_message` shim) MUST invoke [`SessionSink::record_message`]
//! before calling `PersistenceService::persist_message`, not after.

use std::sync::Arc;

use zeph_common::SessionId;
use zeph_llm::provider::{MessagePart, Role};
use zeph_session::{CompactionTier, SessionError, SessionEvent, SessionEventLog, SessionStore};

/// Dual-writes turn messages to the durable JSONL event log ahead of the `SQLite` projection.
///
/// One `SessionSink` is constructed per live conversation-session and held for its lifetime
/// (mirrors `zeph_session`'s single-writer precondition, INV-D2 — only one `SessionSink` may
/// hold a given session's [`SessionEventLog`] at a time).
pub struct SessionSink {
    log: Arc<SessionEventLog>,
    store: SessionStore,
    session_id: SessionId,
}

impl SessionSink {
    /// Wrap an already-opened [`SessionEventLog`] and [`SessionStore`] for `session_id`.
    #[must_use]
    pub fn new(log: Arc<SessionEventLog>, store: SessionStore, session_id: SessionId) -> Self {
        Self {
            log,
            store,
            session_id,
        }
    }

    /// The session this sink writes to.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Record one persisted message as a `SessionEvent`, then update `acp_sessions.last_seq`.
    ///
    /// `role == Role::System` (and any future non-exhaustive `Role` variant) is a no-op:
    /// system-role `persist_message` calls carry live in-memory compaction summaries, which map
    /// to the `Compaction` event emitted separately by the compaction hook (spec §8.1), not to a
    /// generic system message — there is no `SessionEvent` variant for an untyped system message.
    ///
    /// For `Role::Assistant`, the real production call sites
    /// (`Agent::persist_message` from `crates/zeph-core/src/agent/tool_execution/tier_loop.rs`)
    /// always pass the response text via `content` and an empty `parts` slice — `parts` is only
    /// populated in a handful of tests that construct it explicitly. When `parts` is empty and
    /// `content` is non-empty, `content` is wrapped into a single [`MessagePart::Text`] so the
    /// durable log captures the real response either way; an explicitly provided `parts` is used
    /// as-is (never overwritten by `content`).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] if the log append or the `last_seq` update fails. Callers should
    /// log and continue rather than fail the turn — a dropped session-log write only means that
    /// turn is absent from replay, not that agent state is corrupted (`SQLite` projection write
    /// still proceeds independently after this call in the caller's turn-persistence path).
    #[tracing::instrument(
        name = "persistence.session_sink.record_message",
        skip_all,
        level = "debug"
    )]
    pub async fn record_message(
        &self,
        role: Role,
        content: &str,
        parts: &[MessagePart],
    ) -> Result<(), SessionError> {
        let event = match role {
            Role::User => SessionEvent::UserMessage {
                text: content.to_owned(),
                image_refs: Vec::new(),
            },
            Role::Assistant => {
                let parts = if parts.is_empty() && !content.is_empty() {
                    vec![MessagePart::Text {
                        text: content.to_owned(),
                    }]
                } else {
                    parts.to_vec()
                };
                SessionEvent::AssistantMessage { parts }
            }
            _ => return Ok(()),
        };

        let envelope = self.log.append(None, None, event).await?;
        let event_count = envelope.seq + 1;
        self.store
            .update_seq(self.session_id.as_str(), envelope.seq, event_count)
            .await?;
        Ok(())
    }

    /// Record that live in-memory compaction fired (spec §8.1), making it replayable.
    ///
    /// Unlike [`crate::reconcile_projection`]'s `Condensation` event (event-log-driven, carries a
    /// precise `replaced_seq_range`), `Compaction` reflects a live, in-memory prune the agent
    /// already applied to `MessageState.messages` — its schema has no `replaced_seq_range`
    /// ([`zeph_session::replay::ReplayEngine::fold`] folds it conservatively: a recorded summary
    /// replaces everything folded so far). `summary` is `None` for now — the LLM-produced
    /// hard-compaction summary text is not yet surfaced from `zeph-agent-context`'s internal
    /// state to this call site; `tier`/`cleared_count` alone are enough to make replay aware a
    /// compaction happened.
    ///
    /// A no-op when `cleared_count == 0` (nothing was actually pruned this turn).
    ///
    /// M2 (spec §8.3, `INV-SP-4`): also advances `acp_sessions.last_condensed_seq` to this
    /// event's `seq` — live compaction and durable condensation
    /// ([`zeph_agent_persistence::maybe_condense_on_resume`](crate::maybe_condense_on_resume))
    /// share the same non-overlap ledger (`zeph_session::condenser::validate_non_overlap`), so a
    /// later resume-condensation cannot re-summarize a range compaction already pruned live.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] if the log append or either `SessionStore` update fails. Callers
    /// should log and continue rather than fail the turn, matching [`Self::record_message`].
    #[tracing::instrument(
        name = "persistence.session_sink.record_compaction",
        skip_all,
        level = "debug",
        fields(cleared_count)
    )]
    pub async fn record_compaction(
        &self,
        tier: CompactionTier,
        cleared_count: u32,
    ) -> Result<(), SessionError> {
        if cleared_count == 0 {
            return Ok(());
        }
        let event = SessionEvent::Compaction {
            tier,
            cleared_count,
            summary: None,
        };
        let envelope = self.log.append(None, None, event).await?;
        let event_count = envelope.seq + 1;
        self.store
            .update_seq(self.session_id.as_str(), envelope.seq, event_count)
            .await?;
        self.store
            .set_condensed_seq(self.session_id.as_str(), envelope.seq)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_sink(session_id: &str) -> (SessionSink, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(SessionEventLog::open(dir.path()).await.unwrap());

        let config = zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        };
        let pool = config.connect().await.unwrap();
        zeph_db::run_migrations(&pool).await.unwrap();
        let store = SessionStore::new(pool);
        store.create(session_id).await.unwrap();

        let sink = SessionSink::new(log, store, SessionId::new(session_id));
        (sink, dir)
    }

    #[tokio::test]
    async fn record_user_message_appends_event_and_updates_seq() {
        let (sink, _dir) = make_sink("s1").await;
        sink.record_message(Role::User, "hello", &[]).await.unwrap();

        let meta = sink.store.get("s1").await.unwrap().unwrap();
        assert_eq!(meta.last_seq, 0);
        assert_eq!(meta.event_count, 1);

        let events = sink.log.read_all().await.unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, SessionEvent::UserMessage { .. }));
    }

    #[tokio::test]
    async fn record_assistant_message_carries_parts() {
        let (sink, _dir) = make_sink("s1").await;
        let parts = vec![MessagePart::Text {
            text: "hi there".to_owned(),
        }];
        sink.record_message(Role::Assistant, "hi there", &parts)
            .await
            .unwrap();

        let events = sink.log.read_all().await.unwrap();
        assert_eq!(events.len(), 1);
        let SessionEvent::AssistantMessage { parts: got } = &events[0].kind else {
            panic!("expected AssistantMessage");
        };
        assert_eq!(got.len(), 1);
    }

    #[tokio::test]
    async fn record_assistant_message_wraps_content_when_parts_empty() {
        // Mirrors the real production call shape (`Agent::persist_message` from
        // `crates/zeph-core/src/agent/tool_execution/tier_loop.rs`): content set, parts empty —
        // not `record_assistant_message_carries_parts`'s shape above, which only a handful of
        // tests use. Regression test for #5419.
        let (sink, _dir) = make_sink("s1").await;
        sink.record_message(Role::Assistant, "the real response text", &[])
            .await
            .unwrap();

        let events = sink.log.read_all().await.unwrap();
        assert_eq!(events.len(), 1);
        let SessionEvent::AssistantMessage { parts: got } = &events[0].kind else {
            panic!("expected AssistantMessage");
        };
        assert_eq!(got.len(), 1);
        let MessagePart::Text { text } = &got[0] else {
            panic!("expected MessagePart::Text");
        };
        assert_eq!(text, "the real response text");
    }

    #[tokio::test]
    async fn record_assistant_message_prefers_explicit_parts_over_content() {
        // When parts is explicitly non-empty, content must not be used to overwrite it.
        let (sink, _dir) = make_sink("s1").await;
        let parts = vec![MessagePart::Text {
            text: "explicit part".to_owned(),
        }];
        sink.record_message(Role::Assistant, "ignored content", &parts)
            .await
            .unwrap();

        let events = sink.log.read_all().await.unwrap();
        let SessionEvent::AssistantMessage { parts: got } = &events[0].kind else {
            panic!("expected AssistantMessage");
        };
        assert_eq!(got.len(), 1);
        let MessagePart::Text { text } = &got[0] else {
            panic!("expected MessagePart::Text");
        };
        assert_eq!(text, "explicit part");
    }

    #[tokio::test]
    async fn record_system_message_is_noop() {
        let (sink, _dir) = make_sink("s1").await;
        sink.record_message(Role::System, "compaction summary", &[])
            .await
            .unwrap();

        let events = sink.log.read_all().await.unwrap();
        assert!(events.is_empty());
        let meta = sink.store.get("s1").await.unwrap().unwrap();
        assert_eq!(meta.event_count, 0);
    }

    #[tokio::test]
    async fn sequential_messages_increment_seq() {
        let (sink, _dir) = make_sink("s1").await;
        sink.record_message(Role::User, "a", &[]).await.unwrap();
        sink.record_message(Role::Assistant, "b", &[])
            .await
            .unwrap();

        let meta = sink.store.get("s1").await.unwrap().unwrap();
        assert_eq!(meta.last_seq, 1);
        assert_eq!(meta.event_count, 2);
    }

    #[tokio::test]
    async fn record_compaction_appends_event_and_updates_seq() {
        let (sink, _dir) = make_sink("s1").await;
        sink.record_compaction(CompactionTier::Hard, 6)
            .await
            .unwrap();

        let events = sink.log.read_all().await.unwrap();
        assert_eq!(events.len(), 1);
        let SessionEvent::Compaction {
            tier,
            cleared_count,
            summary,
        } = &events[0].kind
        else {
            panic!("expected Compaction");
        };
        assert_eq!(*tier, CompactionTier::Hard);
        assert_eq!(*cleared_count, 6);
        assert!(summary.is_none());

        let meta = sink.store.get("s1").await.unwrap().unwrap();
        assert_eq!(meta.event_count, 1);
        // M2 (spec §8.3 INV-SP-4): live compaction advances the same non-overlap ledger
        // resume-time condensation reads, so a later condensation cannot overlap this range.
        assert_eq!(meta.last_condensed_seq, 0);
    }

    #[tokio::test]
    async fn record_compaction_zero_cleared_is_noop() {
        let (sink, _dir) = make_sink("s1").await;
        sink.record_compaction(CompactionTier::Soft, 0)
            .await
            .unwrap();

        let events = sink.log.read_all().await.unwrap();
        assert!(events.is_empty());
    }
}
