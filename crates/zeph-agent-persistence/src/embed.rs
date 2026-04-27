// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Message embedding and memory-write helpers.
//!
//! These are pure async functions that operate on `SemanticMemory` and message slices — no
//! agent state or borrow-lens views required.

use zeph_llm::provider::{MessagePart, Role};
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::store::role_str;

/// Decide whether a message should be embedded into Qdrant.
///
/// Returns `false` when:
/// - `skip_embedding` is `true` (exfiltration guard active)
/// - The message contains `[skipped]` or `[stopped]` `ToolResult` parts (internal policy markers)
/// - The message is from the assistant and autosave is disabled or content is too short
///
/// # Examples
///
/// ```
/// use zeph_agent_persistence::embed::should_embed_message;
/// use zeph_llm::provider::Role;
///
/// assert!(should_embed_message(false, &[], Role::User, false, 0, 100));
/// assert!(!should_embed_message(true, &[], Role::User, false, 0, 100));
/// ```
#[must_use]
pub fn should_embed_message(
    skip_embedding: bool,
    parts: &[MessagePart],
    role: Role,
    autosave_assistant: bool,
    autosave_min_length: usize,
    content_len: usize,
) -> bool {
    if skip_embedding {
        return false;
    }
    // Do not embed [skipped] or [stopped] ToolResult content into Qdrant — these are
    // internal policy markers that carry no useful semantic information and would
    // contaminate memory_search results.
    let has_skipped_tool_result = parts.iter().any(|p| {
        if let MessagePart::ToolResult { content, .. } = p {
            content.starts_with("[skipped]") || content.starts_with("[stopped]")
        } else {
            false
        }
    });
    if has_skipped_tool_result {
        return false;
    }
    match role {
        Role::Assistant => autosave_assistant && content_len >= autosave_min_length,
        _ => true,
    }
}

/// Write a single message to `SemanticMemory` (either `remember_with_parts` or `save_only`).
///
/// Returns `Some((embedding_stored, message_id))` on success, or `None` when the message was
/// rejected by the A-MAC admission filter or a DB error occurred.
///
/// # Parameters
///
/// - `memory` — Semantic memory backend
/// - `cid` — Active conversation ID
/// - `role` — Message role
/// - `content` — Full text content
/// - `parts_json` — JSON-serialized message parts
/// - `goal_text` — Optional current conversation goal (used for embedding enrichment)
/// - `should_embed` — Whether to embed into Qdrant or write to `SQLite` only
pub async fn write_message_to_memory(
    memory: &SemanticMemory,
    cid: zeph_memory::ConversationId,
    role: Role,
    content: &str,
    parts_json: &str,
    goal_text: Option<&str>,
    should_embed: bool,
) -> Option<(bool, i64)> {
    if should_embed {
        match memory
            .remember_with_parts(cid, role_str(role), content, parts_json, goal_text)
            .await
        {
            Ok((Some(message_id), stored)) => Some((stored, message_id.0)),
            Ok((None, _)) => {
                // A-MAC admission rejected — skip increment and further processing.
                None
            }
            Err(e) => {
                tracing::error!("failed to persist message: {e:#}");
                None
            }
        }
    } else {
        match memory
            .save_only(cid, role_str(role), content, parts_json)
            .await
        {
            Ok(message_id) => Some((false, message_id.0)),
            Err(e) => {
                tracing::error!("failed to persist message: {e:#}");
                None
            }
        }
    }
}

/// Serialize message parts to a JSON string.
///
/// Returns `None` and logs an error if serialization fails — callers should skip persistence
/// to avoid creating orphaned tool-pair records in `SQLite`.
pub fn serialize_parts_json(parts: &[MessagePart], role: Role) -> Option<String> {
    if parts.is_empty() {
        return Some("[]".to_string());
    }
    match serde_json::to_string(parts) {
        Ok(json) => Some(json),
        Err(e) => {
            tracing::error!(
                role = ?role,
                parts_count = parts.len(),
                error = %e,
                "failed to serialize message parts — skipping persist to avoid orphaned tool pair"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::Role;

    #[test]
    fn should_embed_user_message() {
        assert!(should_embed_message(false, &[], Role::User, false, 0, 0));
    }

    #[test]
    fn should_not_embed_when_skip_flag_set() {
        assert!(!should_embed_message(true, &[], Role::User, false, 0, 100));
    }

    #[test]
    fn should_embed_assistant_when_autosave_and_length_met() {
        assert!(should_embed_message(
            false,
            &[],
            Role::Assistant,
            true,
            20,
            50
        ));
    }

    #[test]
    fn should_not_embed_assistant_when_autosave_disabled() {
        assert!(!should_embed_message(
            false,
            &[],
            Role::Assistant,
            false,
            20,
            50
        ));
    }

    #[test]
    fn should_not_embed_assistant_when_too_short() {
        assert!(!should_embed_message(
            false,
            &[],
            Role::Assistant,
            true,
            100,
            50
        ));
    }

    #[test]
    fn should_not_embed_skipped_tool_result() {
        let parts = vec![MessagePart::ToolResult {
            tool_use_id: "abc".to_owned(),
            content: "[skipped] tool was blocked".to_owned(),
            is_error: false,
        }];
        assert!(!should_embed_message(
            false,
            &parts,
            Role::User,
            false,
            0,
            100
        ));
    }

    #[test]
    fn serialize_empty_parts() {
        let result = serialize_parts_json(&[], Role::User);
        assert_eq!(result.as_deref(), Some("[]"));
    }
}
