// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Prompt caching utilities for the Claude provider.

use zeph_common::text::estimate_tokens;

use crate::provider::ToolDefinition;

use super::types::{
    AGENT_IDENTITY_PREAMBLE, AnthropicContentBlock, CACHE_MARKER_STABLE, CACHE_MARKER_TOOLS,
    CACHE_MARKER_VOLATILE, CacheControl, CacheType, StructuredApiMessage, StructuredContent,
    SystemContentBlock,
};
use crate::CacheTtl;

/// Build a `CacheControl` value for the given TTL variant.
///
/// For `None` or `Ephemeral`, the `ttl` field is omitted from the serialized output,
/// preserving byte-identical wire format with pre-feature requests.
pub(super) fn build_cache_control(ttl: Option<CacheTtl>) -> CacheControl {
    CacheControl {
        cache_type: CacheType::Ephemeral,
        ttl: match ttl {
            Some(CacheTtl::OneHour) => Some(CacheTtl::OneHour),
            Some(CacheTtl::Ephemeral) | None => None,
        },
    }
}

pub(super) fn log_cache_usage(usage: &super::types::ApiUsage) {
    tracing::debug!(
        input_tokens = usage.input_tokens,
        output_tokens = usage.output_tokens,
        cache_creation = usage.cache_creation_input_tokens,
        cache_read = usage.cache_read_input_tokens,
        "Claude API usage"
    );
}

/// Returns the minimum token count required for caching to activate for the given model.
/// Uses `byte_len / 4` as a conservative token estimate (1 token ≈ 4 chars for English).
pub(super) fn cache_min_tokens(model: &str) -> usize {
    if model.contains("sonnet") { 2048 } else { 4096 }
}

pub(super) fn tool_cache_key(tools: &[ToolDefinition]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for t in tools {
        t.name.hash(&mut hasher);
        t.description.hash(&mut hasher);
        t.parameters.to_string().hash(&mut hasher);
        t.output_schema
            .as_ref()
            .map(std::string::ToString::to_string)
            .hash(&mut hasher);
    }
    hasher.finish()
}

pub(super) fn split_system_into_blocks(
    system: &str,
    model: &str,
    ttl: Option<CacheTtl>,
) -> Vec<SystemContentBlock> {
    // Split on volatile marker first: everything before is cacheable
    let (cacheable_part, volatile_part) = if let Some(pos) = system.find(CACHE_MARKER_VOLATILE) {
        (
            &system[..pos],
            Some(&system[pos + CACHE_MARKER_VOLATILE.len()..]),
        )
    } else {
        (system, None)
    };

    let mut blocks = Vec::new();
    let cache_markers = [CACHE_MARKER_STABLE, CACHE_MARKER_TOOLS];
    let mut remaining = cacheable_part;
    let min_tokens = cache_min_tokens(model);

    let mut first_block = true;
    for marker in &cache_markers {
        if let Some(pos) = remaining.find(marker) {
            let before = remaining[..pos].trim();
            if !before.is_empty() {
                // Pad Block 1 (the stable base prompt) with agent identity text
                // when it is below the cache minimum threshold. This ensures
                // the block gets cache_control and avoids silent cache misses.
                let text = if first_block {
                    let estimated = estimate_tokens(before);
                    if estimated < min_tokens {
                        tracing::debug!(
                            estimated_tokens = estimated,
                            min_tokens,
                            model,
                            "Block 1 below cache threshold, padding with agent identity preamble"
                        );
                        format!("{before}\n{AGENT_IDENTITY_PREAMBLE}")
                    } else {
                        before.to_owned()
                    }
                } else {
                    before.to_owned()
                };
                let estimated_tokens = estimate_tokens(&text);
                let cc = if estimated_tokens >= min_tokens {
                    Some(build_cache_control(ttl))
                } else {
                    tracing::debug!(
                        estimated_tokens,
                        min_tokens,
                        model,
                        "system block below cache threshold, skipping cache_control"
                    );
                    None
                };
                blocks.push(SystemContentBlock {
                    block_type: "text",
                    text,
                    cache_control: cc,
                });
            }
            remaining = &remaining[pos + marker.len()..];
            first_block = false;
        }
    }

    let remaining = remaining.trim();
    if !remaining.is_empty() {
        // When markers were present, the trailing segment is always cached (it's the
        // last explicit cacheable block). When no markers exist, `remaining` equals the
        // full system prompt — apply the same min-token threshold as the fallback path.
        let had_markers = remaining.len() < cacheable_part.trim().len();
        let estimated_tokens = estimate_tokens(remaining);
        let cc = if had_markers || estimated_tokens >= min_tokens {
            Some(build_cache_control(ttl))
        } else {
            tracing::debug!(
                estimated_tokens,
                min_tokens,
                model,
                "fallback system block below cache threshold, skipping cache_control"
            );
            None
        };
        blocks.push(SystemContentBlock {
            block_type: "text",
            text: remaining.to_owned(),
            cache_control: cc,
        });
    }

    if let Some(volatile) = volatile_part {
        let volatile = volatile.trim();
        if !volatile.is_empty() {
            blocks.push(SystemContentBlock {
                block_type: "text",
                text: volatile.to_owned(),
                cache_control: None,
            });
        }
    }

    blocks
}

pub(super) fn apply_cache_breakpoint(chat: &mut [StructuredApiMessage], ttl: Option<CacheTtl>) {
    let target = chat.len().saturating_sub(20);
    let breakpoint_idx = (target..chat.len())
        .find(|&i| chat[i].role == "user")
        .unwrap_or(0);
    let msg = &mut chat[breakpoint_idx];
    match &mut msg.content {
        StructuredContent::Blocks(blocks) => {
            if let Some(
                AnthropicContentBlock::Text { cache_control, .. }
                | AnthropicContentBlock::ToolResult { cache_control, .. },
            ) = blocks.last_mut()
            {
                *cache_control = Some(build_cache_control(ttl));
            }
        }
        StructuredContent::Text(text) => {
            let owned = std::mem::take(text);
            msg.content = StructuredContent::Blocks(vec![AnthropicContentBlock::Text {
                text: owned,
                cache_control: Some(build_cache_control(ttl)),
            }]);
        }
    }
}
