// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool-pair sanitization helpers: remove orphaned `ToolUse`/`ToolResult` parts from restored
//! conversation history.
//!
//! These are pure functions operating on `Vec<Message>` slices — no agent state required.
//! Orphan repair itself delegates to `zeph_llm::tool_pairing` (issue #6771).
//!
//! A prior revision of this module also ran a separate duplicate-`ToolResult` sweep after orphan
//! repair (#5513: a `tool_use_id` that already received a result earlier in the same open call
//! window, re-appearing later — e.g. from a cancellation-handling defect that wrote more than one
//! tombstone for the same call). That sweep is now provably unreachable and was removed: after
//! [`zeph_llm::tool_pairing::repair_tool_pairs`] with [`OrphanAction::Delete`], every surviving
//! `ToolResult(X)` has, by [`unmatched_tool_result_ids`](zeph_llm::tool_pairing::unmatched_tool_result_ids)'s
//! own definition, an `Assistant` message carrying `ToolUse(X)` as its immediately preceding
//! non-system message — so any #5513 shape is already caught by adjacency repair itself, which
//! never depended on a cross-message "already resolved" id set in the first place (that is
//! exactly the global-vs-adjacency distinction issue #6770 is about). The `#5513` regression
//! tests below are kept: they now document that adjacency repair alone handles duplicate/reopened
//! `tool_use_id`s correctly, without needing a second bookkeeping pass.

use zeph_llm::provider::Message;
use zeph_llm::tool_pairing::{self, OrphanAction};

/// Remove orphaned `ToolUse`/`ToolResult` parts from restored history.
///
/// Delegates to [`zeph_llm::tool_pairing::repair_tool_pairs`] with [`OrphanAction::Delete`]: a
/// `ToolUse` part with no matching `ToolResult` in the immediately following non-system message,
/// or a `ToolResult` part with no matching `ToolUse` in the immediately preceding non-system
/// message, is deleted. A message left with no parts and no meaningful content is removed
/// entirely. Repair is part-level — a message that also carries independent text is kept, only
/// the orphaned part is stripped (spec-078 FR-003).
///
/// Returns `(removed_count, db_ids)` where `removed_count` is the number of messages removed
/// entirely and `db_ids` contains `metadata.db_id` values of those messages for `SQLite`
/// soft-delete.
///
/// # Examples
///
/// ```
/// use zeph_agent_persistence::sanitize::sanitize_tool_pairs;
/// use zeph_llm::provider::{Message, MessageMetadata, Role};
///
/// let mut messages = vec![
///     Message { role: Role::User, content: "hello".into(), parts: vec![], metadata: MessageMetadata::default() },
/// ];
/// let (removed, ids) = sanitize_tool_pairs(&mut messages);
/// assert_eq!(removed, 0);
/// assert!(ids.is_empty());
/// ```
pub fn sanitize_tool_pairs(messages: &mut Vec<Message>) -> (usize, Vec<i64>) {
    let report = tool_pairing::repair_tool_pairs(messages, OrphanAction::Delete);
    let db_ids: Vec<i64> = report
        .removed_messages
        .iter()
        .filter_map(|m| m.metadata.db_id)
        .collect();
    (report.removed_messages.len(), db_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::{MessageMetadata, MessagePart, Role};

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    fn msg_with_parts(role: Role, content: &str, parts: Vec<MessagePart>) -> Message {
        Message {
            role,
            content: content.to_owned(),
            parts,
            metadata: MessageMetadata::default(),
        }
    }

    #[test]
    fn empty_messages_unchanged() {
        let mut msgs: Vec<Message> = vec![];
        let (removed, ids) = sanitize_tool_pairs(&mut msgs);
        assert_eq!(removed, 0);
        assert!(ids.is_empty());
    }

    #[test]
    fn clean_conversation_unchanged() {
        let mut msgs = vec![msg(Role::User, "hello"), msg(Role::Assistant, "hi")];
        let (removed, _) = sanitize_tool_pairs(&mut msgs);
        assert_eq!(removed, 0);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn trailing_orphan_tool_use_removed() {
        let tool_use = MessagePart::ToolUse {
            id: "abc".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({}),
        };
        let mut msgs = vec![
            msg(Role::User, "run something"),
            msg_with_parts(Role::Assistant, "[tool_use: bash(abc)]", vec![tool_use]),
        ];
        let (removed, _) = sanitize_tool_pairs(&mut msgs);
        assert_eq!(removed, 1);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn single_leading_orphan_tool_result_removed() {
        let tool_result = MessagePart::ToolResult {
            tool_use_id: "x1".to_owned(),
            content: "output".to_owned(),
            is_error: false,
        };
        let mut msgs = vec![
            msg_with_parts(Role::User, "[tool_result: x1]", vec![tool_result]),
            msg(Role::User, "hello"),
            msg(Role::Assistant, "hi"),
        ];
        let (removed, _) = sanitize_tool_pairs(&mut msgs);
        assert_eq!(removed, 1);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "hello");
    }

    #[test]
    fn multiple_consecutive_leading_orphans_removed() {
        let tr = |id: &str| MessagePart::ToolResult {
            tool_use_id: id.to_owned(),
            content: "out".to_owned(),
            is_error: false,
        };
        let mut msgs = vec![
            msg_with_parts(Role::User, "[tool_result: a]", vec![tr("a")]),
            msg_with_parts(Role::User, "[tool_result: b]", vec![tr("b")]),
            msg_with_parts(Role::User, "[tool_result: c]", vec![tr("c")]),
            msg(Role::User, "real message"),
            msg(Role::Assistant, "ok"),
        ];
        let (removed, _) = sanitize_tool_pairs(&mut msgs);
        assert_eq!(removed, 3);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "real message");
    }

    #[test]
    fn trailing_orphan_does_not_remove_leading_clean_messages() {
        let tool_use = MessagePart::ToolUse {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({}),
        };
        let mut msgs = vec![
            msg(Role::User, "first"),
            msg(Role::Assistant, "second"),
            msg(Role::User, "third"),
            msg_with_parts(Role::Assistant, "[tool_use: bash(t1)]", vec![tool_use]),
        ];
        let (removed, _) = sanitize_tool_pairs(&mut msgs);
        assert_eq!(removed, 1);
        assert_eq!(msgs.len(), 3);
    }

    /// Relaxation (spec-078 FR-003 amendment): the shared helper is part-level. A trailing
    /// assistant message that carries an orphaned `ToolUse` *and* independent text now keeps
    /// the text instead of being removed wholesale, unlike the old whole-message removal.
    #[test]
    fn trailing_orphan_tool_use_alongside_text_keeps_the_text() {
        let mut msgs = vec![
            msg(Role::User, "run something"),
            msg_with_parts(
                Role::Assistant,
                "thinking",
                vec![
                    MessagePart::Text {
                        text: "thinking out loud".to_owned(),
                    },
                    MessagePart::ToolUse {
                        id: "abc".to_owned(),
                        name: "bash".to_owned(),
                        input: serde_json::json!({}),
                    },
                ],
            ),
        ];
        let (removed, _) = sanitize_tool_pairs(&mut msgs);
        assert_eq!(removed, 0, "the message survives because text remains");
        assert_eq!(msgs.len(), 2);
        assert!(
            msgs[1]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::Text { text } if text == "thinking out loud"))
        );
        assert!(
            !msgs[1]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. }))
        );
    }

    /// C2 regression: a partial strip must never touch `content`, not just a fully-empty one.
    /// A restored assistant message with `parts = [ThinkingBlock, ToolUse(orphan)]` keeps
    /// `ThinkingBlock` after the orphan is stripped (parts non-empty), but `ThinkingBlock` alone
    /// flattens to `""` — rebuilding `content` from the surviving parts would silently empty a
    /// message whose `content` field holds real, independently-authored DB-restored text.
    #[test]
    fn partial_strip_never_touches_independently_authored_content() {
        let mut msgs = vec![
            msg(Role::User, "run something"),
            Message {
                role: Role::Assistant,
                content: "Independently-authored reasoning text flatten_parts cannot \
                          reconstruct."
                    .to_owned(),
                parts: vec![
                    MessagePart::ThinkingBlock {
                        thinking: "internal reasoning".to_owned(),
                        signature: "sig".to_owned(),
                    },
                    MessagePart::ToolUse {
                        id: "orphan".to_owned(),
                        name: "bash".to_owned(),
                        input: serde_json::json!({}),
                    },
                ],
                metadata: MessageMetadata::default(),
            },
        ];
        let content_before = msgs[1].content.clone();
        let (removed, _) = sanitize_tool_pairs(&mut msgs);
        assert_eq!(
            removed, 0,
            "the message survives because ThinkingBlock keeps parts non-empty"
        );
        assert_eq!(msgs.len(), 2);
        assert!(
            !msgs[1]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. })),
            "the orphaned ToolUse must be stripped"
        );
        assert_eq!(
            msgs[1].content, content_before,
            "content must never be touched by a partial strip — it may hold independently- \
             authored text `flatten_parts` cannot reconstruct"
        );
    }

    /// Regression test for #5513 item 6: when a *second* `ToolUse` reuses the same id
    /// (re-opening the call), the second `ToolResult(id)` must pair with the second `ToolUse`,
    /// not be treated as a duplicate of the first result. Adjacency repair handles this for
    /// free — each `ToolResult` matches only against its own immediately preceding non-system
    /// message, never a cross-message "already resolved" id set — so there is nothing here that
    /// needs id reuse to be distinguished from a genuine duplicate; see
    /// `duplicate_tool_result_several_messages_downstream_is_stripped` for the shape that *is* a
    /// genuine duplicate, and the module docs for why a separate dedup sweep was removed as
    /// provably unreachable after this repair runs.
    #[test]
    fn tool_result_after_id_reopened_by_new_tool_use_is_not_a_duplicate() {
        let tool_use = |id: &str| MessagePart::ToolUse {
            id: id.to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({}),
        };
        let tool_result = |id: &str, content: &str| MessagePart::ToolResult {
            tool_use_id: id.to_owned(),
            content: content.to_owned(),
            is_error: false,
        };

        let mut msgs = vec![
            msg_with_parts(
                Role::Assistant,
                "[tool_use: bash(t1)]",
                vec![tool_use("t1")],
            ),
            msg_with_parts(
                Role::User,
                "[tool_result: t1]\nfirst output",
                vec![tool_result("t1", "first output")],
            ),
            // A second ToolUse legitimately re-opens the same id "t1".
            msg_with_parts(
                Role::Assistant,
                "[tool_use: bash(t1)]",
                vec![tool_use("t1")],
            ),
            msg_with_parts(
                Role::User,
                "[tool_result: t1]\nsecond output",
                vec![tool_result("t1", "second output")],
            ),
        ];

        let (removed, _) = sanitize_tool_pairs(&mut msgs);

        assert_eq!(
            removed, 0,
            "the second ToolResult for t1 must survive: it pairs with the second ToolUse, \
             not a duplicate of the first"
        );
        let remaining_results: Vec<&str> = msgs
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| {
                if let MessagePart::ToolResult { content, .. } = p {
                    Some(content.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            remaining_results,
            vec!["first output", "second output"],
            "both results must survive intact"
        );
    }

    /// Regression test for the Ollama id-reuse finding (impl-critic, verified against
    /// `crates/zeph-llm/src/ollama.rs:462`): Ollama assigns `tool_call` ids as `format!("call_{i}")`
    /// by batch index, so `call_0` legitimately recurs on *every* turn of a multi-turn tool
    /// conversation — unlike OpenAI/Claude/Gemini, which use globally unique per-call ids.
    ///
    /// Adjacency repair (issue #6770/#6771) matches each `ToolResult` only against its own
    /// immediately preceding non-system message, so a later turn's legitimate
    /// `ToolUse(call_0) -> ToolResult(call_0, real)` pair is evaluated fresh against its own
    /// neighbour — never flagged as a duplicate of an earlier, unrelated turn's result.
    #[test]
    fn legitimate_id_reuse_across_turns_ollama_style_must_not_be_stripped() {
        let tool_use = |id: &str| MessagePart::ToolUse {
            id: id.to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({}),
        };
        let tool_result = |id: &str, content: &str| MessagePart::ToolResult {
            tool_use_id: id.to_owned(),
            content: content.to_owned(),
            is_error: false,
        };

        let mut msgs = vec![
            // Turn 1: ToolUse(call_0) -> ToolResult(call_0, real turn-1 output).
            msg_with_parts(
                Role::Assistant,
                "[tool_use: bash(call_0)]",
                vec![tool_use("call_0")],
            ),
            msg_with_parts(
                Role::User,
                "[tool_result: call_0]\nturn-1 output",
                vec![tool_result("call_0", "turn-1 output")],
            ),
            // Turn 2 (Ollama-style id reuse, unrelated to turn 1): ToolUse(call_0) ->
            // ToolResult(call_0, real turn-2 output). Both parts are legitimate — this is not
            // a cancellation-cascade duplicate.
            msg_with_parts(
                Role::Assistant,
                "[tool_use: bash(call_0)]",
                vec![tool_use("call_0")],
            ),
            msg_with_parts(
                Role::User,
                "[tool_result: call_0]\nturn-2 output",
                vec![tool_result("call_0", "turn-2 output")],
            ),
        ];

        let (removed, _) = sanitize_tool_pairs(&mut msgs);

        assert_eq!(
            removed, 0,
            "turn 2's legitimate ToolResult(call_0) must not be stripped just because \
             call_0 was already resolved in turn 1 (Ollama-style id reuse)"
        );
        let remaining_results: Vec<&str> = msgs
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| {
                if let MessagePart::ToolResult { content, .. } = p {
                    Some(content.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            remaining_results,
            vec!["turn-1 output", "turn-2 output"],
            "both turns' results must survive intact"
        );
    }

    /// Regression test for #5513: the exact malformed shape from the issue's evidence dump —
    /// a real `ToolResult` followed several turns later by a contradicting `[Cancelled]`
    /// tombstone for the same `tool_use_id`, with unrelated messages in between. Even though
    /// this shape was already caught by the pre-existing single-lookback "orphan" check (its
    /// immediate predecessor is a plain non-tool message), this test locks in that end-to-end
    /// behavior so a future refactor of the lookback logic cannot silently regress it.
    #[test]
    fn duplicate_tool_result_several_messages_downstream_is_stripped() {
        let tool_use = |id: &str| MessagePart::ToolUse {
            id: id.to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({}),
        };
        let tool_result = |id: &str, content: &str| MessagePart::ToolResult {
            tool_use_id: id.to_owned(),
            content: content.to_owned(),
            is_error: false,
        };

        let mut msgs = vec![
            msg_with_parts(
                Role::Assistant,
                "[tool_use: bash(t1)]",
                vec![tool_use("t1")],
            ),
            msg_with_parts(
                Role::User,
                "[tool_result: t1]\nreal output",
                vec![tool_result("t1", "real output")],
            ),
            msg(Role::User, "a follow-up question"),
            msg(Role::Assistant, "a plain reply, no tool use"),
            msg(Role::User, "another follow-up"),
            msg_with_parts(
                Role::User,
                "[tool_result: t1]",
                vec![tool_result("t1", "[Cancelled]")],
            ),
        ];

        let (removed, _) = sanitize_tool_pairs(&mut msgs);

        assert_eq!(
            removed, 1,
            "the downstream duplicate ToolResult must be stripped"
        );
        let remaining_results: Vec<&str> = msgs
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| {
                if let MessagePart::ToolResult { content, .. } = p {
                    Some(content.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(remaining_results, vec!["real output"]);
    }

    /// A leading orphaned `ToolResult` whose id recurs later in history must not confuse the
    /// later, legitimate pair: adjacency repair matches each `ToolResult` only against its own
    /// immediately preceding non-system message, so the leading orphan (`prev == None`) and the
    /// later pair (`prev` is the matching `Assistant(ToolUse)`) are judged independently, with
    /// no cross-message id-set bookkeeping to confuse.
    #[test]
    fn leading_orphan_result_id_recurring_later_does_not_confuse_the_later_legitimate_pair() {
        let tool_use = |id: &str| MessagePart::ToolUse {
            id: id.to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({}),
        };
        let tool_result = |id: &str, content: &str| MessagePart::ToolResult {
            tool_use_id: id.to_owned(),
            content: content.to_owned(),
            is_error: false,
        };

        let mut msgs = vec![
            // Leading orphan: no preceding ToolUse for "dup_id" — will be repaired away first.
            msg_with_parts(
                Role::User,
                "[tool_result: dup_id]",
                vec![tool_result("dup_id", "boundary-cut orphan")],
            ),
            msg(Role::User, "real task"),
            msg_with_parts(
                Role::Assistant,
                "[tool_use: bash(dup_id)]",
                vec![tool_use("dup_id")],
            ),
            msg_with_parts(
                Role::User,
                "[tool_result: dup_id]\nreal output",
                vec![tool_result("dup_id", "real output")],
            ),
        ];

        let (removed, _) = sanitize_tool_pairs(&mut msgs);

        assert_eq!(removed, 1, "only the leading orphan must be removed");
        let remaining_results: Vec<&str> = msgs
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| {
                if let MessagePart::ToolResult { content, .. } = p {
                    Some(content.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            remaining_results,
            vec!["real output"],
            "the legitimate later pair must survive, not be flagged a duplicate of the orphan"
        );
    }
}
