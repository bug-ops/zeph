// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-conversation guard over the request prefix the Claude API sees.
//!
//! A conversation that stays append-only keeps its prompt cache and its preserved thinking
//! blocks valid. The guard fingerprints the wire projection of every request (system, tools and
//! each wire message, `cache_control` excluded) and compares it with the fingerprint of the
//! previous successful request of the same conversation. When something before the tail changed,
//! it reports where, so the caller can drop the thinking from that point on instead of sending
//! blocks the API will reject or silently discard.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

use parking_lot::Mutex;

use crate::provider::ConversationKey;

use super::types::{
    AnthropicContentBlock, StructuredApiMessage, StructuredContent, SystemContentBlock,
};

/// Conversations tracked at once; the oldest is evicted first.
const CAPACITY: usize = 16;

/// Hashes of one request's wire prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WireFingerprint {
    system: u64,
    tools: u64,
    messages: Vec<u64>,
}

/// What changed relative to the previous request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DivergenceReason {
    /// The system blocks differ.
    System,
    /// The tool set differs.
    Tools,
    /// A message before the tail differs.
    Message,
}

impl DivergenceReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Tools => "tools",
            Self::Message => "message",
        }
    }
}

/// First point at which a request no longer extends the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Divergence {
    pub reason: DivergenceReason,
    /// Wire `messages[]` index; 0 for a system or tools change.
    pub wire_index: usize,
}

fn hash_one(value: &impl Hash) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Hash a JSON value structurally, without serializing it. Object members hash in document
/// order, which `preserve_order` keeps stable for a given value.
fn hash_value(value: &serde_json::Value, hasher: &mut impl Hasher) {
    use serde_json::Value;
    match value {
        Value::Null => 0u8.hash(hasher),
        Value::Bool(b) => (1u8, b).hash(hasher),
        Value::Number(n) => (2u8, n.to_string()).hash(hasher),
        Value::String(s) => (3u8, s).hash(hasher),
        Value::Array(items) => {
            (4u8, items.len()).hash(hasher);
            for item in items {
                hash_value(item, hasher);
            }
        }
        Value::Object(map) => {
            (5u8, map.len()).hash(hasher);
            for (key, item) in map {
                key.hash(hasher);
                hash_value(item, hasher);
            }
        }
    }
}

fn hash_block(block: &AnthropicContentBlock, hasher: &mut impl Hasher) {
    match block {
        AnthropicContentBlock::Text { text, .. } => (0u8, text).hash(hasher),
        AnthropicContentBlock::ToolUse { id, name, input } => {
            (1u8, id, name).hash(hasher);
            hash_value(input, hasher);
        }
        AnthropicContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => (2u8, tool_use_id, content, is_error).hash(hasher),
        AnthropicContentBlock::Image { source } => {
            (3u8, &source.source_type, &source.media_type, &source.data).hash(hasher);
        }
        AnthropicContentBlock::Thinking {
            thinking,
            signature,
        } => (4u8, thinking, signature).hash(hasher),
        AnthropicContentBlock::RedactedThinking { data } => (5u8, data).hash(hasher),
        AnthropicContentBlock::Compaction { content } => (6u8, content).hash(hasher),
    }
}

/// Hash a message by role and blocks. Plain text hashes like a single text block, so adding a
/// cache breakpoint (which turns text into a block) does not change the hash.
fn hash_message(message: &StructuredApiMessage) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    message.role.hash(&mut hasher);
    match &message.content {
        StructuredContent::Text(text) => (0u8, text).hash(&mut hasher),
        StructuredContent::Blocks(blocks) => {
            for block in blocks {
                hash_block(block, &mut hasher);
            }
        }
    }
    hasher.finish()
}

impl WireFingerprint {
    /// Fingerprint the request prefix. `tools` is the hash of the tool definitions
    /// ([`super::cache::tool_cache_key`]).
    #[tracing::instrument(name = "llm.claude.prefix_fingerprint", skip_all, fields(messages = messages.len()))]
    pub(super) fn of(
        system: Option<&[SystemContentBlock]>,
        tools: u64,
        messages: &[StructuredApiMessage],
    ) -> Self {
        let system_texts: Vec<&str> = system
            .unwrap_or_default()
            .iter()
            .map(|b| b.text.as_str())
            .collect();
        Self {
            system: hash_one(&system_texts),
            tools,
            messages: messages.iter().map(hash_message).collect(),
        }
    }

    /// Where `self` stops extending `previous`, or `None` when it is an extension or a prefix.
    ///
    /// A shorter request whose messages all match (an error pop) is not a divergence.
    pub(super) fn divergence_from(&self, previous: &Self) -> Option<Divergence> {
        if self.system != previous.system {
            return Some(Divergence {
                reason: DivergenceReason::System,
                wire_index: 0,
            });
        }
        if self.tools != previous.tools {
            return Some(Divergence {
                reason: DivergenceReason::Tools,
                wire_index: 0,
            });
        }
        self.messages
            .iter()
            .zip(&previous.messages)
            .position(|(new, old)| new != old)
            .map(|wire_index| Divergence {
                reason: DivergenceReason::Message,
                wire_index,
            })
    }
}

/// Fingerprints of the last successful request per conversation, bounded to [`CAPACITY`].
#[derive(Debug, Default)]
pub(super) struct PrefixGuard {
    entries: Mutex<VecDeque<(ConversationKey, WireFingerprint)>>,
}

impl PrefixGuard {
    /// Fingerprint stored for `key`, if any.
    pub(super) fn previous(&self, key: ConversationKey) -> Option<WireFingerprint> {
        self.entries
            .lock()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, fp)| fp.clone())
    }

    /// Record `fingerprint` as the latest successful request of `key`.
    pub(super) fn store(&self, key: ConversationKey, fingerprint: WireFingerprint) {
        let mut entries = self.entries.lock();
        entries.retain(|(k, _)| *k != key);
        if entries.len() == CAPACITY {
            entries.pop_front();
        }
        entries.push_back((key, fingerprint));
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_message(role: &str, text: &str) -> StructuredApiMessage {
        StructuredApiMessage {
            role: role.to_owned(),
            content: StructuredContent::Text(text.to_owned()),
        }
    }

    fn fp(system: &str, tools: u64, messages: &[StructuredApiMessage]) -> WireFingerprint {
        let blocks = [SystemContentBlock {
            block_type: "text",
            text: system.to_owned(),
            cache_control: None,
        }];
        WireFingerprint::of(Some(&blocks), tools, messages)
    }

    #[test]
    fn extension_and_shrink_are_not_divergence() {
        let a = [text_message("user", "a"), text_message("assistant", "b")];
        let longer = [
            text_message("user", "a"),
            text_message("assistant", "b"),
            text_message("user", "c"),
        ];
        let prev = fp("s", 1, &a);
        assert_eq!(fp("s", 1, &longer).divergence_from(&prev), None);
        assert_eq!(prev.divergence_from(&fp("s", 1, &longer)), None);
    }

    #[test]
    fn changed_message_reports_its_wire_index() {
        let prev = fp(
            "s",
            1,
            &[text_message("user", "a"), text_message("assistant", "b")],
        );
        let now = fp(
            "s",
            1,
            &[text_message("user", "a"), text_message("assistant", "x")],
        );
        assert_eq!(
            now.divergence_from(&prev),
            Some(Divergence {
                reason: DivergenceReason::Message,
                wire_index: 1
            })
        );
    }

    #[test]
    fn system_or_tools_change_diverges_at_zero() {
        let msgs = [text_message("user", "a")];
        let prev = fp("s", 1, &msgs);
        assert_eq!(
            fp("t", 1, &msgs).divergence_from(&prev),
            Some(Divergence {
                reason: DivergenceReason::System,
                wire_index: 0
            })
        );
        assert_eq!(
            fp("s", 2, &msgs).divergence_from(&prev),
            Some(Divergence {
                reason: DivergenceReason::Tools,
                wire_index: 0
            })
        );
    }

    #[test]
    fn cache_control_and_text_shape_do_not_change_the_hash() {
        let plain = text_message("user", "a");
        let block = StructuredApiMessage {
            role: "user".to_owned(),
            content: StructuredContent::Blocks(vec![AnthropicContentBlock::Text {
                text: "a".to_owned(),
                cache_control: Some(super::super::cache::build_cache_control(None)),
            }]),
        };
        assert_eq!(hash_message(&plain), hash_message(&block));
    }

    #[test]
    fn store_is_bounded_and_keyed() {
        let guard = PrefixGuard::default();
        let msgs = [text_message("user", "a")];
        for key in 0..40u64 {
            guard.store(ConversationKey(key), fp("s", key, &msgs));
        }
        assert_eq!(guard.len(), CAPACITY);
        assert!(guard.previous(ConversationKey(0)).is_none());
        assert!(guard.previous(ConversationKey(39)).is_some());
        guard.store(ConversationKey(39), fp("s", 99, &msgs));
        assert_eq!(guard.len(), CAPACITY);
        assert_eq!(
            guard.previous(ConversationKey(39)).map(|f| f.tools),
            Some(99)
        );
    }
}
