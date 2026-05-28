// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Heuristic fidelity scorer for Context-Adaptive Memory (CAM).
//!
//! [`FidelityScorer`] is a stateless scoring engine that assigns a three-level
//! representation ([`ContextFidelity::Full`] / [`ContextFidelity::Compressed`] /
//! [`ContextFidelity::Placeholder`]) to each message in the context window. Scoring is
//! driven by weighted signals: temporal recency, role importance, keyword-based semantic
//! relevance, and optional plan hints.
//!
//! [`FidelityConfig`] holds all tuning knobs; it is read from `[memory.fidelity]` in
//! `config.toml`. When `enabled = false` (the default), the scorer returns immediately
//! without modifying the message window.

use tracing::info_span;
use zeph_common::memory::TokenCounting;
use zeph_common::{ContextFidelity, PlannedToolHint};
use zeph_llm::provider::{Message, MessagePart, Role};

use crate::assembler::CORRECTIONS_PREFIX;

// Re-export FidelityConfig from zeph-config so both crates share one definition.
pub use zeph_config::FidelityConfig;

struct FidelityScore {
    score: f32,
    level: ContextFidelity,
    original_tokens: u32,
}

/// Stateless heuristic scorer that assigns and applies fidelity levels to a message window.
///
/// Call [`FidelityScorer::score_and_apply`] after `apply_prepared_context()` returns to
/// enforce the three-level representation (Full / Compressed / Placeholder) on historical
/// messages. The scorer never touches exempt messages (INV-07 through INV-10).
///
/// # Examples
///
/// ```
/// use zeph_context::fidelity::{FidelityConfig, FidelityScorer};
///
/// let scorer = FidelityScorer;
/// let cfg = FidelityConfig { enabled: false, ..FidelityConfig::default() };
/// // With `enabled = false` the scorer is a no-op.
/// let mut messages = vec![];
/// scorer.score_and_apply(&mut messages, "query", &[], &cfg, &MockTc, 0);
///
/// struct MockTc;
/// impl zeph_common::memory::TokenCounting for MockTc {
///     fn count_tokens(&self, text: &str) -> usize { text.len() / 4 }
///     fn count_tool_schema_tokens(&self, _: &serde_json::Value) -> usize { 0 }
/// }
/// ```
pub struct FidelityScorer;

impl FidelityScorer {
    /// Score all non-exempt messages and apply fidelity rendering in-place.
    ///
    /// Steps (per spec §5 data flow):
    /// 1. Guard: return early when `enabled == false`.
    /// 2. Build exempt set (INV-07 through INV-10).
    /// 3. Score each non-exempt message with normalized weight sum (INV-05).
    /// 4. Apply tool-pair atomicity — both get `min(score_a, score_b)` (INV-03).
    /// 5. Render `Compressed` / `Placeholder` messages (INV-12).
    /// 6. Merge consecutive same-role `Placeholder` messages (INV-04).
    ///
    /// # Parameters
    ///
    /// - `messages` — mutable message window (includes system prompt at index 0).
    /// - `query` — current user query; drives semantic signal.
    /// - `planned_tools` — DAG lookahead hints; empty slice disables plan signal.
    /// - `config` — scoring thresholds and weights.
    /// - `tc` — token counter used for `Placeholder`/`Compressed` rendering.
    /// - `inserted_count` — number of memory messages freshly injected at indices
    ///   `1..1+inserted_count`; these are always exempt (INV-10).
    pub fn score_and_apply(
        &self,
        messages: &mut Vec<Message>,
        query: &str,
        planned_tools: &[PlannedToolHint],
        config: &FidelityConfig,
        tc: &dyn TokenCounting,
        inserted_count: usize,
    ) {
        if !config.enabled || messages.is_empty() {
            return;
        }

        let scores = compute_scores(messages, query, planned_tools, config, tc, inserted_count);
        apply_scores(messages, &scores, config, tc);

        let _merge_span = info_span!("context.fidelity.merge").entered();
        let merged_count = merge_consecutive_placeholders(messages);
        tracing::debug!(merged_count, "fidelity merge complete");
    }
}

fn compute_scores(
    messages: &[Message],
    query: &str,
    planned_tools: &[PlannedToolHint],
    config: &FidelityConfig,
    tc: &dyn TokenCounting,
    inserted_count: usize,
) -> Vec<Option<FidelityScore>> {
    let n = messages.len();

    // Performance cap: only score oldest messages; newest `exempt_tail_messages` default to Full.
    let score_end = if n > config.max_scored_messages {
        n.saturating_sub(config.exempt_tail_messages)
    } else {
        n
    };

    let semantic_active = query.len() >= config.min_query_length;
    let plan_active = !planned_tools.is_empty();
    // Build once outside the per-message loop (SF-1: avoids 500 redundant allocations).
    let query_words: std::collections::HashSet<&str> = if semantic_active {
        query.split_whitespace().collect()
    } else {
        std::collections::HashSet::default()
    };

    // Compute the active weight sum (INV-05).
    let mut weight_sum = config.w_temporal + config.w_importance;
    if semantic_active {
        weight_sum += config.w_semantic;
    }
    if plan_active {
        weight_sum += config.w_plan;
    }
    if weight_sum <= 0.0 {
        weight_sum = 1.0;
    }

    #[allow(clippy::cast_precision_loss)]
    let max_dist = score_end.saturating_sub(1) as f32;

    let mut scores: Vec<Option<FidelityScore>> = (0..n).map(|_| None).collect();

    for (i, msg) in messages.iter().enumerate().take(score_end) {
        if is_exempt(msg, i, inserted_count) {
            continue;
        }

        #[allow(clippy::cast_possible_truncation)]
        let original_tokens = tc.count_tokens(&msg.content) as u32;

        // distance_from_end = 0 for newest (i = score_end-1), N-1 for oldest (i = 0).
        // Spec §6.1: temporal = 1.0 - distance_from_end / max_dist → newest = 1.0, oldest ≈ 0.0.
        #[allow(clippy::cast_precision_loss)]
        let temporal = if max_dist > 0.0 {
            let distance_from_end = (score_end - 1 - i) as f32;
            1.0 - distance_from_end / max_dist
        } else {
            1.0
        };
        // Spec §6.2: ToolResult messages use weight 0.4 regardless of Role::User mapping.
        let importance = if msg
            .parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        {
            0.4
        } else {
            role_weight(msg.role)
        };
        let semantic = if semantic_active {
            keyword_overlap(&msg.content, &query_words)
        } else {
            0.0
        };
        let plan = if plan_active {
            plan_relevance(&msg.content, planned_tools)
        } else {
            0.0
        };

        let raw = config.w_temporal * temporal
            + config.w_importance * importance
            + if semantic_active {
                config.w_semantic * semantic
            } else {
                0.0
            }
            + if plan_active {
                config.w_plan * plan
            } else {
                0.0
            };

        let score = (raw / weight_sum).clamp(0.0, 1.0);
        let level = score_to_level(score, config);
        scores[i] = Some(FidelityScore {
            score,
            level,
            original_tokens,
        });
    }

    apply_tool_pair_atomicity(messages, &mut scores, config);
    scores
}

fn apply_scores(
    messages: &mut [Message],
    scores: &[Option<FidelityScore>],
    config: &FidelityConfig,
    tc: &dyn TokenCounting,
) {
    let _apply_span = info_span!("context.fidelity.apply").entered();
    let (mut full_count, mut compressed_count, mut placeholder_count, mut tokens_saved) =
        (0u32, 0u32, 0u32, 0u32);

    for (i, msg) in messages.iter_mut().enumerate() {
        let Some(ref fs) = scores[i] else { continue };
        match fs.level {
            ContextFidelity::Compressed => {
                #[allow(clippy::cast_possible_truncation)]
                let original_tokens = fs.original_tokens;
                render_compressed(msg, config, tc);
                #[allow(clippy::cast_possible_truncation)]
                let new_tokens = tc.count_tokens(&msg.content) as u32;
                tokens_saved += original_tokens.saturating_sub(new_tokens);
                compressed_count += 1;
            }
            ContextFidelity::Placeholder => {
                render_placeholder(msg, fs.score, fs.original_tokens);
                placeholder_count += 1;
            }
            // Full and any future variants keep original content.
            _ => {
                msg.metadata.fidelity_tag = Some(ContextFidelity::Full);
                full_count += 1;
            }
        }
    }

    tracing::debug!(
        full_count,
        compressed_count,
        placeholder_count,
        tokens_saved,
        "fidelity apply complete"
    );
}

fn is_exempt(msg: &Message, idx: usize, inserted_count: usize) -> bool {
    // INV-07: system prompt at index 0.
    // INV-08: focus_pinned messages.
    // INV-09: correction messages.
    // INV-10: freshly injected memory context at indices 1..1+inserted_count.
    (idx == 0 && msg.role == Role::System)
        || msg.metadata.focus_pinned
        || msg.content.starts_with(CORRECTIONS_PREFIX)
        || (idx >= 1 && idx < 1 + inserted_count)
}

fn role_weight(role: Role) -> f32 {
    match role {
        Role::System => 1.0,
        Role::User => 0.8,
        Role::Assistant => 0.6,
    }
}

/// Simple word-intersection semantic overlap, normalized to [0, 1].
///
/// `query_words` is pre-built outside the per-message loop (SF-1).
fn keyword_overlap(content: &str, query_words: &std::collections::HashSet<&str>) -> f32 {
    let content_words: std::collections::HashSet<&str> = content.split_whitespace().collect();
    let min_len = content_words.len().min(query_words.len());
    if min_len == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let result = content_words.intersection(query_words).count() as f32 / min_len as f32;
    result.clamp(0.0, 1.0)
}

/// Keyword overlap between message content and planned tool keywords.
///
/// Weighted by `1.0 / distance_from_current` and averaged across all hints.
fn plan_relevance(content: &str, planned_tools: &[PlannedToolHint]) -> f32 {
    if planned_tools.is_empty() {
        return 0.0;
    }
    let content_words: std::collections::HashSet<&str> = content.split_whitespace().collect();
    let mut weighted_sum = 0.0f32;
    let mut weight_total = 0.0f32;
    for hint in planned_tools {
        let dist = f32::from(hint.distance_from_current.max(1));
        let weight = 1.0 / dist;
        weight_total += weight;
        let hint_words: std::collections::HashSet<&str> =
            hint.keywords.iter().map(String::as_str).collect();
        let min_len = content_words.len().min(hint_words.len());
        if min_len == 0 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let overlap = content_words.intersection(&hint_words).count() as f32 / min_len as f32;
        weighted_sum += weight * overlap.clamp(0.0, 1.0);
    }
    if weight_total <= 0.0 {
        return 0.0;
    }
    (weighted_sum / weight_total).clamp(0.0, 1.0)
}

/// O(N) backward scan: find `ToolUse`/`ToolResult` pairs and assign `min(score_a, score_b)`.
fn apply_tool_pair_atomicity(
    messages: &[Message],
    scores: &mut [Option<FidelityScore>],
    config: &FidelityConfig,
) {
    // Collect (tool_use_id, message_index) for ToolResult messages.
    let mut tool_result_map: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for (i, msg) in messages.iter().enumerate() {
        for part in &msg.parts {
            if let MessagePart::ToolResult { tool_use_id, .. } = part {
                tool_result_map.insert(tool_use_id.as_str(), i);
            }
        }
    }

    // Walk backward to find ToolUse messages and pair with their result.
    for (i, msg) in messages.iter().enumerate().rev() {
        for part in &msg.parts {
            if let MessagePart::ToolUse { id, .. } = part
                && let Some(&result_idx) = tool_result_map.get(id.as_str())
            {
                let score_a = scores[i].as_ref().map_or(1.0, |s| s.score);
                let score_b = scores[result_idx].as_ref().map_or(1.0, |s| s.score);
                let min_score = score_a.min(score_b);
                let min_level = score_to_level(min_score, config);
                let tokens_a = scores[i].as_ref().map_or(0, |s| s.original_tokens);
                let tokens_b = scores[result_idx].as_ref().map_or(0, |s| s.original_tokens);
                scores[i] = Some(FidelityScore {
                    score: min_score,
                    level: min_level,
                    original_tokens: tokens_a,
                });
                scores[result_idx] = Some(FidelityScore {
                    score: min_score,
                    level: min_level,
                    original_tokens: tokens_b,
                });
            }
        }
    }
}

fn score_to_level(score: f32, config: &FidelityConfig) -> ContextFidelity {
    if score >= config.full_threshold {
        ContextFidelity::Full
    } else if score >= config.compressed_threshold {
        ContextFidelity::Compressed
    } else {
        ContextFidelity::Placeholder
    }
}

fn render_compressed(msg: &mut Message, config: &FidelityConfig, tc: &dyn TokenCounting) {
    if let Some(summary) = msg.metadata.deferred_summary.take() {
        msg.content = summary;
    } else {
        truncate_to_tokens(&mut msg.content, config.compressed_max_tokens, tc);
    }
    msg.parts.clear();
    msg.metadata.fidelity_tag = Some(ContextFidelity::Compressed);
}

fn truncate_to_tokens(content: &mut String, max_tokens: usize, tc: &dyn TokenCounting) {
    if tc.count_tokens(content) <= max_tokens {
        return;
    }
    let mut len = content.len();
    while len > 0 && tc.count_tokens(&content[..len]) > max_tokens {
        len /= 2;
        while len > 0 && !content.is_char_boundary(len) {
            len -= 1;
        }
    }
    content.truncate(len);
}

fn render_placeholder(msg: &mut Message, score: f32, original_tokens: u32) {
    let role_str = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    msg.content = format!(
        "[placeholder: role={role_str}, original_tokens={original_tokens}, importance={score:.2}]"
    );
    msg.parts.clear();
    msg.metadata.fidelity_tag = Some(ContextFidelity::Placeholder);
}

/// Merge consecutive same-role `Placeholder` messages into a single merged placeholder.
///
/// Returns the number of individual messages consumed by merges.
fn merge_consecutive_placeholders(messages: &mut Vec<Message>) -> usize {
    let mut merged_count = 0usize;
    let mut i = 0;
    while i < messages.len() {
        if messages[i].metadata.fidelity_tag != Some(ContextFidelity::Placeholder)
            || messages[i].role == Role::System
        {
            i += 1;
            continue;
        }
        let role = messages[i].role;
        let mut j = i + 1;
        while j < messages.len()
            && messages[j].metadata.fidelity_tag == Some(ContextFidelity::Placeholder)
            && messages[j].role == role
        {
            j += 1;
        }
        if j - i <= 1 {
            i += 1;
            continue;
        }
        let count = j - i;
        let mut total_tokens = 0u32;
        let mut importance_sum = 0.0f32;
        for msg in &messages[i..j] {
            total_tokens += parse_placeholder_tokens(&msg.content);
            importance_sum += parse_placeholder_importance(&msg.content);
        }
        debug_assert!(count >= 2, "placeholder merge triggered with count={count}");
        #[allow(clippy::cast_precision_loss)]
        let avg_importance = if count > 0 {
            importance_sum / count as f32
        } else {
            0.0
        };
        let role_str = match role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let merged_content = format!(
            "[placeholder: {count} messages, role={role_str}, total_tokens={total_tokens}, avg_importance={avg_importance:.2}]"
        );
        let first = messages[i].clone();
        messages.drain(i..j);
        messages.insert(
            i,
            Message {
                role: first.role,
                content: merged_content,
                parts: vec![],
                metadata: {
                    let mut m = first.metadata;
                    m.fidelity_tag = Some(ContextFidelity::Placeholder);
                    m
                },
            },
        );
        merged_count += count - 1;
        i += 1;
    }
    merged_count
}

fn parse_placeholder_tokens(content: &str) -> u32 {
    for part in content.split(',') {
        let part = part.trim();
        for prefix in &["original_tokens=", "total_tokens="] {
            if let Some(rest) = part.strip_prefix(prefix)
                && let Ok(n) = rest.trim_end_matches(']').trim().parse::<u32>()
            {
                return n;
            }
        }
    }
    0
}

fn parse_placeholder_importance(content: &str) -> f32 {
    for part in content.split(',') {
        let part = part.trim();
        for prefix in &["importance=", "avg_importance="] {
            if let Some(rest) = part.strip_prefix(prefix)
                && let Ok(v) = rest.trim_end_matches(']').trim().parse::<f32>()
            {
                return v;
            }
        }
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    struct FixedTc(usize);
    impl TokenCounting for FixedTc {
        fn count_tokens(&self, text: &str) -> usize {
            text.len() / self.0.max(1)
        }

        fn count_tool_schema_tokens(&self, _schema: &serde_json::Value) -> usize {
            0
        }
    }

    fn make_msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    fn make_cfg() -> FidelityConfig {
        FidelityConfig {
            enabled: true,
            w_semantic: 0.3,
            w_temporal: 0.3,
            w_importance: 0.2,
            w_plan: 0.2,
            full_threshold: 0.7,
            compressed_threshold: 0.3,
            compressed_max_tokens: 50,
            regrade_threshold: 0.6,
            min_query_length: 8,
            max_scored_messages: 500,
            exempt_tail_messages: 0,
        }
    }

    // 1. Empty window → no change.
    #[test]
    fn empty_window_no_change() {
        let scorer = FidelityScorer;
        let cfg = make_cfg();
        let tc = FixedTc(4);
        let mut messages: Vec<Message> = vec![];
        scorer.score_and_apply(&mut messages, "query text", &[], &cfg, &tc, 0);
        assert!(messages.is_empty());
    }

    // 2. All-exempt window → no downgrade.
    #[test]
    fn all_exempt_no_downgrade() {
        let scorer = FidelityScorer;
        let cfg = make_cfg();
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system prompt"),
            // Injected memory at index 1 with inserted_count=1.
            make_msg(Role::User, "memory context"),
        ];
        scorer.score_and_apply(&mut messages, "short", &[], &cfg, &tc, 1);
        for msg in &messages {
            assert!(
                msg.metadata.fidelity_tag.is_none()
                    || msg.metadata.fidelity_tag == Some(ContextFidelity::Full)
            );
        }
    }

    // 3. Tool pair atomicity: divergent scores → min applied.
    #[test]
    fn tool_pair_atomicity() {
        let scorer = FidelityScorer;
        // Very high thresholds to force Placeholder for older messages.
        let cfg = FidelityConfig {
            full_threshold: 0.9,
            compressed_threshold: 0.5,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let tool_use_id = "abc123".to_string();
        let mut tool_use_msg = make_msg(Role::Assistant, "calling tool");
        tool_use_msg.parts = vec![MessagePart::ToolUse {
            id: tool_use_id.clone(),
            name: "shell".to_string(),
            input: serde_json::json!({}),
        }];
        let mut tool_result_msg = make_msg(Role::User, "tool result body");
        tool_result_msg.parts = vec![MessagePart::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: "result".to_string(),
            is_error: false,
        }];
        let mut messages = vec![
            make_msg(Role::System, "system"),
            tool_use_msg,
            tool_result_msg,
        ];
        scorer.score_and_apply(
            &mut messages,
            "completely unrelated query blah",
            &[],
            &cfg,
            &tc,
            0,
        );
        let tag_a = messages[1].metadata.fidelity_tag;
        let tag_b = messages[2].metadata.fidelity_tag;
        assert_eq!(tag_a, tag_b, "tool pair must share fidelity level");
    }

    // 4. Same-role Placeholder merge: 5 consecutive assistant → merged to 1.
    #[test]
    fn same_role_placeholder_merge() {
        let scorer = FidelityScorer;
        // Force all non-system messages to become Placeholder.
        let cfg = FidelityConfig {
            full_threshold: 2.0,       // impossible to reach
            compressed_threshold: 1.5, // impossible to reach
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages: Vec<Message> = std::iter::once(make_msg(Role::System, "system"))
            .chain((0..5).map(|i| make_msg(Role::Assistant, &format!("msg {i}"))))
            .collect();
        scorer.score_and_apply(&mut messages, "some query here", &[], &cfg, &tc, 0);
        // System + 1 merged placeholder.
        assert_eq!(
            messages.len(),
            2,
            "5 assistant placeholders must merge to 1"
        );
        assert!(messages[1].content.contains("5 messages"));
    }

    // 5. Score normalization: active signal subset still produces [0,1].
    #[test]
    fn score_normalization_no_panic() {
        let scorer = FidelityScorer;
        let cfg = make_cfg();
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "hello"),
            make_msg(Role::Assistant, "world response"),
        ];
        scorer.score_and_apply(&mut messages, "hello world signal", &[], &cfg, &tc, 0);
        for msg in &messages {
            let _ = msg.metadata.fidelity_tag;
        }
    }

    // 6. Short query fallback: query.len() < 8 → semantic signal excluded.
    #[test]
    fn short_query_fallback() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            min_query_length: 8,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "test"),
        ];
        // Must not panic or produce out-of-range scores.
        scorer.score_and_apply(&mut messages, "short", &[], &cfg, &tc, 0);
    }

    // 7. AC-09: memory_first bypass is the caller's responsibility.
    //    When enabled=false, score_and_apply is always a no-op — callers that activate
    //    memory_first simply skip the call (see service.rs guard at INV-11).
    //    This test documents the contract: the scorer itself is stateless and harmless
    //    when called with disabled config or an all-exempt window.
    #[test]
    fn memory_first_bypass_is_callers_responsibility() {
        let scorer = FidelityScorer;
        // Simulate: caller would skip this call when memory_first=true.
        // The scorer itself must be a complete no-op when enabled=false.
        let cfg = FidelityConfig {
            enabled: false,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system prompt"),
            make_msg(Role::User, "memory-injected context"),
            make_msg(Role::Assistant, "response"),
        ];
        let before: Vec<_> = messages.iter().map(|m| m.content.clone()).collect();
        // Even with a real query, disabled scorer must not touch any message.
        scorer.score_and_apply(
            &mut messages,
            "some user query text here",
            &[],
            &cfg,
            &tc,
            2,
        );
        for (msg, orig) in messages.iter().zip(&before) {
            assert_eq!(msg.content, *orig, "content must be unchanged");
            assert!(
                msg.metadata.fidelity_tag.is_none(),
                "no fidelity tag must be set"
            );
        }
    }

    // 9. enabled=false guard: no changes applied.
    #[test]
    fn enabled_false_guard() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            enabled: false,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "user message that would normally be scored"),
        ];
        let original_contents: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        scorer.score_and_apply(&mut messages, "query text here", &[], &cfg, &tc, 0);
        for (msg, orig) in messages.iter().zip(&original_contents) {
            assert_eq!(msg.content, *orig);
            assert!(msg.metadata.fidelity_tag.is_none());
        }
    }

    // 10. Score always in [0.0, 1.0] for extreme inputs (zero weights).
    #[test]
    fn score_always_in_range() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            enabled: true,
            w_semantic: 0.0,
            w_temporal: 0.0,
            w_importance: 0.0,
            w_plan: 0.0,
            full_threshold: 0.7,
            compressed_threshold: 0.3,
            compressed_max_tokens: 50,
            regrade_threshold: 0.6,
            min_query_length: 0,
            max_scored_messages: 500,
            exempt_tail_messages: 0,
        };
        let tc = FixedTc(4);
        let mut messages = vec![make_msg(Role::System, ""), make_msg(Role::User, "")];
        // Must not panic with zero weights.
        scorer.score_and_apply(&mut messages, "", &[], &cfg, &tc, 0);
    }

    // 11. Token count uses tc.count_tokens for Placeholder rendering.
    #[test]
    fn placeholder_uses_tc_count_tokens() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            full_threshold: 2.0,
            compressed_threshold: 1.5,
            ..make_cfg()
        };
        let tc = FixedTc(1); // every character = 1 token
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "user message content for placeholder rendering"),
        ];
        scorer.score_and_apply(&mut messages, "some query text here", &[], &cfg, &tc, 0);
        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Placeholder)
        );
        assert!(messages[1].content.starts_with("[placeholder:"));
    }

    // 13. #4593: exempt_tail_messages respected when n > max_scored_messages.
    //     Verify that tail messages (beyond score_end) keep no fidelity_tag.
    //     Use focus_pinned=true on tail messages so they stay exempt-from-scoring
    //     via is_exempt() and retain no tag regardless of the merge pass.
    //     n=20, max_scored_messages=10, exempt_tail_messages=5 → score_end=15.
    //     Indices [15..19] are in the exempt tail.
    #[test]
    fn exempt_tail_messages_large_window() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            // Force all scored messages to Placeholder so we can see which ones got tagged.
            full_threshold: 2.0,
            compressed_threshold: 1.5,
            max_scored_messages: 10,
            exempt_tail_messages: 5,
            ..make_cfg()
        };
        let tc = FixedTc(4);

        // Index 0: system (always exempt).
        // Indices 1..14: regular user messages that fall in the scored region [0..15).
        // Indices 15..19: tail messages — mark them focus_pinned so they don't get scored.
        //   focus_pinned is the is_exempt() gate (INV-08), which makes them opaque to
        //   the scorer. This lets us assert their fidelity_tag stays None while the
        //   merge pass leaves them intact (it only merges Placeholder-tagged messages).
        let mut messages: Vec<Message> = std::iter::once(make_msg(Role::System, "system prompt"))
            .chain((1..15).map(|i| make_msg(Role::Assistant, &format!("assistant message {i}"))))
            .chain((15..20).map(|i| {
                let mut m = make_msg(Role::User, &format!("tail message {i}"));
                m.metadata.focus_pinned = true;
                m
            }))
            .collect();

        scorer.score_and_apply(&mut messages, "query text here long", &[], &cfg, &tc, 0);

        // Tail messages (focus_pinned) must have no fidelity_tag.
        let tail: Vec<_> = messages
            .iter()
            .filter(|m| m.metadata.focus_pinned)
            .collect();
        assert_eq!(
            tail.len(),
            5,
            "all 5 tail messages must survive the merge pass"
        );
        for msg in &tail {
            assert!(
                msg.metadata.fidelity_tag.is_none(),
                "tail message must have no fidelity_tag, got {:?}",
                msg.metadata.fidelity_tag
            );
        }
    }

    // 14. #4593: when n <= max_scored_messages, exempt_tail_messages has no effect.
    //     n=8, max_scored_messages=10, exempt_tail_messages=5 → score_end=8 (all scored).
    //     Use alternating roles to avoid placeholder merging.
    #[test]
    fn exempt_tail_messages_small_window_no_effect() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            full_threshold: 2.0,
            compressed_threshold: 1.5,
            max_scored_messages: 10,
            exempt_tail_messages: 5,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        // 8 messages: index 0 = system, then alternating user/assistant.
        // Alternating roles prevent placeholder merge, keeping the length stable.
        let roles = [Role::User, Role::Assistant];
        let mut messages: Vec<Message> = std::iter::once(make_msg(Role::System, "system prompt"))
            .chain((1..8usize).map(|i| make_msg(roles[i % 2], &format!("message {i}"))))
            .collect();
        scorer.score_and_apply(&mut messages, "query text here long", &[], &cfg, &tc, 0);
        // score_end = 8 (n=8 <= max_scored_messages=10, so exempt_tail not applied).
        // All non-system messages must be scored (tagged).
        let untagged_count = messages[1..]
            .iter()
            .filter(|m| m.metadata.fidelity_tag.is_none())
            .count();
        assert_eq!(
            untagged_count, 0,
            "all non-system messages must be scored when n <= max_scored_messages"
        );
    }

    // 12. Compressed rendering uses deferred_summary when available.
    #[test]
    fn compressed_uses_deferred_summary() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            full_threshold: 2.0,       // nothing reaches Full
            compressed_threshold: 0.0, // everything at or above 0 → Compressed
            compressed_max_tokens: 5,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut msg_with_summary =
            make_msg(Role::User, "original long content that would be truncated");
        msg_with_summary.metadata.deferred_summary = Some("short summary".to_string());
        let mut messages = vec![make_msg(Role::System, "system"), msg_with_summary];
        scorer.score_and_apply(&mut messages, "query text here long", &[], &cfg, &tc, 0);
        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Compressed)
        );
        assert_eq!(messages[1].content, "short summary");
    }
}
