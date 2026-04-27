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

    loop {
        // Remove trailing orphaned tool_use (assistant message with ToolUse, no following tool_result).
        if let Some(last) = messages.last()
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
            continue;
        }

        // Remove leading orphaned tool_result (user message with ToolResult, no preceding tool_use).
        if let Some(first) = messages.first()
            && first.role == Role::User
            && first
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        {
            let ids: Vec<String> = first
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
            if let Some(db_id) = messages.first().and_then(|m| m.metadata.db_id) {
                db_ids.push(db_id);
            }
            messages.remove(0);
            removed += 1;
            continue;
        }

        break;
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

/// Scan all messages and strip orphaned `ToolUse`/`ToolResult` parts from mid-history messages.
fn strip_mid_history_orphans(messages: &mut Vec<Message>) -> (usize, Vec<i64>) {
    let mut removed = 0;
    let mut db_ids: Vec<i64> = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == Role::Assistant
            && messages[i]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. }))
        {
            let next_non_system = (i + 1..messages.len())
                .find(|&j| messages[j].role != Role::System)
                .and_then(|j| messages.get(j));
            let orphaned_ids = orphaned_tool_use_ids(&messages[i], next_non_system);
            if !orphaned_ids.is_empty() {
                tracing::warn!(
                    tool_ids = ?orphaned_ids,
                    index = i,
                    "stripping orphaned mid-history tool_use parts from assistant message"
                );
                messages[i].parts.retain(
                    |p| !matches!(p, MessagePart::ToolUse { id, .. } if orphaned_ids.contains(id)),
                );
                let is_empty =
                    !has_meaningful_content(&messages[i].content) && messages[i].parts.is_empty();
                if is_empty {
                    if let Some(db_id) = messages[i].metadata.db_id {
                        db_ids.push(db_id);
                    }
                    messages.remove(i);
                    removed += 1;
                    continue;
                }
            }
        }

        if messages[i].role == Role::User
            && messages[i]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        {
            let prev_non_system = (0..i)
                .rev()
                .find(|&j| messages[j].role != Role::System)
                .and_then(|j| messages.get(j));
            let orphaned_ids = orphaned_tool_result_ids(&messages[i], prev_non_system);
            if !orphaned_ids.is_empty() {
                tracing::warn!(
                    tool_use_ids = ?orphaned_ids,
                    index = i,
                    "stripping orphaned mid-history tool_result parts from user message"
                );
                messages[i].parts.retain(|p| {
                    !matches!(p, MessagePart::ToolResult { tool_use_id, .. } if orphaned_ids.contains(tool_use_id.as_str()))
                });

                let is_empty =
                    !has_meaningful_content(&messages[i].content) && messages[i].parts.is_empty();
                if is_empty {
                    if let Some(db_id) = messages[i].metadata.db_id {
                        db_ids.push(db_id);
                    }
                    messages.remove(i);
                    removed += 1;
                    continue;
                }
            }
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
}
