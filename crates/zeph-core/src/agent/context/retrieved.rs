// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Extracted retrieved-memory context from the current turn's message list.
//!
//! Used by the MARCH self-check pipeline to obtain the evidence set without
//! giving the pipeline direct access to the full message history.

use zeph_llm::provider::{Message, MessagePart, Role};

use crate::quality::pipeline::RetrievedContext;
use zeph_agent_context::helpers::{
    CROSS_SESSION_PREFIX, GRAPH_FACTS_PREFIX, RECALL_PREFIX, SUMMARY_PREFIX,
};

/// Walk the message list and collect all retrieved-memory fragments.
///
/// Two paths are supported:
/// - **Canonical multipart path**: `MessagePart::{Recall, Summary, CrossSession}` attached to any message.
/// - **Legacy string-prefix path**: `Role::System` messages whose text content begins with a
///   known prefix constant (used by Ollama and older session restores).
///
/// `MessagePart::GraphFacts` does not exist; graph facts flow via `Role::System` messages
/// with the `GRAPH_FACTS_PREFIX` prefix — they are captured by the legacy path.
pub fn collect_retrieved_context(messages: &[Message]) -> RetrievedContext<'_> {
    let mut rc = RetrievedContext::default();

    for msg in messages {
        // (a) Canonical multipart path
        for part in &msg.parts {
            match part {
                MessagePart::Recall { text } => rc.recall.push(text.as_str()),
                MessagePart::Summary { text } => rc.summaries.push(text.as_str()),
                MessagePart::CrossSession { text } => rc.cross_session.push(text.as_str()),
                _ => {}
            }
        }

        // (b) Legacy string-prefix path on System role only
        if msg.role == Role::System {
            for part in &msg.parts {
                if let Some(text) = part.as_plain_text() {
                    if let Some(body) = text.strip_prefix(RECALL_PREFIX) {
                        rc.recall.push(body);
                    } else if let Some(body) = text.strip_prefix(SUMMARY_PREFIX) {
                        rc.summaries.push(body);
                    } else if let Some(body) = text.strip_prefix(CROSS_SESSION_PREFIX) {
                        rc.cross_session.push(body);
                    } else if let Some(body) = text.strip_prefix(GRAPH_FACTS_PREFIX) {
                        rc.graph_facts.push(body);
                    }
                }
            }
            // Also scan legacy content field (Ollama providers set content only, no parts)
            if msg.parts.is_empty() {
                let text = msg.content.as_str();
                if let Some(body) = text.strip_prefix(RECALL_PREFIX) {
                    rc.recall.push(body);
                } else if let Some(body) = text.strip_prefix(SUMMARY_PREFIX) {
                    rc.summaries.push(body);
                } else if let Some(body) = text.strip_prefix(CROSS_SESSION_PREFIX) {
                    rc.cross_session.push(body);
                } else if let Some(body) = text.strip_prefix(GRAPH_FACTS_PREFIX) {
                    rc.graph_facts.push(body);
                }
            }
        }
    }

    rc
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::MessageMetadata;

    fn sys_msg(content: &str) -> Message {
        Message {
            role: Role::System,
            content: content.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    fn msg_with_part(role: Role, part: MessagePart) -> Message {
        Message {
            role,
            content: String::new(),
            parts: vec![part],
            metadata: MessageMetadata::default(),
        }
    }

    #[test]
    fn collect_finds_multipart_recall() {
        let msgs = vec![msg_with_part(
            Role::User,
            MessagePart::Recall {
                text: "recall fragment".into(),
            },
        )];
        let rc = collect_retrieved_context(&msgs);
        assert_eq!(rc.recall, vec!["recall fragment"]);
        assert!(rc.summaries.is_empty());
    }

    #[test]
    fn collect_finds_legacy_prefix_system() {
        let msgs = vec![sys_msg(&format!("{RECALL_PREFIX}legacy recall body"))];
        let rc = collect_retrieved_context(&msgs);
        assert_eq!(rc.recall, vec!["legacy recall body"]);
    }

    #[test]
    fn collect_combines_both_shapes() {
        let msgs = vec![
            msg_with_part(
                Role::User,
                MessagePart::Recall {
                    text: "part recall".into(),
                },
            ),
            sys_msg(&format!("{GRAPH_FACTS_PREFIX}graph data")),
        ];
        let rc = collect_retrieved_context(&msgs);
        assert_eq!(rc.recall, vec!["part recall"]);
        assert_eq!(rc.graph_facts, vec!["graph data"]);
    }

    #[test]
    fn collect_skips_non_retrieval_parts() {
        let msgs = vec![msg_with_part(
            Role::User,
            MessagePart::Text {
                text: "plain user text".into(),
            },
        )];
        let rc = collect_retrieved_context(&msgs);
        assert!(rc.is_empty());
    }

    #[test]
    fn collect_empty_on_plain_user_turn() {
        let msgs = vec![Message {
            role: Role::User,
            content: "hello world".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let rc = collect_retrieved_context(&msgs);
        assert!(rc.is_empty());
    }
}
