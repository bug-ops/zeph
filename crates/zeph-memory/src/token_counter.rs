// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use dashmap::DashMap;
use tiktoken_rs::CoreBPE;

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

pub struct TokenCounter {
    bpe: Option<CoreBPE>,
    cache: DashMap<u64, usize>,
    cache_cap: usize,
}

impl TokenCounter {
    /// Create a new counter. Falls back to chars/4 if tiktoken init fails.
    #[must_use]
    pub fn new() -> Self {
        let bpe = match tiktoken_rs::cl100k_base() {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!("tiktoken cl100k_base init failed, using chars/4 fallback: {e}");
                None
            }
        };
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
            return text.chars().count() / 4;
        }

        let key = hash_text(text);

        if let Some(cached) = self.cache.get(&key) {
            return *cached;
        }

        let count = match &self.bpe {
            Some(bpe) => bpe.encode_with_special_tokens(text).len(),
            None => text.chars().count() / 4,
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
        let counter = TokenCounter {
            bpe: None,
            cache: DashMap::new(),
            cache_cap: CACHE_CAP,
        };
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
        assert_eq!(result, large.chars().count() / 4);
        // Must not be cached
        assert!(counter.cache.is_empty());
    }

    #[test]
    fn count_tokens_unicode_bpe_differs_from_fallback() {
        let counter = TokenCounter::new();
        let text = "Привет мир! 你好世界! こんにちは! 🌍";
        let bpe_count = counter.count_tokens(text);
        let fallback_count = text.chars().count() / 4;
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
    fn cache_eviction_at_capacity() {
        let counter = TokenCounter {
            bpe: None,
            cache: DashMap::new(),
            cache_cap: 3,
        };
        let _ = counter.count_tokens("aaaa");
        let _ = counter.count_tokens("bbbb");
        let _ = counter.count_tokens("cccc");
        assert_eq!(counter.cache.len(), 3);
        // This should evict one entry
        let _ = counter.count_tokens("dddd");
        assert_eq!(counter.cache.len(), 3);
    }
}
