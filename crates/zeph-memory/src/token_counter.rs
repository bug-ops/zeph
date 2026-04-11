// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use dashmap::DashMap;
use tiktoken_rs::CoreBPE;
use zeph_common::text::estimate_tokens;
use zeph_llm::provider::{Message, MessagePart};

static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();

const CACHE_CAP: usize = 10_000;
/// Inputs larger than this limit bypass BPE encoding and use the chars/4 fallback.
/// Prevents CPU amplification from pathologically large inputs.
const MAX_INPUT_LEN: usize = 65_536;

// OpenAI function-calling token overhead constants
const FUNC_INIT: usize = 7;
const PROP_INIT: usize = 3;
const PROP_KEY: usize = 3;
const ENUM_INIT: isize = -3;
const ENUM_ITEM: usize = 3;
const FUNC_END: usize = 12;

// Structural overhead per part type (approximate token counts for JSON framing)
/// `{"type":"tool_use","id":"","name":"","input":}`
const TOOL_USE_OVERHEAD: usize = 20;
/// `{"type":"tool_result","tool_use_id":"","content":""}`
const TOOL_RESULT_OVERHEAD: usize = 15;
/// `[tool output: <name>]\n` wrapping
const TOOL_OUTPUT_OVERHEAD: usize = 8;
/// Image block JSON structure overhead (dimension-based counting unavailable)
const IMAGE_OVERHEAD: usize = 50;
/// Default token estimate for a typical image (dimensions not available in `ImageData`)
const IMAGE_DEFAULT_TOKENS: usize = 1000;
/// Thinking/redacted block framing
const THINKING_OVERHEAD: usize = 10;

/// Token counter backed by `tiktoken` `cl100k_base` BPE encoding.
///
/// Estimates how many tokens a piece of text or a full [`Message`] will consume when
/// sent to an LLM API.  Uses a process-scoped [`OnceLock`] so BPE data is loaded once
/// and shared across all instances.
///
/// Falls back to a `chars/4` heuristic when tiktoken init fails or when the input
/// exceeds `MAX_INPUT_LEN` bytes (64 KiB).
///
/// # Examples
///
/// ```
/// use zeph_memory::TokenCounter;
///
/// let counter = TokenCounter::new();
/// let n = counter.count_tokens("Hello, world!");
/// assert!(n > 0);
/// ```
pub struct TokenCounter {
    bpe: &'static Option<CoreBPE>,
    cache: DashMap<u64, usize>,
    cache_cap: usize,
}

impl TokenCounter {
    /// Create a new counter. Falls back to chars/4 if tiktoken init fails.
    ///
    /// BPE data is loaded once and cached in a `OnceLock` for the process lifetime.
    #[must_use]
    pub fn new() -> Self {
        let bpe = BPE.get_or_init(|| match tiktoken_rs::cl100k_base() {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!("tiktoken cl100k_base init failed, using chars/4 fallback: {e}");
                None
            }
        });
        Self {
            bpe,
            cache: DashMap::new(),
            cache_cap: CACHE_CAP,
        }
    }

    /// Count tokens in text. Uses cache, falls back to heuristic.
    ///
    /// Inputs exceeding 64 KiB bypass BPE and use chars/4 without caching to
    /// avoid CPU amplification from oversized inputs.
    #[must_use]
    pub fn count_tokens(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }

        if text.len() > MAX_INPUT_LEN {
            return zeph_common::text::estimate_tokens(text);
        }

        let key = hash_text(text);

        if let Some(cached) = self.cache.get(&key) {
            return *cached;
        }

        let count = match self.bpe {
            Some(bpe) => bpe.encode_with_special_tokens(text).len(),
            None => zeph_common::text::estimate_tokens(text),
        };

        // TOCTOU between len() check and insert is benign: worst case we evict
        // one extra entry and temporarily exceed the cap by one slot.
        if self.cache.len() >= self.cache_cap {
            let key_to_evict = self.cache.iter().next().map(|e| *e.key());
            if let Some(k) = key_to_evict {
                self.cache.remove(&k);
            }
        }
        self.cache.insert(key, count);

        count
    }

    /// Estimate token count for a message the way the LLM API will see it.
    ///
    /// When structured parts exist, counts from parts matching the API payload
    /// structure. Falls back to `content` (flattened text) when parts is empty.
    #[must_use]
    pub fn count_message_tokens(&self, msg: &Message) -> usize {
        if msg.parts.is_empty() {
            return self.count_tokens(&msg.content);
        }
        msg.parts.iter().map(|p| self.count_part_tokens(p)).sum()
    }

    /// Estimate tokens for a single [`MessagePart`] matching the API payload structure.
    #[must_use]
    fn count_part_tokens(&self, part: &MessagePart) -> usize {
        match part {
            MessagePart::Text { text }
            | MessagePart::Recall { text }
            | MessagePart::CodeContext { text }
            | MessagePart::Summary { text }
            | MessagePart::CrossSession { text } => {
                if text.trim().is_empty() {
                    return 0;
                }
                self.count_tokens(text)
            }

            // API always emits `[tool output: {name}]\n{body}` regardless of compacted_at.
            // When body is emptied by compaction, count_tokens(body) returns 0 naturally.
            MessagePart::ToolOutput {
                tool_name, body, ..
            } => {
                TOOL_OUTPUT_OVERHEAD
                    + self.count_tokens(tool_name.as_str())
                    + self.count_tokens(body)
            }

            // API sends structured JSON block: `{"type":"tool_use","id":"...","name":"...","input":...}`
            MessagePart::ToolUse { id, name, input } => {
                TOOL_USE_OVERHEAD
                    + self.count_tokens(id)
                    + self.count_tokens(name)
                    + self.count_tokens(&input.to_string())
            }

            // API sends structured block: `{"type":"tool_result","tool_use_id":"...","content":"..."}`
            MessagePart::ToolResult {
                tool_use_id,
                content,
                ..
            } => TOOL_RESULT_OVERHEAD + self.count_tokens(tool_use_id) + self.count_tokens(content),

            // Image token count depends on pixel dimensions, which are unavailable in ImageData.
            // Using a fixed constant is more accurate than bytes-based formula because
            // Claude's actual formula is width*height based, not payload-size based.
            MessagePart::Image(_) => IMAGE_OVERHEAD + IMAGE_DEFAULT_TOKENS,

            // ThinkingBlock is preserved verbatim in multi-turn requests.
            MessagePart::ThinkingBlock {
                thinking,
                signature,
            } => THINKING_OVERHEAD + self.count_tokens(thinking) + self.count_tokens(signature),

            // RedactedThinkingBlock is an opaque base64 blob — BPE is not meaningful here.
            MessagePart::RedactedThinkingBlock { data } => {
                THINKING_OVERHEAD + estimate_tokens(data)
            }

            // Compaction summary is sent back verbatim to the API.
            MessagePart::Compaction { summary } => self.count_tokens(summary),
        }
    }

    /// Count tokens for an `OpenAI` tool/function schema `JSON` value.
    #[must_use]
    pub fn count_tool_schema_tokens(&self, schema: &serde_json::Value) -> usize {
        let base = count_schema_value(self, schema);
        let total =
            base.cast_signed() + ENUM_INIT + FUNC_INIT.cast_signed() + FUNC_END.cast_signed();
        total.max(0).cast_unsigned()
    }
}

impl Default for TokenCounter {
    fn default() -> Self {
        Self::new()
    }
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn count_schema_value(counter: &TokenCounter, value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => {
            let mut tokens = PROP_INIT;
            for (key, val) in map {
                tokens += PROP_KEY + counter.count_tokens(key);
                tokens += count_schema_value(counter, val);
            }
            tokens
        }
        serde_json::Value::Array(arr) => {
            let mut tokens = ENUM_ITEM;
            for item in arr {
                tokens += count_schema_value(counter, item);
            }
            tokens
        }
        serde_json::Value::String(s) => counter.count_tokens(s),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) | serde_json::Value::Null => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::{ImageData, Message, MessageMetadata, MessagePart, Role};

    static BPE_NONE: Option<CoreBPE> = None;

    fn counter_with_no_bpe(cache_cap: usize) -> TokenCounter {
        TokenCounter {
            bpe: &BPE_NONE,
            cache: DashMap::new(),
            cache_cap,
        }
    }

    fn make_msg(parts: Vec<MessagePart>) -> Message {
        Message::from_parts(Role::User, parts)
    }

    fn make_msg_no_parts(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    #[test]
    fn count_message_tokens_empty_parts_falls_back_to_content() {
        let counter = TokenCounter::new();
        let msg = make_msg_no_parts("hello world");
        assert_eq!(
            counter.count_message_tokens(&msg),
            counter.count_tokens("hello world")
        );
    }

    #[test]
    fn count_message_tokens_text_part_matches_count_tokens() {
        let counter = TokenCounter::new();
        let text = "the quick brown fox jumps over the lazy dog";
        let msg = make_msg(vec![MessagePart::Text {
            text: text.to_string(),
        }]);
        assert_eq!(
            counter.count_message_tokens(&msg),
            counter.count_tokens(text)
        );
    }

    #[test]
    fn count_message_tokens_tool_use_exceeds_flattened_content() {
        let counter = TokenCounter::new();
        // Large JSON input: structured counting should be higher than flattened "[tool_use: bash(id)]"
        let input = serde_json::json!({"command": "find /home -name '*.rs' -type f | head -100"});
        let msg = make_msg(vec![MessagePart::ToolUse {
            id: "toolu_abc".into(),
            name: "bash".into(),
            input,
        }]);
        let structured = counter.count_message_tokens(&msg);
        let flattened = counter.count_tokens(&msg.content);
        assert!(
            structured > flattened,
            "structured={structured} should exceed flattened={flattened}"
        );
    }

    #[test]
    fn count_message_tokens_compacted_tool_output_is_small() {
        let counter = TokenCounter::new();
        // Compacted ToolOutput has empty body — should count close to overhead only
        let msg = make_msg(vec![MessagePart::ToolOutput {
            tool_name: "bash".into(),
            body: String::new(),
            compacted_at: Some(1_700_000_000),
        }]);
        let tokens = counter.count_message_tokens(&msg);
        // Should be small: TOOL_OUTPUT_OVERHEAD + count_tokens("bash") + 0
        assert!(
            tokens <= 15,
            "compacted tool output should be small, got {tokens}"
        );
    }

    #[test]
    fn count_message_tokens_image_returns_constant() {
        let counter = TokenCounter::new();
        let msg = make_msg(vec![MessagePart::Image(Box::new(ImageData {
            data: vec![0u8; 1000],
            mime_type: "image/jpeg".into(),
        }))]);
        assert_eq!(
            counter.count_message_tokens(&msg),
            IMAGE_OVERHEAD + IMAGE_DEFAULT_TOKENS
        );
    }

    #[test]
    fn count_message_tokens_thinking_block_counts_text() {
        let counter = TokenCounter::new();
        let thinking = "step by step reasoning about the problem";
        let signature = "sig";
        let msg = make_msg(vec![MessagePart::ThinkingBlock {
            thinking: thinking.to_string(),
            signature: signature.to_string(),
        }]);
        let expected =
            THINKING_OVERHEAD + counter.count_tokens(thinking) + counter.count_tokens(signature);
        assert_eq!(counter.count_message_tokens(&msg), expected);
    }

    #[test]
    fn count_part_tokens_empty_text_returns_zero() {
        let counter = TokenCounter::new();
        assert_eq!(
            counter.count_part_tokens(&MessagePart::Text {
                text: String::new()
            }),
            0
        );
        assert_eq!(
            counter.count_part_tokens(&MessagePart::Text {
                text: "   ".to_string()
            }),
            0
        );
        assert_eq!(
            counter.count_part_tokens(&MessagePart::Recall {
                text: "\n\t".to_string()
            }),
            0
        );
    }

    #[test]
    fn count_message_tokens_push_recompute_consistency() {
        // Verify that sum of count_message_tokens per part equals recompute result
        let counter = TokenCounter::new();
        let parts = vec![
            MessagePart::Text {
                text: "hello".into(),
            },
            MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: "output data".into(),
                compacted_at: None,
            },
        ];
        let msg = make_msg(parts);
        let total = counter.count_message_tokens(&msg);
        let sum: usize = msg.parts.iter().map(|p| counter.count_part_tokens(p)).sum();
        assert_eq!(total, sum);
    }

    #[test]
    fn count_message_tokens_parts_take_priority_over_content() {
        // R-2: primary regression guard — when parts is non-empty, content is ignored.
        let counter = TokenCounter::new();
        let parts_text = "hello from parts";
        let msg = Message {
            role: Role::User,
            content: "completely different content that should be ignored".to_string(),
            parts: vec![MessagePart::Text {
                text: parts_text.to_string(),
            }],
            metadata: MessageMetadata::default(),
        };
        let parts_based = counter.count_tokens(parts_text);
        let content_based = counter.count_tokens(&msg.content);
        assert_ne!(
            parts_based, content_based,
            "test setup: parts and content must differ"
        );
        assert_eq!(counter.count_message_tokens(&msg), parts_based);
    }

    #[test]
    fn count_part_tokens_tool_result() {
        // R-3: verify ToolResult arm counting
        let counter = TokenCounter::new();
        let tool_use_id = "toolu_xyz";
        let content = "result text";
        let part = MessagePart::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: content.to_string(),
            is_error: false,
        };
        let expected = TOOL_RESULT_OVERHEAD
            + counter.count_tokens(tool_use_id)
            + counter.count_tokens(content);
        assert_eq!(counter.count_part_tokens(&part), expected);
    }

    #[test]
    fn count_tokens_empty() {
        let counter = TokenCounter::new();
        assert_eq!(counter.count_tokens(""), 0);
    }

    #[test]
    fn count_tokens_non_empty() {
        let counter = TokenCounter::new();
        assert!(counter.count_tokens("hello world") > 0);
    }

    #[test]
    fn count_tokens_cache_hit_returns_same() {
        let counter = TokenCounter::new();
        let text = "the quick brown fox";
        let first = counter.count_tokens(text);
        let second = counter.count_tokens(text);
        assert_eq!(first, second);
    }

    #[test]
    fn count_tokens_fallback_mode() {
        let counter = counter_with_no_bpe(CACHE_CAP);
        // 8 chars / 4 = 2
        assert_eq!(counter.count_tokens("abcdefgh"), 2);
        assert_eq!(counter.count_tokens(""), 0);
    }

    #[test]
    fn count_tokens_oversized_input_uses_fallback_without_cache() {
        let counter = TokenCounter::new();
        // Generate input larger than MAX_INPUT_LEN (65536 bytes)
        let large = "a".repeat(MAX_INPUT_LEN + 1);
        let result = counter.count_tokens(&large);
        // chars/4 fallback: (65537 chars) / 4
        assert_eq!(result, zeph_common::text::estimate_tokens(&large));
        // Must not be cached
        assert!(counter.cache.is_empty());
    }

    #[test]
    fn count_tokens_unicode_bpe_differs_from_fallback() {
        let counter = TokenCounter::new();
        let text = "Привет мир! 你好世界! こんにちは! 🌍";
        let bpe_count = counter.count_tokens(text);
        let fallback_count = zeph_common::text::estimate_tokens(text);
        // BPE should return > 0
        assert!(bpe_count > 0, "BPE count must be positive");
        // BPE result should differ from naive chars/4 for multi-byte text
        assert_ne!(
            bpe_count, fallback_count,
            "BPE tokenization should differ from chars/4 for unicode text"
        );
    }

    #[test]
    fn count_tool_schema_tokens_sample() {
        let counter = TokenCounter::new();
        let schema = serde_json::json!({
            "name": "get_weather",
            "description": "Get the current weather for a location",
            "parameters": {
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "The city name"
                    }
                },
                "required": ["location"]
            }
        });
        let tokens = counter.count_tool_schema_tokens(&schema);
        // Pinned value: computed by running the formula with cl100k_base on this exact schema.
        // If this fails, a tokenizer or formula change likely occurred.
        assert_eq!(tokens, 82);
    }

    #[test]
    fn two_instances_share_bpe_pointer() {
        let a = TokenCounter::new();
        let b = TokenCounter::new();
        assert!(std::ptr::eq(a.bpe, b.bpe));
    }

    #[test]
    fn cache_eviction_at_capacity() {
        let counter = counter_with_no_bpe(3);
        let _ = counter.count_tokens("aaaa");
        let _ = counter.count_tokens("bbbb");
        let _ = counter.count_tokens("cccc");
        assert_eq!(counter.cache.len(), 3);
        // This should evict one entry
        let _ = counter.count_tokens("dddd");
        assert_eq!(counter.cache.len(), 3);
    }
}
