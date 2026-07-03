// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool-pair sanitization helpers: remove orphaned `ToolUse`/`ToolResult` messages from
//! restored conversation history.
//!
//! These are pure functions operating on `Vec<Message>` slices — no agent state required.

use std::collections::HashSet;

use zeph_llm::provider::{Message, MessagePart, Role};

/// Remove orphaned `ToolUse`/`ToolResult` messages from restored history.
///
/// Four failure modes are handled:
/// 1. **Trailing orphan**: the last message is an assistant with `ToolUse` parts but no
///    subsequent user message with `ToolResult` — caused by LIMIT boundary splits or
///    interrupted sessions.
/// 2. **Leading orphan**: the first message is a user with `ToolResult` parts but no
///    preceding assistant message with `ToolUse` — caused by LIMIT boundary cuts.
/// 3. **Mid-history orphaned `ToolUse`**: an assistant message with `ToolUse` parts is not
///    followed by a user message with matching `ToolResult` parts. The `ToolUse` parts are
///    stripped; if no content remains the message is removed.
/// 4. **Mid-history orphaned `ToolResult`**: a user message has `ToolResult` parts whose
///    `tool_use_id` is not present in the preceding assistant message. Those `ToolResult` parts
///    are stripped; if no content remains the message is removed.
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
    let mut removed = 0;
    let mut db_ids: Vec<i64> = Vec::new();

    // Remove trailing orphaned tool_use messages (assistant with ToolUse, no following tool_result).
    while let Some(last) = messages.last()
        && last.role == Role::Assistant
        && last
            .parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolUse { .. }))
    {
        let ids: Vec<String> = last
            .parts
            .iter()
            .filter_map(|p| {
                if let MessagePart::ToolUse { id, .. } = p {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();
        tracing::warn!(
            tool_ids = ?ids,
            "removing orphaned trailing tool_use message from restored history"
        );
        if let Some(db_id) = messages.last().and_then(|m| m.metadata.db_id) {
            db_ids.push(db_id);
        }
        messages.pop();
        removed += 1;
    }

    // Count leading orphaned tool_result messages (user with ToolResult, no preceding tool_use),
    // then drain them in a single O(N) pass instead of repeated O(N) remove(0) calls.
    let skip_count = messages
        .iter()
        .take_while(|m| {
            m.role == Role::User
                && m.parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        })
        .count();

    if skip_count > 0 {
        for m in messages.iter().take(skip_count) {
            let ids: Vec<String> = m
                .parts
                .iter()
                .filter_map(|p| {
                    if let MessagePart::ToolResult { tool_use_id, .. } = p {
                        Some(tool_use_id.clone())
                    } else {
                        None
                    }
                })
                .collect();
            tracing::warn!(
                tool_use_ids = ?ids,
                "removing orphaned leading tool_result message from restored history"
            );
            if let Some(db_id) = m.metadata.db_id {
                db_ids.push(db_id);
            }
        }
        messages.drain(0..skip_count);
        removed += skip_count;
    }

    let (mid_removed, mid_db_ids) = strip_mid_history_orphans(messages);
    removed += mid_removed;
    db_ids.extend(mid_db_ids);

    (removed, db_ids)
}

/// Returns `true` if `content` contains human-readable text beyond legacy tool bracket markers.
///
/// Legacy markers produced by `Message::flatten_parts` are:
/// - `[tool_use: name(id)]` — assistant `ToolUse`
/// - `[tool_result: id]\nbody` — user `ToolResult`
/// - `[tool output: name] body` — `ToolOutput`
///
/// A message whose content consists solely of such markers (and whitespace) has no
/// user-visible text and is a candidate for soft-delete.
///
/// # Examples
///
/// ```
/// use zeph_agent_persistence::sanitize::has_meaningful_content;
///
/// assert!(has_meaningful_content("hello world"));
/// assert!(!has_meaningful_content("[tool_use: bash(abc123)]"));
/// assert!(!has_meaningful_content("   [tool_result: abc]\nsome output"));
/// ```
#[must_use]
pub fn has_meaningful_content(content: &str) -> bool {
    const PREFIXES: [&str; 3] = ["[tool_use: ", "[tool_result: ", "[tool output: "];

    let mut remaining = content.trim();

    loop {
        let next = PREFIXES
            .iter()
            .filter_map(|prefix| remaining.find(prefix).map(|pos| (pos, *prefix)))
            .min_by_key(|(pos, _)| *pos);

        let Some((start, prefix)) = next else {
            break;
        };

        if !remaining[..start].trim().is_empty() {
            return true;
        }

        let after_prefix = &remaining[start + prefix.len()..];
        let Some(close) = after_prefix.find(']') else {
            return true; // Malformed tag — treat as meaningful.
        };

        let tag_end = start + prefix.len() + close + 1;

        if prefix == "[tool_result: " || prefix == "[tool output: " {
            let body = remaining[tag_end..].trim_start_matches('\n');
            let next_tag = PREFIXES
                .iter()
                .filter_map(|p| body.find(p))
                .min()
                .unwrap_or(body.len());
            remaining = &body[next_tag..];
        } else {
            remaining = &remaining[tag_end..];
        }
    }

    !remaining.trim().is_empty()
}

/// Collect `tool_use` IDs from `msg` that have no matching `ToolResult` in `next_msg`.
fn orphaned_tool_use_ids(msg: &Message, next_msg: Option<&Message>) -> HashSet<String> {
    let matched: HashSet<String> = next_msg
        .filter(|n| n.role == Role::User)
        .map(|n| {
            msg.parts
                .iter()
                .filter_map(|p| if let MessagePart::ToolUse { id, .. } = p { Some(id.clone()) } else { None })
                .filter(|uid| n.parts.iter().any(|np| matches!(np, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == uid)))
                .collect()
        })
        .unwrap_or_default();
    msg.parts
        .iter()
        .filter_map(|p| {
            if let MessagePart::ToolUse { id, .. } = p
                && !matched.contains(id)
            {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Collect `tool_result` IDs from `msg` that have no matching `ToolUse` in `prev_msg`.
fn orphaned_tool_result_ids(msg: &Message, prev_msg: Option<&Message>) -> HashSet<String> {
    let avail: HashSet<&str> = prev_msg
        .filter(|p| p.role == Role::Assistant)
        .map(|p| {
            p.parts
                .iter()
                .filter_map(|part| {
                    if let MessagePart::ToolUse { id, .. } = part {
                        Some(id.as_str())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    msg.parts
        .iter()
        .filter_map(|p| {
            if let MessagePart::ToolResult { tool_use_id, .. } = p
                && !avail.contains(tool_use_id.as_str())
            {
                Some(tool_use_id.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Strips orphaned `ToolUse` parts from `messages[i]` (an assistant message) when unmatched by a
/// `ToolResult` in the next non-system message. Returns `true` if the message was removed
/// entirely (caller must not advance past `i`).
fn strip_orphaned_tool_use_at(
    messages: &mut Vec<Message>,
    i: usize,
    db_ids: &mut Vec<i64>,
) -> bool {
    let next_non_system = (i + 1..messages.len())
        .find(|&j| messages[j].role != Role::System)
        .and_then(|j| messages.get(j));
    let orphaned_ids = orphaned_tool_use_ids(&messages[i], next_non_system);
    if orphaned_ids.is_empty() {
        return false;
    }
    tracing::warn!(
        tool_ids = ?orphaned_ids,
        index = i,
        "stripping orphaned mid-history tool_use parts from assistant message"
    );
    messages[i]
        .parts
        .retain(|p| !matches!(p, MessagePart::ToolUse { id, .. } if orphaned_ids.contains(id)));
    let is_empty = !has_meaningful_content(&messages[i].content) && messages[i].parts.is_empty();
    if is_empty {
        if let Some(db_id) = messages[i].metadata.db_id {
            db_ids.push(db_id);
        }
        messages.remove(i);
    }
    is_empty
}

/// Strips orphaned and duplicate `ToolResult` parts from `messages[i]` (a user message).
///
/// A part is orphaned when unmatched by a `ToolUse` in the previous non-system message, or a
/// **duplicate** (#5513) when its `tool_use_id` is already in `resolved_tool_use_ids` — i.e. a
/// `tool_use_id` that already received a result (real or tombstone) earlier in history and shows
/// up again later, e.g. from a cancellation-handling defect that wrote more than one tombstone for
/// the same call. Either shape would trip the same "`tool_calls` must be followed by tool
/// messages" provider error, so both are stripped.
///
/// Whatever `ToolResult` parts survive are added to `resolved_tool_use_ids`. Returns `true` if the
/// message was removed entirely (caller must not advance past `i`).
fn strip_tool_result_orphans_at(
    messages: &mut Vec<Message>,
    i: usize,
    resolved_tool_use_ids: &mut HashSet<String>,
    db_ids: &mut Vec<i64>,
) -> bool {
    let prev_non_system = (0..i)
        .rev()
        .find(|&j| messages[j].role != Role::System)
        .and_then(|j| messages.get(j));
    let orphaned_ids = orphaned_tool_result_ids(&messages[i], prev_non_system);
    let duplicate_ids: HashSet<String> = messages[i]
        .parts
        .iter()
        .filter_map(|p| {
            if let MessagePart::ToolResult { tool_use_id, .. } = p {
                Some(tool_use_id.clone())
            } else {
                None
            }
        })
        .filter(|id| resolved_tool_use_ids.contains(id))
        .collect();
    if !duplicate_ids.is_empty() {
        tracing::warn!(
            tool_use_ids = ?duplicate_ids,
            index = i,
            "stripping duplicate mid-history tool_result parts from user message"
        );
    }
    if !orphaned_ids.is_empty() {
        tracing::warn!(
            tool_use_ids = ?orphaned_ids,
            index = i,
            "stripping orphaned mid-history tool_result parts from user message"
        );
    }
    let strip_ids: HashSet<&str> = orphaned_ids
        .iter()
        .chain(duplicate_ids.iter())
        .map(String::as_str)
        .collect();
    let mut removed = false;
    if !strip_ids.is_empty() {
        messages[i].parts.retain(|p| {
            !matches!(p, MessagePart::ToolResult { tool_use_id, .. } if strip_ids.contains(tool_use_id.as_str()))
        });
        let is_empty =
            !has_meaningful_content(&messages[i].content) && messages[i].parts.is_empty();
        if is_empty {
            if let Some(db_id) = messages[i].metadata.db_id {
                db_ids.push(db_id);
            }
            messages.remove(i);
            removed = true;
        }
    }
    if !removed {
        for p in &messages[i].parts {
            if let MessagePart::ToolResult { tool_use_id, .. } = p {
                resolved_tool_use_ids.insert(tool_use_id.clone());
            }
        }
    }
    removed
}

/// Scan all messages and strip orphaned `ToolUse`/`ToolResult` parts from mid-history messages,
/// as well as **duplicate** `ToolResult` parts (#5513) — see [`strip_tool_result_orphans_at`].
///
/// `resolved_tool_use_ids` is scoped to the current open call window: whenever a `ToolUse(id)`
/// is encountered, `id` is removed from the set first, since it is being re-opened. Some
/// providers (e.g. Ollama, which assigns `tool_call` ids as `format!("call_{i}")` by batch
/// index) legitimately reuse the same `tool_use_id` across turns; without this, a later turn's
/// real `ToolResult` would be misdetected as a duplicate of an earlier turn's and stripped,
/// orphaning the later turn's `ToolUse`.
fn strip_mid_history_orphans(messages: &mut Vec<Message>) -> (usize, Vec<i64>) {
    let mut removed = 0;
    let mut db_ids: Vec<i64> = Vec::new();
    let mut resolved_tool_use_ids: HashSet<String> = HashSet::new();
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == Role::Assistant
            && messages[i]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. }))
        {
            for p in &messages[i].parts {
                if let MessagePart::ToolUse { id, .. } = p {
                    resolved_tool_use_ids.remove(id);
                }
            }
            if strip_orphaned_tool_use_at(messages, i, &mut db_ids) {
                removed += 1;
                continue;
            }
        }

        if messages[i].role == Role::User
            && messages[i]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
            && strip_tool_result_orphans_at(messages, i, &mut resolved_tool_use_ids, &mut db_ids)
        {
            removed += 1;
            continue;
        }

        i += 1;
    }
    (removed, db_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::MessageMetadata;

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

    #[test]
    fn has_meaningful_content_with_text() {
        assert!(has_meaningful_content("hello world"));
        assert!(has_meaningful_content(
            "some text [tool_use: bash(abc)] more text"
        ));
    }

    #[test]
    fn has_meaningful_content_only_markers() {
        assert!(!has_meaningful_content("[tool_use: bash(abc123)]"));
        assert!(!has_meaningful_content("  "));
    }

    #[test]
    fn has_meaningful_content_empty() {
        assert!(!has_meaningful_content(""));
    }

    /// Regression test for #5513 item 6 (turn-scoped, see S1 correction below):
    /// `strip_mid_history_orphans` must track resolved `tool_use_id`s cumulatively *within one
    /// open call window*, so a duplicate `ToolResult` several messages downstream of its
    /// matching `ToolUse` (no intervening re-open) is still caught — see
    /// `duplicate_tool_result_several_messages_downstream_is_stripped` for that shape.
    ///
    /// This test instead documents the corrected boundary: when a *second* `ToolUse` reuses the
    /// same id (re-opening the call), `resolved_tool_use_ids` forgets the earlier resolution, so
    /// the following `ToolResult` is evaluated as a fresh pairing, not a duplicate. An earlier
    /// version of this test asserted the opposite (id reuse via a new `ToolUse` always means
    /// duplicate) — that assumption was wrong: it is indistinguishable from legitimate
    /// index-based id reuse (e.g. Ollama's `format!("call_{i}")`, see
    /// `legitimate_id_reuse_across_turns_ollama_style_must_not_be_stripped`), and stripping it
    /// unconditionally re-creates the #5513 corruption for those providers.
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
    /// `resolved_tool_use_ids` is scoped to the current open call window (S1 fix): a `ToolUse(id)`
    /// removes `id` from the set, so a later turn's legitimate `ToolUse(call_0) ->
    /// ToolResult(call_0, real)` pair is evaluated fresh, not flagged as a duplicate of an
    /// earlier turn's result.
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
}
