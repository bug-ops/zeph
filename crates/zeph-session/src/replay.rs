// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ReplayEngine`]: deterministic fold of a session's event log into agent-ready messages.
//!
//! Replay never calls the LLM or a tool executor (spec §6.2, §15 NEVER) — it only folds
//! previously recorded events. This is the correctness guarantee behind AC-2 (byte-identical
//! replay) and the foundation `ForkEngine` (spec §7) builds on.

use std::ops::ControlFlow;
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
    /// Reads the log in bounded chunks (spec §6.2 step 3: ≤ 100 raw envelopes in memory at
    /// once) rather than materializing the whole file's parsed events into one `Vec` first —
    /// unlike [`Self::fold`], which operates on an already-materialized `Vec` for callers that
    /// already hold the events in memory (e.g. `llm_condenser.rs`, which folds an
    /// already-sliced sub-`Vec`). `ForkEngine::fork` copies raw events via
    /// `SessionEventLog::read_all` directly and calls this method (not `Self::fold`) only to
    /// validate the cut point.
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

        let mut messages: Vec<Message> = Vec::new();
        let mut origin_seqs: Vec<u64> = Vec::new();
        let mut state = ReconstructedState::default();

        log.read_chunked(|chunk| {
            for envelope in chunk {
                if fold_step(&mut state, &mut messages, &mut origin_seqs, envelope, up_to)
                    .is_break()
                {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        })
        .await?;

        state.messages = messages;
        Ok(state)
    }

    /// Fold a sequence of envelopes already read from disk. Exposed separately from
    /// [`Self::replay`] so callers that already hold the events (e.g. a live `SessionActor`
    /// applying its own just-appended event, or `llm_condenser.rs`'s condense step folding an
    /// already-sliced sub-`Vec`) can fold incrementally without re-reading the file.
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
            if fold_step(&mut state, &mut messages, &mut origin_seqs, envelope, up_to).is_break() {
                break;
            }
        }

        state.messages = messages;
        state
    }
}

/// Applies one envelope's effect to the running replay state (`state`, `messages`,
/// `origin_seqs`). Shared by [`ReplayEngine::fold`] (iterating an in-memory `Vec`) and
/// [`ReplayEngine::replay`] (iterating envelopes as they arrive from a chunked file read) so both
/// paths apply identical fold semantics.
///
/// Returns [`ControlFlow::Break`] once `up_to` is reached, without applying `envelope` —
/// callers must stop folding further envelopes.
fn fold_step(
    state: &mut ReconstructedState,
    messages: &mut Vec<Message>,
    origin_seqs: &mut Vec<u64>,
    envelope: SessionEventEnvelope,
    up_to: Option<u64>,
) -> ControlFlow<()> {
    if let Some(bound) = up_to
        && envelope.seq >= bound
    {
        return ControlFlow::Break(());
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
                messages,
                origin_seqs,
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
            push_part_to_tool_result_batch(
                messages,
                origin_seqs,
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
            replace_range(messages, origin_seqs, lo, hi, summary.to_markdown());
        }
        SessionEvent::Compaction { summary, .. } => {
            // Compaction's schema (spec §4.3) does not carry an explicit `replaced_seq_range`
            // the way `Condensation` does — it is emitted from the live in-memory compactor,
            // which prunes by message count, not by logged seq. Until P2 wires real emission
            // (zeph-agent-persistence) and settles the exact seq-range accounting, fold
            // conservatively: a recorded summary replaces everything folded so far. No-op when
            // `summary` is absent (a soft-tier prune that dropped raw tool output but produced
            // no summary).
            if let Some(summary) = summary {
                let hi = origin_seqs.last().copied().unwrap_or(seq);
                replace_range(messages, origin_seqs, 0, hi, summary.to_markdown());
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

    ControlFlow::Continue(())
}

/// Append `part` (a `MessagePart::ToolUse`) to the last message if it is a pending `Assistant`
/// message; otherwise start a new one. `ToolCall` events always follow the `AssistantMessage`
/// that requested them within the same turn, but the fold does not assume `AssistantMessage` was
/// itself logged first (a tool-only turn is valid).
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

/// Append `part` (a `MessagePart::ToolResult`) to the last message if it is an already-open
/// tool-result batch; otherwise start a new `Role::User` message.
///
/// `zeph-llm`'s `OpenAI` and Claude serializers require every tool result to arrive in a
/// `Role::User` message, never merged into the preceding `Role::Assistant` message that carried
/// the matching `MessagePart::ToolUse` (#5464) — this mirrors the real shape
/// `process_tool_result_batch` in `crates/zeph-core/src/agent/tool_execution/tier_loop.rs`
/// produces live: one `Role::User` message per tool-call batch, holding one `ToolResult` part per
/// tool. "Already-open batch" is a `Role::User` message with non-empty `parts` that are all
/// `ToolResult` — a genuine `SessionEvent::UserMessage` always folds to empty `parts`
/// ([`Message::from_legacy`]), so this never merges into a real user turn.
fn push_part_to_tool_result_batch(
    messages: &mut Vec<Message>,
    origin_seqs: &mut Vec<u64>,
    seq: u64,
    part: MessagePart,
) {
    let is_open_batch = messages.last().is_some_and(|m| {
        m.role == Role::User
            && !m.parts.is_empty()
            && m.parts
                .iter()
                .all(|p| matches!(p, MessagePart::ToolResult { .. }))
    });
    if is_open_batch {
        let last = messages.last_mut().expect("checked by is_open_batch above");
        last.parts.push(part);
        last.rebuild_content();
        return;
    }
    messages.push(Message::from_parts(Role::User, vec![part]));
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
            3,
            "user message + assistant ToolUse message + user ToolResult message (#5464: a \
             ToolResult must never merge into the preceding Assistant message — OpenAI/Claude \
             both require it in a separate Role::User message)"
        );
        let assistant = &state.messages[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.parts.len(), 1);
        assert!(matches!(assistant.parts[0], MessagePart::ToolUse { .. }));

        let tool_result_msg = &state.messages[2];
        assert_eq!(tool_result_msg.role, Role::User);
        assert_eq!(tool_result_msg.parts.len(), 1);
        assert!(matches!(
            tool_result_msg.parts[0],
            MessagePart::ToolResult { .. }
        ));
    }

    #[test]
    fn test_replay_tool_result_batch_merges_into_one_user_message() {
        // Multiple ToolResult events from the same tool-call batch (tier_loop.rs's
        // process_tool_result_batch persists one Role::User message per batch, holding one
        // ToolResult part per tool) must fold back into a single Role::User message, not one
        // per event.
        let events = vec![
            envelope(
                0,
                SessionEvent::AssistantMessage {
                    parts: vec![
                        MessagePart::ToolUse {
                            id: "tc1".to_owned(),
                            name: "shell".to_owned(),
                            input: serde_json::json!({}),
                        },
                        MessagePart::ToolUse {
                            id: "tc2".to_owned(),
                            name: "shell".to_owned(),
                            input: serde_json::json!({}),
                        },
                    ],
                },
            ),
            envelope(
                1,
                SessionEvent::ToolResult {
                    id: "tc1".to_owned(),
                    name: "shell".to_owned(),
                    output: "a".to_owned(),
                    is_error: false,
                    duration_ms: 1,
                },
            ),
            envelope(
                2,
                SessionEvent::ToolResult {
                    id: "tc2".to_owned(),
                    name: "shell".to_owned(),
                    output: "b".to_owned(),
                    is_error: false,
                    duration_ms: 1,
                },
            ),
        ];
        let state = ReplayEngine::fold(events, None);
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[1].role, Role::User);
        assert_eq!(state.messages[1].parts.len(), 2);
    }

    #[test]
    fn test_replay_tool_result_never_merges_into_plain_user_message() {
        // A genuine SessionEvent::UserMessage (folds to empty `parts`) must never be treated as
        // an open tool-result batch, even if a ToolResult event immediately follows it.
        let events = vec![
            envelope(
                0,
                SessionEvent::UserMessage {
                    text: "hello".to_owned(),
                    image_refs: vec![],
                },
            ),
            envelope(
                1,
                SessionEvent::ToolResult {
                    id: "tc1".to_owned(),
                    name: "shell".to_owned(),
                    output: "a".to_owned(),
                    is_error: false,
                    duration_ms: 1,
                },
            ),
        ];
        let state = ReplayEngine::fold(events, None);
        assert_eq!(state.messages.len(), 2);
        assert!(state.messages[0].parts.is_empty());
        assert_eq!(state.messages[1].parts.len(), 1);
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

    /// Seeds `dir` with a synthetic log spanning `n_turns` turns, exercising all 10
    /// `SessionEvent` variants `fold_step` handles: a tool-call/tool-result pair every 7th turn,
    /// a plain-text assistant reply otherwise, a `Condensation` and a `Compaction` partway
    /// through (both exercise `replace_range`, via distinct range-computation logic), and a
    /// trailing `ForkPoint`/`SessionEnded`/`ModelChanged` (the first two are no-ops in
    /// `fold_step`; included for completeness). Returns the opened log so the caller can read it
    /// back either whole-file or chunked.
    #[allow(clippy::too_many_lines)] // exhaustive fixture covering every SessionEvent variant
    async fn seed_large_synthetic_log(dir: &Path, n_turns: u64) -> SessionEventLog {
        use crate::event::CompactionTier;
        use zeph_common::memory::AnchoredSummary;

        let log = SessionEventLog::open(dir).await.unwrap();

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

        for turn in 0..n_turns {
            log.append(
                Some(turn),
                None,
                SessionEvent::UserMessage {
                    text: format!("user turn {turn}"),
                    image_refs: vec![],
                },
            )
            .await
            .unwrap();

            if turn % 7 == 0 {
                // A tool-call/tool-result pair every 7th turn.
                log.append(
                    Some(turn),
                    None,
                    SessionEvent::AssistantMessage { parts: vec![] },
                )
                .await
                .unwrap();
                log.append(
                    Some(turn),
                    None,
                    SessionEvent::ToolCall {
                        id: format!("tc-{turn}"),
                        name: "shell".to_owned(),
                        input: serde_json::json!({"cmd": "ls"}),
                    },
                )
                .await
                .unwrap();
                log.append(
                    Some(turn),
                    None,
                    SessionEvent::ToolResult {
                        id: format!("tc-{turn}"),
                        name: "shell".to_owned(),
                        output: format!("output-{turn}"),
                        is_error: false,
                        duration_ms: 3,
                    },
                )
                .await
                .unwrap();
            } else {
                log.append(
                    Some(turn),
                    None,
                    SessionEvent::AssistantMessage {
                        parts: vec![MessagePart::Text {
                            text: format!("assistant reply {turn}"),
                        }],
                    },
                )
                .await
                .unwrap();
            }

            if turn == 100 {
                // A condensation partway through, replacing an already-folded range.
                log.append(
                    Some(turn),
                    None,
                    SessionEvent::Condensation {
                        replaced_seq_range: (0, 10),
                        summary: AnchoredSummary {
                            session_intent: "test".to_owned(),
                            files_modified: vec![],
                            decisions_made: vec![],
                            open_questions: vec![],
                            next_steps: vec!["continue".to_owned()],
                        },
                        tokens_before: 500,
                        tokens_after: 50,
                    },
                )
                .await
                .unwrap();
            }

            if turn == 150 {
                // A live hard-compaction partway through, replacing everything folded so far
                // (Compaction's range-computation differs from Condensation's: it has no
                // explicit `replaced_seq_range`, see fold_step's comment on this variant).
                log.append(
                    Some(turn),
                    None,
                    SessionEvent::Compaction {
                        tier: CompactionTier::Hard,
                        cleared_count: 42,
                        summary: Some(AnchoredSummary {
                            session_intent: "test".to_owned(),
                            files_modified: vec![],
                            decisions_made: vec![],
                            open_questions: vec![],
                            next_steps: vec!["keep going".to_owned()],
                        }),
                    },
                )
                .await
                .unwrap();
            }
        }

        // Trailing metadata/no-op events: ForkPoint and SessionEnded are no-ops in `fold_step`,
        // included so this fixture genuinely covers all 10 SessionEvent variants as documented.
        log.append(
            None,
            None,
            SessionEvent::ForkPoint {
                new_session_id: "child-of-s1".to_owned(),
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::SessionEnded {
                reason: "user_quit".to_owned(),
            },
        )
        .await
        .unwrap();
        log.append(
            None,
            None,
            SessionEvent::ModelChanged {
                provider_name: "openai".to_owned(),
                model: "gpt-5.4".to_owned(),
            },
        )
        .await
        .unwrap();

        log
    }

    /// Regression test for #5445 Finding 3: `ReplayEngine::replay`'s new chunked-read path must
    /// produce output equivalent to the old whole-file-`Vec` + `fold` path on a large synthetic
    /// log spanning several hundred events and every `SessionEvent` variant `fold_step` handles.
    #[tokio::test]
    async fn test_replay_streaming_matches_vec_based_fold_on_large_log() {
        const N_TURNS: u64 = 250; // produces well over 100 events (> one REPLAY_CHUNK_SIZE)

        let dir = tempfile::tempdir().unwrap();
        let log = seed_large_synthetic_log(dir.path(), N_TURNS).await;

        // Old path: whole-file Vec read + Vec-based fold.
        let all_events = log.read_all().await.unwrap();
        assert!(
            all_events.len() > 100,
            "synthetic log must exceed one REPLAY_CHUNK_SIZE to exercise multi-chunk streaming"
        );
        let vec_based = ReplayEngine::fold(all_events, None);

        // New path: chunked streaming read via ReplayEngine::replay.
        let streamed = ReplayEngine::replay(dir.path(), None).await.unwrap();

        assert_eq!(streamed.last_seq, vec_based.last_seq);
        assert_eq!(streamed.provider_name, vec_based.provider_name);
        assert_eq!(streamed.model, vec_based.model);
        assert_eq!(streamed.cwd, vec_based.cwd);
        assert_eq!(streamed.messages.len(), vec_based.messages.len());
        assert_eq!(
            serde_json::to_string(&streamed.messages).unwrap(),
            serde_json::to_string(&vec_based.messages).unwrap(),
            "streaming replay must be byte-identical to the old Vec-based fold"
        );
    }
}
