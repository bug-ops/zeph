// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ReplayEngine`]: deterministic fold of a session's event log into agent-ready messages.
//!
//! Replay never calls the LLM or a tool executor (spec §6.2, §15 NEVER) — it only folds
//! previously recorded events. This is the correctness guarantee behind AC-2 (byte-identical
//! replay) and the foundation `ForkEngine` (spec §7) builds on.

use std::path::Path;

use zeph_llm::provider::{Message, MessagePart, Role};

use crate::error::SessionError;
use crate::event::{SessionEvent, SessionEventEnvelope};
use crate::log::SessionEventLog;

/// The result of folding a session's event log up to some point.
#[derive(Debug, Clone, Default)]
pub struct ReconstructedState {
    /// Agent-ready message history, ready for hydration into `MessageState`.
    pub messages: Vec<Message>,
    /// The highest `seq` folded, or `None` if the log was empty.
    pub last_seq: Option<u64>,
    pub provider_name: String,
    pub model: String,
    pub cwd: String,
}

/// Folds a session's `events.jsonl` into a [`ReconstructedState`].
pub struct ReplayEngine;

impl ReplayEngine {
    /// Replay the session log at `session_dir`.
    ///
    /// `up_to`, if set, is an *exclusive* upper bound on `seq` (used by `ForkEngine` to replay
    /// only the prefix being copied). `None` replays the full log (resume).
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Io`] if the log cannot be opened/read.
    #[tracing::instrument(name = "session.replay.run", skip_all, level = "debug")]
    pub async fn replay(
        session_dir: &Path,
        up_to: Option<u64>,
    ) -> Result<ReconstructedState, SessionError> {
        let log = SessionEventLog::open(session_dir).await?;
        let events = log.read_all().await?;
        Ok(Self::fold(events, up_to))
    }

    /// Fold a sequence of envelopes already read from disk. Exposed separately from
    /// [`Self::replay`] so callers that already hold the events (e.g. a live `SessionActor`
    /// applying its own just-appended event) can fold incrementally without re-reading the file.
    #[must_use]
    #[tracing::instrument(
        name = "session.replay.fold",
        skip_all,
        level = "debug",
        fields(event_count = events.len())
    )]
    pub fn fold(events: Vec<SessionEventEnvelope>, up_to: Option<u64>) -> ReconstructedState {
        let mut messages: Vec<Message> = Vec::new();
        // Parallel to `messages`: the seq of the event that produced each message, so a later
        // `Condensation`/`Compaction` event can replace exactly the messages in its range.
        let mut origin_seqs: Vec<u64> = Vec::new();
        let mut state = ReconstructedState::default();

        for envelope in events {
            if let Some(bound) = up_to
                && envelope.seq >= bound
            {
                break;
            }
            let seq = envelope.seq;
            state.last_seq = Some(seq);

            match envelope.kind {
                SessionEvent::SessionStarted {
                    cwd,
                    provider_name,
                    model,
                    ..
                } => {
                    state.cwd = cwd;
                    state.provider_name = provider_name;
                    state.model = model;
                }
                SessionEvent::UserMessage { text, .. } => {
                    messages.push(Message::from_legacy(Role::User, text));
                    origin_seqs.push(seq);
                }
                SessionEvent::AssistantMessage { parts } => {
                    messages.push(Message::from_parts(Role::Assistant, parts));
                    origin_seqs.push(seq);
                }
                SessionEvent::ToolCall { id, name, input } => {
                    push_part_to_last_assistant(
                        &mut messages,
                        &mut origin_seqs,
                        seq,
                        MessagePart::ToolUse { id, name, input },
                    );
                }
                SessionEvent::ToolResult {
                    id,
                    output,
                    is_error,
                    ..
                } => {
                    push_part_to_last_assistant(
                        &mut messages,
                        &mut origin_seqs,
                        seq,
                        MessagePart::ToolResult {
                            tool_use_id: id,
                            content: output,
                            is_error,
                        },
                    );
                }
                SessionEvent::Condensation {
                    replaced_seq_range: (lo, hi),
                    summary,
                    ..
                } => {
                    replace_range(
                        &mut messages,
                        &mut origin_seqs,
                        lo,
                        hi,
                        summary.to_markdown(),
                    );
                }
                SessionEvent::Compaction { summary, .. } => {
                    // Compaction's schema (spec §4.3) does not carry an explicit
                    // `replaced_seq_range` the way `Condensation` does — it is emitted from the
                    // live in-memory compactor, which prunes by message count, not by logged
                    // seq. Until P2 wires real emission (zeph-agent-persistence) and settles the
                    // exact seq-range accounting, fold conservatively: a recorded summary
                    // replaces everything folded so far. No-op when `summary` is absent (a
                    // soft-tier prune that dropped raw tool output but produced no summary).
                    if let Some(summary) = summary {
                        let hi = origin_seqs.last().copied().unwrap_or(seq);
                        replace_range(
                            &mut messages,
                            &mut origin_seqs,
                            0,
                            hi,
                            summary.to_markdown(),
                        );
                    }
                }
                SessionEvent::ModelChanged {
                    provider_name,
                    model,
                } => {
                    state.provider_name = provider_name;
                    state.model = model;
                }
                SessionEvent::ForkPoint { .. } | SessionEvent::SessionEnded { .. } => {}
            }
        }

        state.messages = messages;
        state
    }
}

/// Append `part` to the last message if it is a pending `Assistant` message; otherwise start a
/// new one. Tool calls/results always follow the `AssistantMessage` that requested them within
/// the same turn, but the fold does not assume `AssistantMessage` was itself logged first (a
/// tool-only turn is valid).
fn push_part_to_last_assistant(
    messages: &mut Vec<Message>,
    origin_seqs: &mut Vec<u64>,
    seq: u64,
    part: MessagePart,
) {
    if let Some(last) = messages.last_mut()
        && last.role == Role::Assistant
    {
        last.parts.push(part);
        return;
    }
    messages.push(Message::from_parts(Role::Assistant, vec![part]));
    origin_seqs.push(seq);
}

/// Replace every message whose origin `seq` falls within `[lo, hi]` (inclusive) with a single
/// system summary message, preserving the position of the first replaced message.
fn replace_range(
    messages: &mut Vec<Message>,
    origin_seqs: &mut Vec<u64>,
    lo: u64,
    hi: u64,
    summary_text: String,
) {
    let mut new_messages = Vec::with_capacity(messages.len());
    let mut new_seqs = Vec::with_capacity(origin_seqs.len());
    let mut inserted = false;

    for (message, seq) in messages.drain(..).zip(origin_seqs.drain(..)) {
        if seq >= lo && seq <= hi {
            if !inserted {
                new_messages.push(Message::from_parts(
                    Role::System,
                    vec![MessagePart::Summary {
                        text: summary_text.clone(),
                    }],
                ));
                new_seqs.push(lo);
                inserted = true;
            }
            continue;
        }
        new_messages.push(message);
        new_seqs.push(seq);
    }

    if !inserted {
        new_messages.push(Message::from_parts(
            Role::System,
            vec![MessagePart::Summary { text: summary_text }],
        ));
        new_seqs.push(lo);
    }

    *messages = new_messages;
    *origin_seqs = new_seqs;
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_common::memory::AnchoredSummary;

    fn envelope(seq: u64, kind: SessionEvent) -> SessionEventEnvelope {
        SessionEventEnvelope::new(seq, None, None, kind)
    }

    #[tokio::test]
    async fn test_replay_empty_session() {
        let dir = tempfile::tempdir().unwrap();
        let state = ReplayEngine::replay(dir.path(), None).await.unwrap();
        assert!(state.messages.is_empty());
        assert!(state.last_seq.is_none());
    }

    #[tokio::test]
    async fn test_replay_basic_turn() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionEventLog::open(dir.path()).await.unwrap();
        log.append(
            None,
            None,
            SessionEvent::SessionStarted {
                session_id: "s1".to_owned(),
                cwd: "/repo".to_owned(),
                provider_name: "claude".to_owned(),
                model: "opus".to_owned(),
                forked_from: None,
            },
        )
        .await
        .unwrap();
        log.append(
            Some(1),
            None,
            SessionEvent::UserMessage {
                text: "hi".to_owned(),
                image_refs: vec![],
            },
        )
        .await
        .unwrap();
        log.append(
            Some(1),
            None,
            SessionEvent::AssistantMessage {
                parts: vec![MessagePart::Text {
                    text: "hello".to_owned(),
                }],
            },
        )
        .await
        .unwrap();

        let state = ReplayEngine::replay(dir.path(), None).await.unwrap();
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].role, Role::User);
        assert_eq!(state.messages[1].role, Role::Assistant);
        assert_eq!(state.provider_name, "claude");
        assert_eq!(state.cwd, "/repo");
        assert_eq!(state.last_seq, Some(2));
    }

    #[test]
    fn test_replay_tool_roundtrip() {
        let events = vec![
            envelope(
                0,
                SessionEvent::UserMessage {
                    text: "run ls".to_owned(),
                    image_refs: vec![],
                },
            ),
            envelope(
                1,
                SessionEvent::ToolCall {
                    id: "tc1".to_owned(),
                    name: "shell".to_owned(),
                    input: serde_json::json!({"cmd": "ls"}),
                },
            ),
            envelope(
                2,
                SessionEvent::ToolResult {
                    id: "tc1".to_owned(),
                    name: "shell".to_owned(),
                    output: "file.txt".to_owned(),
                    is_error: false,
                    duration_ms: 5,
                },
            ),
        ];
        let state = ReplayEngine::fold(events, None);
        assert_eq!(
            state.messages.len(),
            2,
            "user message + one assistant message holding both parts"
        );
        let assistant = &state.messages[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.parts.len(), 2);
        assert!(matches!(assistant.parts[0], MessagePart::ToolUse { .. }));
        assert!(matches!(assistant.parts[1], MessagePart::ToolResult { .. }));
    }

    #[test]
    fn test_replay_condensation_folds() {
        let summary = AnchoredSummary {
            session_intent: "test".to_owned(),
            files_modified: vec![],
            decisions_made: vec![],
            open_questions: vec![],
            next_steps: vec!["continue".to_owned()],
        };
        let events = vec![
            envelope(
                0,
                SessionEvent::UserMessage {
                    text: "a".to_owned(),
                    image_refs: vec![],
                },
            ),
            envelope(
                1,
                SessionEvent::AssistantMessage {
                    parts: vec![MessagePart::Text {
                        text: "b".to_owned(),
                    }],
                },
            ),
            envelope(
                2,
                SessionEvent::Condensation {
                    replaced_seq_range: (0, 1),
                    summary,
                    tokens_before: 100,
                    tokens_after: 10,
                },
            ),
            envelope(
                3,
                SessionEvent::UserMessage {
                    text: "c".to_owned(),
                    image_refs: vec![],
                },
            ),
        ];
        let state = ReplayEngine::fold(events, None);
        // The two condensed messages collapse into one summary message, followed by the new one.
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0].role, Role::System);
        assert!(matches!(
            state.messages[0].parts[0],
            MessagePart::Summary { .. }
        ));
        assert_eq!(state.messages[1].role, Role::User);
    }

    #[test]
    fn test_replay_stop_at_seq() {
        let events = vec![
            envelope(
                0,
                SessionEvent::UserMessage {
                    text: "a".to_owned(),
                    image_refs: vec![],
                },
            ),
            envelope(
                1,
                SessionEvent::UserMessage {
                    text: "b".to_owned(),
                    image_refs: vec![],
                },
            ),
            envelope(
                2,
                SessionEvent::UserMessage {
                    text: "c".to_owned(),
                    image_refs: vec![],
                },
            ),
        ];
        let state = ReplayEngine::fold(events, Some(2));
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.last_seq, Some(1));
    }
}
