// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Graph extraction configuration helpers.
//!
//! Pure functions that convert `zeph-config` graph settings into `zeph-memory` extraction
//! configs, and collect context message snapshots for background tasks.

use zeph_llm::provider::{Message, MessagePart, Role};

/// Build a [`zeph_memory::semantic::GraphExtractionConfig`] from a config section.
///
/// This is a pure field-mapping function with no side effects.
///
/// # Examples
///
/// ```no_run
/// use zeph_agent_persistence::graph::build_graph_extraction_config;
/// use zeph_config::memory::GraphConfig;
///
/// let cfg = GraphConfig::default();
/// let extraction_cfg = build_graph_extraction_config(&cfg, Some(42));
/// assert_eq!(extraction_cfg.conversation_id, Some(42));
/// ```
#[must_use]
pub fn build_graph_extraction_config(
    cfg: &zeph_config::memory::GraphConfig,
    conversation_id: Option<i64>,
) -> zeph_memory::semantic::GraphExtractionConfig {
    zeph_memory::semantic::GraphExtractionConfig {
        max_entities: cfg.max_entities_per_message,
        max_edges: cfg.max_edges_per_message,
        extraction_timeout_secs: cfg.extraction_timeout_secs,
        community_refresh_interval: cfg.community_refresh_interval,
        expired_edge_retention_days: cfg.expired_edge_retention_days,
        max_entities_cap: cfg.max_entities,
        community_summary_max_prompt_bytes: cfg.community_summary_max_prompt_bytes,
        community_summary_concurrency: cfg.community_summary_concurrency,
        lpa_edge_chunk_size: cfg.lpa_edge_chunk_size,
        note_linking: zeph_memory::NoteLinkingConfig {
            enabled: cfg.note_linking.enabled,
            similarity_threshold: cfg.note_linking.similarity_threshold,
            top_k: cfg.note_linking.top_k,
            timeout_secs: cfg.note_linking.timeout_secs,
        },
        link_weight_decay_lambda: cfg.link_weight_decay_lambda,
        link_weight_decay_interval_secs: cfg.link_weight_decay_interval_secs,
        belief_revision_enabled: cfg.belief_revision.enabled,
        belief_revision_similarity_threshold: cfg.belief_revision.similarity_threshold,
        conversation_id,
    }
}

/// Collect recent user messages (non-tool-result) as context strings for graph extraction.
///
/// Takes the four most recent qualifying user messages, truncating each to 2048 characters.
/// Returned in reverse chronological order (most recent first).
///
/// # Examples
///
/// ```
/// use zeph_agent_persistence::graph::collect_context_messages;
/// use zeph_llm::provider::{Message, MessageMetadata, Role};
///
/// let messages = vec![
///     Message { role: Role::User, content: "hello".into(), parts: vec![], metadata: MessageMetadata::default() },
/// ];
/// let ctx = collect_context_messages(&messages);
/// assert_eq!(ctx, vec!["hello"]);
/// ```
#[must_use]
pub fn collect_context_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .rev()
        .filter(|m| {
            m.role == Role::User
                && !m
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        })
        .take(4)
        .map(|m| {
            if m.content.len() > 2048 {
                m.content[..m.content.floor_char_boundary(2048)].to_owned()
            } else {
                m.content.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::MessageMetadata;

    fn user_msg(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    fn assistant_msg(content: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: content.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    #[test]
    fn collect_empty_messages() {
        let ctx = collect_context_messages(&[]);
        assert!(ctx.is_empty());
    }

    #[test]
    fn collect_user_messages_only() {
        let msgs = vec![
            user_msg("first"),
            assistant_msg("reply"),
            user_msg("second"),
        ];
        let ctx = collect_context_messages(&msgs);
        // Reversed (most recent first), only user messages
        assert_eq!(ctx, vec!["second", "first"]);
    }

    #[test]
    fn collect_at_most_four_messages() {
        let msgs: Vec<_> = (0..10).map(|i| user_msg(&format!("msg {i}"))).collect();
        let ctx = collect_context_messages(&msgs);
        assert_eq!(ctx.len(), 4);
    }

    #[test]
    fn skips_tool_result_messages() {
        let tool_result_msg = Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "abc".to_owned(),
                content: "output".to_owned(),
                is_error: false,
            }],
            metadata: zeph_llm::provider::MessageMetadata::default(),
        };
        let msgs = vec![user_msg("real message"), tool_result_msg];
        let ctx = collect_context_messages(&msgs);
        assert_eq!(ctx, vec!["real message"]);
    }
}
