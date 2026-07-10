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
use zeph_session::{
    CompactionTier, SessionError, SessionEvent, SessionEventEnvelope, SessionEventLog, SessionStore,
};

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
    /// as-is (never overwritten by `content`) — this is what preserves `MessagePart::ToolUse`
    /// entries for `SessionEvent::AssistantMessage`.
    ///
    /// For `Role::User`, `parts` containing one or more [`MessagePart::ToolResult`] (the shape
    /// `process_tool_result_batch`/`persist_cancelled_tool_results` in
    /// `crates/zeph-core/src/agent/tool_execution/` always use for a tool-result turn) is recorded
    /// as one [`SessionEvent::ToolResult`] per part instead of being collapsed into a plain
    /// [`SessionEvent::UserMessage`]. This preserves the `tool_use_id` pairing
    /// [`crate::hydrate_from_event_log`]'s [`zeph_session::ReplayEngine::fold`] needs to
    /// reconstruct a valid `MessagePart::ToolUse`/`MessagePart::ToolResult` pair — both
    /// `zeph-llm`'s `OpenAI` and Claude serializers reject a `tool_calls` assistant message that
    /// is not immediately followed by matching tool-result messages. A plain user turn (no
    /// `ToolResult` parts) still becomes a single `SessionEvent::UserMessage`, as before.
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
        match role {
            Role::User => self.record_user_message(content, parts).await,
            Role::Assistant => {
                let parts = if parts.is_empty() && !content.is_empty() {
                    vec![MessagePart::Text {
                        text: content.to_owned(),
                    }]
                } else {
                    parts.to_vec()
                };
                self.append_and_advance(SessionEvent::AssistantMessage { parts })
                    .await
            }
            _ => Ok(()),
        }
    }

    /// Record a `Role::User` turn: one [`SessionEvent::ToolResult`] per `MessagePart::ToolResult`
    /// in `parts` when present, otherwise a single [`SessionEvent::UserMessage`] carrying `content`.
    /// See [`Self::record_message`] for why this split exists.
    async fn record_user_message(
        &self,
        content: &str,
        parts: &[MessagePart],
    ) -> Result<(), SessionError> {
        let mut last_envelope: Option<SessionEventEnvelope> = None;
        for part in parts {
            let MessagePart::ToolResult {
                tool_use_id,
                content: output,
                is_error,
            } = part
            else {
                continue;
            };
            let envelope = self
                .log
                .append(
                    None,
                    None,
                    SessionEvent::ToolResult {
                        id: tool_use_id.clone(),
                        // Not available at this call site — only the flat `MessagePart` reaches
                        // `record_message`, not the originating `ToolUseRequest`. Every consumer
                        // of `SessionEvent::ToolResult` (replay, ACP session updates) keys off
                        // `id`/`output`/`is_error`, never `name`.
                        name: String::new(),
                        output: output.clone(),
                        is_error: *is_error,
                        duration_ms: 0,
                    },
                )
                .await?;
            last_envelope = Some(envelope);
        }

        let envelope = match last_envelope {
            Some(envelope) => envelope,
            None => {
                self.log
                    .append(
                        None,
                        None,
                        SessionEvent::UserMessage {
                            text: content.to_owned(),
                            image_refs: Vec::new(),
                        },
                    )
                    .await?
            }
        };
        self.advance_seq(&envelope).await
    }

    /// Append `event` and advance `acp_sessions.last_seq`/`event_count` to match.
    async fn append_and_advance(&self, event: SessionEvent) -> Result<(), SessionError> {
        let envelope = self.log.append(None, None, event).await?;
        self.advance_seq(&envelope).await
    }

    /// Update `acp_sessions.last_seq`/`event_count` to reflect `envelope` as the latest appended
    /// event.
    async fn advance_seq(&self, envelope: &SessionEventEnvelope) -> Result<(), SessionError> {
        let event_count = envelope.seq + 1;
        self.store
            .update_seq(self.session_id.as_str(), envelope.seq, event_count)
            .await
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
        self.advance_seq(&envelope).await?;
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
    async fn record_user_message_with_tool_result_parts_emits_tool_result_events() {
        // Regression test for #5464: a Role::User call carrying MessagePart::ToolResult parts
        // (the shape process_tool_result_batch/persist_cancelled_tool_results always use) must
        // not collapse into a generic SessionEvent::UserMessage — that drops the tool_use_id
        // pairing ReplayEngine::fold needs and corrupts every subsequent turn on resume.
        let (sink, _dir) = make_sink("s1").await;
        let parts = vec![
            MessagePart::ToolResult {
                tool_use_id: "call_1".to_owned(),
                content: "file1.rs".to_owned(),
                is_error: false,
            },
            MessagePart::ToolResult {
                tool_use_id: "call_2".to_owned(),
                content: "boom".to_owned(),
                is_error: true,
            },
        ];
        sink.record_message(Role::User, "", &parts).await.unwrap();

        let events = sink.log.read_all().await.unwrap();
        assert_eq!(events.len(), 2);
        for event in &events {
            assert!(matches!(event.kind, SessionEvent::ToolResult { .. }));
        }
        let SessionEvent::ToolResult {
            id,
            output,
            is_error,
            ..
        } = &events[0].kind
        else {
            panic!("expected ToolResult");
        };
        assert_eq!(id, "call_1");
        assert_eq!(output, "file1.rs");
        assert!(!is_error);

        let meta = sink.store.get("s1").await.unwrap().unwrap();
        assert_eq!(meta.last_seq, 1);
        assert_eq!(meta.event_count, 2);
    }

    #[tokio::test]
    async fn record_user_message_without_tool_parts_still_emits_user_message() {
        let (sink, _dir) = make_sink("s1").await;
        sink.record_message(Role::User, "plain text", &[])
            .await
            .unwrap();

        let events = sink.log.read_all().await.unwrap();
        assert_eq!(events.len(), 1);
        let SessionEvent::UserMessage { text, .. } = &events[0].kind else {
            panic!("expected UserMessage");
        };
        assert_eq!(text, "plain text");
    }

    /// Shared setup for the round-trip regression tests below (#5464): records a
    /// User{prompt} -> Assistant{ToolUse} -> User{ToolResult} turn through
    /// `SessionSink::record_message` and folds it back via `ReplayEngine::fold`.
    async fn replayed_tool_call_messages() -> Vec<zeph_llm::provider::Message> {
        let (sink, _dir) = make_sink("s1").await;

        sink.record_message(Role::User, "run ls", &[])
            .await
            .unwrap();
        sink.record_message(
            Role::Assistant,
            "",
            &[MessagePart::ToolUse {
                id: "call_1".to_owned(),
                name: "shell".to_owned(),
                input: serde_json::json!({"command": "ls"}),
            }],
        )
        .await
        .unwrap();
        sink.record_message(
            Role::User,
            "",
            &[MessagePart::ToolResult {
                tool_use_id: "call_1".to_owned(),
                content: "file.txt".to_owned(),
                is_error: false,
            }],
        )
        .await
        .unwrap();

        let events = sink.log.read_all().await.unwrap();
        zeph_session::ReplayEngine::fold(events, None).messages
    }

    fn shell_tool_definition() -> [zeph_common::ToolDefinition; 1] {
        [zeph_common::ToolDefinition {
            name: zeph_common::ToolName::new("shell"),
            description: "Run a shell command".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
            output_schema: None,
        }]
    }

    /// Regression test for #5464: round-trips a tool-call turn through
    /// `SessionSink::record_message` -> `ReplayEngine::fold` -> the `OpenAI` serializer, and
    /// asserts the resulting `messages` array pairs the assistant `tool_calls` entry with an
    /// immediately-following `tool`-role message carrying a matching `tool_call_id` — the shape
    /// `OpenAI` rejects a request for when it is missing.
    #[tokio::test]
    async fn session_replay_tool_call_turn_produces_valid_openai_message_array() {
        use zeph_llm::openai::{OpenAiConfig, OpenAiProvider};
        use zeph_llm::provider::LlmProvider;

        let messages = replayed_tool_call_messages().await;
        let tools = shell_tool_definition();

        let provider = OpenAiProvider::new(OpenAiConfig {
            api_key: "sk-test".to_owned(),
            base_url: "https://api.openai.com/v1".to_owned(),
            model: "gpt-5.2".to_owned(),
            max_tokens: 4096,
            embedding_model: None,
            reasoning_effort: None,
            context_window: None,
            completion_tokens_param: None,
        });
        let request = provider.debug_request_json(&messages, &tools, false);

        let api_messages = request["messages"]
            .as_array()
            .expect("messages array present");
        let assistant_idx = api_messages
            .iter()
            .position(|m| m["role"] == "assistant" && m["tool_calls"].is_array())
            .expect("assistant tool_calls message present");
        assert_eq!(
            api_messages[assistant_idx]["tool_calls"][0]["id"], "call_1",
            "assistant tool_calls entry must carry the original tool_use_id"
        );

        let tool_msg = &api_messages[assistant_idx + 1];
        assert_eq!(
            tool_msg["role"], "tool",
            "assistant tool_calls message must be immediately followed by a tool-role message"
        );
        assert_eq!(
            tool_msg["tool_call_id"], "call_1",
            "tool-role message must carry the matching tool_call_id"
        );
        assert_eq!(tool_msg["content"], "file.txt");
    }

    /// Regression test for #5464: same round-trip as
    /// `session_replay_tool_call_turn_produces_valid_openai_message_array`, but through the
    /// Claude serializer. Also exercises `compute_matched_tool_ids`'s orphan guard
    /// (`crates/zeph-llm/src/claude/request.rs`): if the fold still produced a mismatched/merged
    /// shape, the guard would silently downgrade the `tool_use`/`tool_result` blocks to plain
    /// text instead of letting the API 400 — so asserting the blocks keep their native
    /// `tool_use`/`tool_result` type is what actually proves the fix here, not just that a
    /// `messages` array of the right length exists.
    #[tokio::test]
    async fn session_replay_tool_call_turn_produces_valid_claude_message_array() {
        use zeph_llm::claude::ClaudeProvider;
        use zeph_llm::provider::LlmProvider;

        let messages = replayed_tool_call_messages().await;
        let tools = shell_tool_definition();

        let provider =
            ClaudeProvider::new("sk-ant-test".to_owned(), "claude-sonnet-5".to_owned(), 4096);
        let request = provider.debug_request_json(&messages, &tools, false);

        let api_messages = request["messages"]
            .as_array()
            .expect("messages array present");
        let assistant_idx = api_messages
            .iter()
            .position(|m| {
                m["role"] == "assistant"
                    && m["content"]
                        .as_array()
                        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_use"))
            })
            .expect("assistant tool_use message present");
        let assistant_blocks = api_messages[assistant_idx]["content"]
            .as_array()
            .expect("assistant content is a block array");
        let tool_use_block = assistant_blocks
            .iter()
            .find(|b| b["type"] == "tool_use")
            .expect("tool_use block present (not downgraded to text by the orphan guard)");
        assert_eq!(tool_use_block["id"], "call_1");

        let tool_msg = &api_messages[assistant_idx + 1];
        assert_eq!(
            tool_msg["role"], "user",
            "assistant tool_use message must be immediately followed by a user message"
        );
        let tool_result_blocks = tool_msg["content"]
            .as_array()
            .expect("tool result content is a block array");
        let tool_result_block = tool_result_blocks
            .iter()
            .find(|b| b["type"] == "tool_result")
            .expect("tool_result block present (not downgraded to text by the orphan guard)");
        assert_eq!(tool_result_block["tool_use_id"], "call_1");
        assert_eq!(tool_result_block["content"], "file.txt");
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
