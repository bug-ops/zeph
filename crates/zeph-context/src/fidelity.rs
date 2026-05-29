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
//!
//! ## Embed pre-pass
//!
//! The private `embed_prepass_dyn` helper runs concurrently via
//! `futures::stream::buffer_unordered` and populates per-message embedding vectors before
//! scoring. The public [`embed_prepass`] is a thin wrapper over the same helper for
//! closure-based callers (e.g. unit tests). The concurrency bound is controlled by
//! [`FidelityConfig::embed_concurrency`] (default 32).

use std::time::Duration;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use tracing::info_span;
use zeph_common::memory::TokenCounting;
use zeph_common::{ContextFidelity, PlannedToolHint};
use zeph_llm::LlmError;
use zeph_llm::LlmProviderDyn;
use zeph_llm::provider::{EmbedFuture, Message, MessageMetadata, MessagePart, Role};

use crate::assembler::CORRECTIONS_PREFIX;

// Re-export FidelityConfig from zeph-config so both crates share one definition.
pub use zeph_config::FidelityConfig;

/// Object-safe embed callback used by [`embed_prepass_dyn`].
///
/// Exists only to unify the `'static`-future closure path (used by [`embed_prepass`])
/// with the `&dyn LlmProviderDyn` path (used by [`FidelityScorer::score_and_apply`])
/// without an HRTB trait-object bound.
trait EmbedCall: Send + Sync {
    fn call<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, LlmError>>;
}

/// Adapts a `Fn(&str) -> EmbedFuture` (where `EmbedFuture` is `'static`) to [`EmbedCall`].
struct ClosureEmbed<F>(F);

impl<F> EmbedCall for ClosureEmbed<F>
where
    F: Fn(&str) -> EmbedFuture + Send + Sync,
{
    fn call<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, LlmError>> {
        // EmbedFuture is Pin<Box<dyn Future + Send>> — implicitly 'static, so
        // coercing to BoxFuture<'a, _> is always sound.
        Box::pin((self.0)(text))
    }
}

/// Adapts `&dyn LlmProviderDyn` to [`EmbedCall`].
struct ProviderEmbed<'p>(&'p dyn LlmProviderDyn);

impl EmbedCall for ProviderEmbed<'_> {
    fn call<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<Vec<f32>, LlmError>> {
        self.0.embed(text)
    }
}

/// Shared concurrent embed pipeline used by both [`embed_prepass`] and
/// [`FidelityScorer::score_and_apply`].
///
/// `messages` is the slice to embed (callers pre-slice to `[..score_end]` when needed).
/// Indices in the returned map are relative to the start of the passed slice.
async fn embed_prepass_dyn(
    messages: &[Message],
    embed: &dyn EmbedCall,
    config: &FidelityConfig,
    inserted_count: usize,
) -> std::collections::HashMap<usize, Vec<f32>> {
    let concurrency = if config.embed_concurrency == 0 {
        tracing::warn!(
            "embed_concurrency is 0, clamping to 1; set a positive value in [memory.fidelity]"
        );
        1
    } else {
        config.embed_concurrency
    };

    let tasks = messages.iter().enumerate().filter_map(|(i, msg)| {
        if is_exempt(msg, i, inserted_count)
            || msg.content.is_empty()
            || msg.metadata.embedding.is_some()
        {
            return None;
        }
        let content = match config.max_embed_input_tokens {
            Some(n) => truncate_to_byte_limit(&msg.content, n.saturating_mul(4)),
            None => msg.content.clone(),
        };
        Some((i, content))
    });

    let timeout = Duration::from_secs(config.embed_timeout_secs);
    futures::stream::iter(tasks)
        .map(|(i, content)| async move {
            let result = tokio::time::timeout(timeout, embed.call(&content)).await;
            match result {
                Ok(Ok(vec)) => Some((i, vec)),
                Ok(Err(e)) => {
                    tracing::warn!(idx = i, err = %e, "embed_prepass: embed failed, skipping");
                    None
                }
                Err(_) => {
                    tracing::warn!(idx = i, "embed_prepass: embed timed out, skipping");
                    None
                }
            }
        })
        .buffer_unordered(concurrency)
        .filter_map(|opt| async move { opt })
        .collect()
        .await
}

/// Run embed calls for `messages` concurrently, returning per-index embedding vectors.
///
/// Indexes not included in the result either had no content, already had an embedding
/// cached in `msg.metadata.embedding`, or produced an error (errors are logged at `warn`
/// level and silently skipped — scoring falls back to keyword overlap for those messages).
/// Each embed call is bounded by [`FidelityConfig::embed_timeout_secs`]; timed-out messages
/// are skipped with a `warn`-level log.
///
/// Only exempt messages (focus-pinned, inserted memory, system) are skipped; for non-exempt messages the content
/// is optionally truncated to `config.max_embed_input_tokens * 4` bytes (at a char boundary)
/// before the call.
///
/// Concurrency is bounded by [`FidelityConfig::embed_concurrency`] (default 32), which
/// maps to the `buffer_unordered(N)` parameter. A zero value is clamped to 1 with a warning.
///
/// # Examples
///
/// ```no_run
/// use zeph_context::fidelity::{embed_prepass, FidelityConfig};
///
/// async fn example() {
///     let messages = vec![];
///     let cfg = FidelityConfig::default();
///     let embed = |_text: &str| -> zeph_llm::provider::EmbedFuture {
///         Box::pin(async { Ok(vec![0.0f32; 384]) })
///     };
///     let _embeddings = embed_prepass(&messages, &embed, &cfg, 0).await;
/// }
/// ```
pub async fn embed_prepass<F>(
    messages: &[Message],
    embed: &F,
    config: &FidelityConfig,
    inserted_count: usize,
) -> std::collections::HashMap<usize, Vec<f32>>
where
    F: Fn(&str) -> EmbedFuture + Send + Sync,
{
    embed_prepass_dyn(messages, &ClosureEmbed(embed), config, inserted_count).await
}

/// Truncate `s` to at most `max_bytes` bytes, landing on a valid UTF-8 char boundary.
///
/// The limit is a byte count, not a character count. Callers using a token approximation
/// should pass `n * 4` (the 4-byte-per-token heuristic) via [`saturating_mul`](usize::saturating_mul).
/// Uses [`str::floor_char_boundary`] (stable since Rust 1.91) so multibyte characters
/// are never split.
///
/// Returns a new `String`; does not modify the original.
fn truncate_to_byte_limit(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let boundary = s.floor_char_boundary(max_bytes);
    s[..boundary].to_string()
}

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
/// ```no_run
/// use zeph_context::fidelity::{FidelityConfig, FidelityScorer};
///
/// # async fn run() {
/// let scorer = FidelityScorer;
/// let cfg = FidelityConfig { enabled: false, ..FidelityConfig::default() };
/// // With `enabled = false` the scorer is a no-op.
/// let mut messages = vec![];
/// scorer.score_and_apply(&mut messages, "query", &[], &cfg, &MockTc, 0, false, None, None).await;
///
/// struct MockTc;
/// impl zeph_common::memory::TokenCounting for MockTc {
///     fn count_tokens(&self, text: &str) -> usize { text.len() / 4 }
///     fn count_tool_schema_tokens(&self, _: &serde_json::Value) -> usize { 0 }
/// }
/// # }
/// ```
pub struct FidelityScorer;

impl FidelityScorer {
    /// Score all non-exempt messages and apply fidelity rendering in-place.
    ///
    /// Steps (per spec §5 data flow):
    /// 1. Guard: return early when `enabled == false`.
    /// 2. Build exempt set (INV-07 through INV-10).
    /// 3. Score each non-exempt message with normalized weight sum (INV-05).
    /// 4. Apply floor invariant: a message with a persisted fidelity tag cannot
    ///    be upgraded to a less restrictive level unless `allow_upgrade = true`.
    /// 5. Apply tool-pair atomicity — both get `min(score_a, score_b)` (INV-03).
    /// 6. Render `Compressed` / `Placeholder` messages (INV-12).
    /// 7. Merge consecutive same-role `Placeholder` messages (INV-04).
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
    /// - `allow_upgrade` — when `true` the persisted floor invariant is bypassed.
    ///   Pass `true` only from the proactive regrade path; pass `false` everywhere else.
    /// - `embed_provider` — optional LLM provider used for query and per-message embeddings
    ///   when `config.semantic_scoring_provider` is set. Pass `None` to fall back to keyword
    ///   overlap.
    /// - `compress_provider` — optional LLM provider used for `Compressed` rendering when
    ///   `config.compress_provider` is set. Pass `None` to fall back to truncation.
    ///
    /// The two providers may be the same instance or different ones (e.g. a fast embedding
    /// model for `embed_provider` and a generative model for `compress_provider`).
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub async fn score_and_apply(
        &self,
        messages: &mut Vec<Message>,
        query: &str,
        planned_tools: &[PlannedToolHint],
        config: &FidelityConfig,
        tc: &dyn TokenCounting,
        inserted_count: usize,
        allow_upgrade: bool,
        embed_provider: Option<&dyn LlmProviderDyn>,
        compress_provider: Option<&dyn LlmProviderDyn>,
    ) {
        if !config.enabled || messages.is_empty() {
            return;
        }

        // Embed query once when semantic provider is configured.
        let query_embedding: Option<Vec<f32>> = if let (true, Some(p)) =
            (config.semantic_scoring_provider.is_some(), embed_provider)
            && p.supports_embeddings()
        {
            let _span = info_span!("context.fidelity.embed_query").entered();
            match tokio::time::timeout(
                Duration::from_secs(config.embed_timeout_secs),
                p.embed(query),
            )
            .await
            {
                Ok(Ok(v)) => Some(v),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "semantic scoring provider unavailable, falling back to keyword");
                    None
                }
                Err(_) => {
                    tracing::warn!("fidelity query embed timed out, falling back to keyword");
                    None
                }
            }
        } else {
            None
        };

        // Concurrent embed pre-pass: populate missing embeddings for the scored window.
        // Uses buffer_unordered so all N embed calls run in parallel (bounded by
        // embed_concurrency), replacing the former sequential O(N) loop.
        if let (Some(q_emb), Some(p)) = (&query_embedding, embed_provider) {
            let n = messages.len();
            let score_end = if n > config.max_scored_messages {
                n.saturating_sub(config.exempt_tail_messages)
            } else {
                n
            };
            let _span = info_span!("context.fidelity.embed_prepass").entered();
            let embeddings = embed_prepass_dyn(
                &messages[..score_end],
                &ProviderEmbed(p),
                config,
                inserted_count,
            )
            .await;
            for (i, emb) in embeddings {
                messages[i].metadata.embedding = Some(emb);
            }
            let _ = q_emb; // used below in compute_scores
        }

        let scores = compute_scores(
            messages,
            query,
            planned_tools,
            config,
            tc,
            inserted_count,
            allow_upgrade,
            query_embedding.as_deref(),
        );
        apply_scores(messages, &scores, config, tc, compress_provider).await;

        let _merge_span = info_span!("context.fidelity.merge").entered();
        let merged_count = merge_consecutive_placeholders(messages);
        tracing::debug!(merged_count, "fidelity merge complete");
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_scores(
    messages: &[Message],
    query: &str,
    planned_tools: &[PlannedToolHint],
    config: &FidelityConfig,
    tc: &dyn TokenCounting,
    inserted_count: usize,
    allow_upgrade: bool,
    query_embedding: Option<&[f32]>,
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
            match (query_embedding, msg.metadata.embedding.as_deref()) {
                (Some(q_emb), Some(m_emb)) => semantic_overlap(m_emb, q_emb),
                _ => keyword_overlap(&msg.content, &query_words),
            }
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
        let candidate_level = score_to_level(score, config);

        // Floor invariant (CAM Phase 2-B): a persisted fidelity tag constrains upgrades.
        // A message previously scored as Compressed cannot be upgraded back to Full;
        // Placeholder cannot be upgraded to Full or Compressed.
        // The regrade path passes allow_upgrade=true to bypass this constraint.
        let level = if allow_upgrade {
            candidate_level
        } else {
            match msg.metadata.fidelity_tag {
                Some(ContextFidelity::Placeholder) => ContextFidelity::Placeholder,
                Some(ContextFidelity::Compressed) => {
                    if candidate_level == ContextFidelity::Full {
                        ContextFidelity::Compressed
                    } else {
                        candidate_level
                    }
                }
                _ => candidate_level,
            }
        };

        scores[i] = Some(FidelityScore {
            score,
            level,
            original_tokens,
        });
    }

    apply_tool_pair_atomicity(messages, &mut scores, config);
    scores
}

async fn apply_scores(
    messages: &mut [Message],
    scores: &[Option<FidelityScore>],
    config: &FidelityConfig,
    tc: &dyn TokenCounting,
    provider: Option<&dyn LlmProviderDyn>,
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
                render_compressed(msg, config, tc, provider).await;
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

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(0.0, 1.0)
}

fn semantic_overlap(msg_embedding: &[f32], query_embedding: &[f32]) -> f32 {
    cosine_similarity(msg_embedding, query_embedding)
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
///
/// The "min" is computed over both the float score and the already-floored level, so
/// the floor invariant set in `compute_scores` is respected (INV-03 + floor invariant).
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

                // The level is the more restrictive of the two already-floored levels.
                // Taking min(float) alone would bypass the floor invariant for the pair
                // because score_to_level(min_score) ignores persisted fidelity tags.
                let level_a = scores[i]
                    .as_ref()
                    .map_or(ContextFidelity::Full, |s| s.level);
                let level_b = scores[result_idx]
                    .as_ref()
                    .map_or(ContextFidelity::Full, |s| s.level);
                let float_level = score_to_level(min_score, config);
                let min_level = more_restrictive(more_restrictive(level_a, level_b), float_level);

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

/// Return the more restrictive of two fidelity levels.
///
/// Restrictiveness order: Placeholder > Compressed > Full.
fn more_restrictive(a: ContextFidelity, b: ContextFidelity) -> ContextFidelity {
    use ContextFidelity::{Compressed, Full, Placeholder};
    match (a, b) {
        (Placeholder, _) | (_, Placeholder) => Placeholder,
        (Compressed, _) | (_, Compressed) => Compressed,
        _ => Full,
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

async fn render_compressed(
    msg: &mut Message,
    config: &FidelityConfig,
    tc: &dyn TokenCounting,
    provider: Option<&dyn LlmProviderDyn>,
) {
    // Priority 1: use pre-computed deferred summary when available.
    if let Some(summary) = msg.metadata.deferred_summary.take() {
        msg.content = summary;
    } else if config.compress_provider.is_some()
        && let Some(p) = provider
    {
        // Priority 2: LLM-assisted compression when provider and config name are set.
        let input_tokens = tc.count_tokens(&msg.content);
        // Guard: skip LLM if input is already small or within 2x of the max budget.
        if input_tokens > config.compressed_max_tokens * 2 && input_tokens > 0 {
            // Apply input cap before sending to LLM.
            if let Some(max_in) = config.max_compress_input_tokens {
                apply_input_cap(&mut msg.content, max_in);
            }

            let prompt = format!(
                "Summarize in {} tokens or fewer: {}",
                config.compressed_max_tokens, msg.content
            );
            let req = vec![Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];

            let span = info_span!(
                "context.fidelity.compress_llm",
                input_tokens,
                cached = false,
            );
            let result = {
                let _enter = span.enter();
                tokio::time::timeout(
                    Duration::from_secs(config.compress_timeout_secs),
                    p.chat(&req),
                )
                .await
            };

            match result {
                Ok(Ok(summary)) => {
                    msg.metadata.deferred_summary = Some(summary.clone());
                    msg.content = summary;
                }
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, "compress_llm failed, falling back to truncation");
                }
                Err(_) => {
                    tracing::warn!("compress_llm timed out, falling back to truncation");
                }
            }
        }
    } else if let Some(max_in) = config.max_compress_input_tokens {
        // Cap input before truncation so pathologically large messages don't blow the budget.
        apply_input_cap(&mut msg.content, max_in);
    }

    // Always cap output to compressed_max_tokens — covers both the deferred-summary path
    // (LLM output may exceed the limit) and the truncation-only path.
    truncate_to_tokens(&mut msg.content, config.compressed_max_tokens, tc);
    msg.parts.clear();
    msg.metadata.fidelity_tag = Some(ContextFidelity::Compressed);
}

/// Truncate `content` in place using the `max_tokens * 4` byte approximation.
///
/// Truncates at a valid UTF-8 char boundary via [`str::floor_char_boundary`]. Uses
/// [`saturating_mul`](usize::saturating_mul) to avoid overflow on extreme token counts.
///
/// Pass `config.max_compress_input_tokens` as `max_tokens` before an LLM compress call,
/// or `config.max_embed_input_tokens` before an embed call.
pub fn apply_input_cap(content: &mut String, max_tokens: usize) {
    let max_bytes = max_tokens.saturating_mul(4);
    if content.len() > max_bytes {
        let boundary = content.floor_char_boundary(max_bytes);
        content.truncate(boundary);
    }
}

fn truncate_to_tokens(content: &mut String, max_tokens: usize, tc: &dyn TokenCounting) {
    if tc.count_tokens(content) <= max_tokens {
        return;
    }
    // Binary search for the largest valid prefix that fits within max_tokens.
    // lo: largest known-safe boundary (count <= max_tokens).
    // hi: upper bound; count > max_tokens on the normal branch; on the stall branch
    //     hi collapses to lo and the loop exits immediately.
    let mut lo: usize = 0;
    let mut hi: usize = content.len();
    while hi - lo > 1 {
        let mid = content.floor_char_boundary(usize::midpoint(lo, hi));
        if mid == lo {
            // floor_char_boundary landed back on lo (multibyte char spans the midpoint).
            // Collapsing hi to lo satisfies hi-lo <= 1 and exits the loop.
            hi = mid;
        } else if tc.count_tokens(&content[..mid]) <= max_tokens {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    content.truncate(lo);
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
    use zeph_common::ProviderName;
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
            compress_provider: None,
            semantic_scoring_provider: None,
            lookahead_depth: 3,
            embed_concurrency: 32,
            max_embed_input_tokens: None,
            max_compress_input_tokens: None,
            embed_timeout_secs: 30,
            compress_timeout_secs: 30,
        }
    }

    // 1. Empty window → no change.
    #[tokio::test]
    async fn empty_window_no_change() {
        let scorer = FidelityScorer;
        let cfg = make_cfg();
        let tc = FixedTc(4);
        let mut messages: Vec<Message> = vec![];
        scorer
            .score_and_apply(
                &mut messages,
                "query text",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        assert!(messages.is_empty());
    }

    // 2. All-exempt window → no downgrade.
    #[tokio::test]
    async fn all_exempt_no_downgrade() {
        let scorer = FidelityScorer;
        let cfg = make_cfg();
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system prompt"),
            // Injected memory at index 1 with inserted_count=1.
            make_msg(Role::User, "memory context"),
        ];
        scorer
            .score_and_apply(&mut messages, "short", &[], &cfg, &tc, 1, false, None, None)
            .await;
        for msg in &messages {
            assert!(
                msg.metadata.fidelity_tag.is_none()
                    || msg.metadata.fidelity_tag == Some(ContextFidelity::Full)
            );
        }
    }

    // 3. Tool pair atomicity: divergent scores → min applied.
    #[tokio::test]
    async fn tool_pair_atomicity() {
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
        scorer
            .score_and_apply(
                &mut messages,
                "completely unrelated query blah",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        let tag_a = messages[1].metadata.fidelity_tag;
        let tag_b = messages[2].metadata.fidelity_tag;
        assert_eq!(tag_a, tag_b, "tool pair must share fidelity level");
    }

    // 4. Same-role Placeholder merge: 5 consecutive assistant → merged to 1.
    #[tokio::test]
    async fn same_role_placeholder_merge() {
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
        scorer
            .score_and_apply(
                &mut messages,
                "some query here",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        // System + 1 merged placeholder.
        assert_eq!(
            messages.len(),
            2,
            "5 assistant placeholders must merge to 1"
        );
        assert!(messages[1].content.contains("5 messages"));
    }

    // 5. Score normalization: active signal subset still produces [0,1].
    #[tokio::test]
    async fn score_normalization_no_panic() {
        let scorer = FidelityScorer;
        let cfg = make_cfg();
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "hello"),
            make_msg(Role::Assistant, "world response"),
        ];
        scorer
            .score_and_apply(
                &mut messages,
                "hello world signal",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        for msg in &messages {
            let _ = msg.metadata.fidelity_tag;
        }
    }

    // 6. Short query fallback: query.len() < 8 → semantic signal excluded.
    #[tokio::test]
    async fn short_query_fallback() {
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
        scorer
            .score_and_apply(&mut messages, "short", &[], &cfg, &tc, 0, false, None, None)
            .await;
    }

    // 7. AC-09: memory_first bypass is the caller's responsibility.
    //    When enabled=false, score_and_apply is always a no-op — callers that activate
    //    memory_first simply skip the call (see service.rs guard at INV-11).
    //    This test documents the contract: the scorer itself is stateless and harmless
    //    when called with disabled config or an all-exempt window.
    #[tokio::test]
    async fn memory_first_bypass_is_callers_responsibility() {
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
        scorer
            .score_and_apply(
                &mut messages,
                "some user query text here",
                &[],
                &cfg,
                &tc,
                2,
                false,
                None,
                None,
            )
            .await;
        for (msg, orig) in messages.iter().zip(&before) {
            assert_eq!(msg.content, *orig, "content must be unchanged");
            assert!(
                msg.metadata.fidelity_tag.is_none(),
                "no fidelity tag must be set"
            );
        }
    }

    // 9. enabled=false guard: no changes applied.
    #[tokio::test]
    async fn enabled_false_guard() {
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
        scorer
            .score_and_apply(
                &mut messages,
                "query text here",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        for (msg, orig) in messages.iter().zip(&original_contents) {
            assert_eq!(msg.content, *orig);
            assert!(msg.metadata.fidelity_tag.is_none());
        }
    }

    // 10. Score always in [0.0, 1.0] for extreme inputs (zero weights).
    #[tokio::test]
    async fn score_always_in_range() {
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
            compress_provider: None,
            semantic_scoring_provider: None,
            lookahead_depth: 3,
            embed_concurrency: 32,
            max_embed_input_tokens: None,
            max_compress_input_tokens: None,
            embed_timeout_secs: 30,
            compress_timeout_secs: 30,
        };
        let tc = FixedTc(4);
        let mut messages = vec![make_msg(Role::System, ""), make_msg(Role::User, "")];
        // Must not panic with zero weights.
        scorer
            .score_and_apply(&mut messages, "", &[], &cfg, &tc, 0, false, None, None)
            .await;
    }

    // 11. Token count uses tc.count_tokens for Placeholder rendering.
    #[tokio::test]
    async fn placeholder_uses_tc_count_tokens() {
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
        scorer
            .score_and_apply(
                &mut messages,
                "some query text here",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
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
    #[tokio::test]
    async fn exempt_tail_messages_large_window() {
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

        scorer
            .score_and_apply(
                &mut messages,
                "query text here long",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;

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
    #[tokio::test]
    async fn exempt_tail_messages_small_window_no_effect() {
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
        scorer
            .score_and_apply(
                &mut messages,
                "query text here long",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
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
    #[tokio::test]
    async fn compressed_uses_deferred_summary() {
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
        scorer
            .score_and_apply(
                &mut messages,
                "query text here long",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Compressed)
        );
        assert_eq!(messages[1].content, "short summary");
    }

    // ── CAM Phase 2-B: floor invariant tests ────────────────────────────────

    fn make_msg_with_fidelity(role: Role, content: &str, tag: Option<ContextFidelity>) -> Message {
        let mut m = make_msg(role, content);
        m.metadata.fidelity_tag = tag;
        m
    }

    // Floor: Compressed cannot upgrade to Full when allow_upgrade=false.
    #[tokio::test]
    async fn floor_prevents_compressed_upgrade_to_full() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            // Very low thresholds so every message naturally scores Full.
            full_threshold: 0.0,
            compressed_threshold: -1.0,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg_with_fidelity(
                Role::User,
                "query text here long keyword",
                Some(ContextFidelity::Compressed),
            ),
        ];
        scorer
            .score_and_apply(
                &mut messages,
                "query text here long keyword",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Compressed),
            "Compressed floor must block upgrade to Full"
        );
    }

    // Floor: Placeholder cannot upgrade to Full.
    #[tokio::test]
    async fn floor_prevents_placeholder_upgrade_to_full() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            full_threshold: 0.0,
            compressed_threshold: -1.0,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg_with_fidelity(
                Role::User,
                "query text here long keyword",
                Some(ContextFidelity::Placeholder),
            ),
        ];
        scorer
            .score_and_apply(
                &mut messages,
                "query text here long keyword",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Placeholder),
            "Placeholder floor must block upgrade to Full"
        );
    }

    // Floor: Placeholder cannot upgrade to Compressed.
    #[tokio::test]
    async fn floor_prevents_placeholder_upgrade_to_compressed() {
        let scorer = FidelityScorer;
        // Thresholds: full=2.0 (unreachable), compressed=0.0 (everything Compressed).
        let cfg = FidelityConfig {
            full_threshold: 2.0,
            compressed_threshold: 0.0,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg_with_fidelity(
                Role::User,
                "message content",
                Some(ContextFidelity::Placeholder),
            ),
        ];
        scorer
            .score_and_apply(
                &mut messages,
                "query text here long",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Placeholder),
            "Placeholder floor must block upgrade to Compressed"
        );
    }

    // Floor: further downgrade (Compressed → Placeholder) is allowed.
    #[tokio::test]
    async fn floor_allows_further_downgrade() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            full_threshold: 2.0,
            compressed_threshold: 2.0, // everything Placeholder
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg_with_fidelity(
                Role::User,
                "some content",
                Some(ContextFidelity::Compressed),
            ),
        ];
        scorer
            .score_and_apply(
                &mut messages,
                "query text here long",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Placeholder),
            "downgrade from Compressed to Placeholder must be allowed"
        );
    }

    // Floor: None tag → no constraint, normal scoring.
    #[tokio::test]
    async fn floor_no_constraint_when_none() {
        let scorer = FidelityScorer;
        // Force every message to Full.
        let cfg = FidelityConfig {
            full_threshold: 0.0,
            compressed_threshold: -1.0,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg_with_fidelity(Role::User, "query text here long keyword", None),
        ];
        scorer
            .score_and_apply(
                &mut messages,
                "query text here long keyword",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Full),
            "None tag must not constrain scoring"
        );
    }

    // allow_upgrade=true bypasses the Placeholder floor.
    #[tokio::test]
    async fn allow_upgrade_bypasses_floor() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            full_threshold: 0.0,
            compressed_threshold: -1.0,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg_with_fidelity(
                Role::User,
                "query text here long keyword",
                Some(ContextFidelity::Placeholder),
            ),
        ];
        scorer
            .score_and_apply(
                &mut messages,
                "query text here long keyword",
                &[],
                &cfg,
                &tc,
                0,
                true,
                None,
                None,
            )
            .await;
        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Full),
            "allow_upgrade=true must bypass the Placeholder floor"
        );
    }

    // ── truncate_to_tokens unit tests ─────────────────────────────────────────

    // no-op when content is well below limit
    #[test]
    fn truncate_no_op_below_limit() {
        let tc = FixedTc(1); // 1 char = 1 token
        let mut s = "hello".to_string(); // 5 tokens
        truncate_to_tokens(&mut s, 10, &tc);
        assert_eq!(s, "hello");
    }

    // no-op when content is exactly at limit
    #[test]
    fn truncate_no_op_at_limit() {
        let tc = FixedTc(1);
        let mut s = "hello".to_string(); // 5 tokens
        truncate_to_tokens(&mut s, 5, &tc);
        assert_eq!(s, "hello");
    }

    // truncates when content is one token over the limit
    #[test]
    fn truncate_minimal_one_over_limit() {
        let tc = FixedTc(1); // 1 char = 1 token
        let mut s = "abcdef".to_string(); // 6 tokens, limit=5
        truncate_to_tokens(&mut s, 5, &tc);
        assert!(
            tc.count_tokens(&s) <= 5,
            "result must fit in 5 tokens, got {}",
            tc.count_tokens(&s)
        );
        assert!(!s.is_empty(), "must keep prefix, not empty");
    }

    // binary search preserves more than halving would: at 90% of limit no truncation occurs
    #[test]
    fn truncate_preserves_90pct_of_limit() {
        // FixedTc(1): each byte = 1 token; 90-byte string with limit=100 must not be truncated.
        let tc = FixedTc(1);
        let s_orig = "a".repeat(90);
        let mut s = s_orig.clone();
        truncate_to_tokens(&mut s, 100, &tc);
        assert_eq!(s, s_orig, "90% of limit must not be truncated");
    }

    // empty string is a no-op
    #[test]
    fn truncate_empty_string_no_op() {
        let tc = FixedTc(1);
        let mut s = String::new();
        truncate_to_tokens(&mut s, 5, &tc);
        assert!(s.is_empty());
    }

    // max_tokens=0 truncates everything
    #[test]
    fn truncate_max_tokens_zero_clears_content() {
        let tc = FixedTc(1);
        let mut s = "hello world".to_string();
        truncate_to_tokens(&mut s, 0, &tc);
        assert!(s.is_empty(), "max_tokens=0 must clear content");
    }

    // multibyte characters: truncation must land on a valid char boundary
    #[test]
    fn truncate_multibyte_stays_on_char_boundary() {
        // "日本語" = 3 chars, each 3 bytes (9 bytes total).
        // FixedTc(3): count = byte_len / 3, so "日本語" = 3 tokens.
        // limit=2 → must truncate to "日本" (2 chars, 6 bytes).
        let tc = FixedTc(3);
        let mut s = "日本語".to_string();
        truncate_to_tokens(&mut s, 2, &tc);
        assert!(
            s.is_char_boundary(s.len()),
            "result must be on a valid char boundary"
        );
        assert!(tc.count_tokens(&s) <= 2);
        assert_eq!(s, "日本");
    }

    // Mixed-fidelity tool pair: None + Some(Compressed) → both end up Compressed via atomicity.
    #[tokio::test]
    async fn mixed_fidelity_tool_pair_floor_plus_atomicity() {
        let scorer = FidelityScorer;
        // Force everything to Full naturally, so the floor is the only downward pressure.
        let cfg = FidelityConfig {
            full_threshold: 0.0,
            compressed_threshold: -1.0,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let tool_id = "tool-42".to_string();

        let mut tool_use_msg = make_msg_with_fidelity(Role::Assistant, "call tool", None);
        tool_use_msg.parts = vec![MessagePart::ToolUse {
            id: tool_id.clone(),
            name: "shell".to_string(),
            input: serde_json::json!({}),
        }];

        let mut tool_result_msg =
            make_msg_with_fidelity(Role::User, "result body", Some(ContextFidelity::Compressed));
        tool_result_msg.parts = vec![MessagePart::ToolResult {
            tool_use_id: tool_id.clone(),
            content: "output".to_string(),
            is_error: false,
        }];

        let mut messages = vec![
            make_msg(Role::System, "system"),
            tool_use_msg,
            tool_result_msg,
        ];

        scorer
            .score_and_apply(
                &mut messages,
                "query text here long",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;

        // After floor clamping: tool_use scores Full (no floor), tool_result scores Full but
        // is clamped to Compressed by its floor. Atomicity then takes min(Full, Compressed) =
        // Compressed for both.
        let tag_use = messages[1].metadata.fidelity_tag;
        let tag_result = messages[2].metadata.fidelity_tag;
        assert_eq!(
            tag_use, tag_result,
            "tool pair must share the same fidelity level"
        );
        assert_eq!(
            tag_use,
            Some(ContextFidelity::Compressed),
            "atomicity must bring the tool-use down to the tool-result floor"
        );
    }

    // 15. LLM path stores deferred_summary and updates content.
    #[tokio::test]
    async fn compress_llm_path_stores_deferred_summary() {
        use zeph_llm::LlmError;
        use zeph_llm::provider::ChatStream;

        #[derive(Debug)]
        struct MockProvider;

        impl zeph_llm::provider::LlmProvider for MockProvider {
            async fn chat(&self, _messages: &[Message]) -> Result<String, LlmError> {
                Ok("summary text".to_string())
            }

            async fn chat_stream(&self, _messages: &[Message]) -> Result<ChatStream, LlmError> {
                Err(LlmError::Unavailable)
            }

            fn supports_streaming(&self) -> bool {
                false
            }

            async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
                Err(LlmError::EmbedUnsupported {
                    provider: "mock".into(),
                })
            }

            fn supports_embeddings(&self) -> bool {
                false
            }

            fn name(&self) -> &'static str {
                "mock"
            }
        }

        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            enabled: true,
            // Force everything to Compressed.
            full_threshold: 2.0,
            compressed_threshold: 0.0,
            compressed_max_tokens: 5,
            compress_provider: Some(ProviderName::new("mock")),
            ..make_cfg()
        };
        // Use tc with chars-per-token=1 so a long string has more than 5*2=10 tokens.
        let tc = FixedTc(1);
        let content = "a".repeat(50); // 50 chars → 50 tokens → well above 5*2=10
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, &content),
        ];

        let provider = MockProvider;
        scorer
            .score_and_apply(
                &mut messages,
                "some query text here",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                Some(&provider),
            )
            .await;

        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Compressed),
        );
        // Content is capped to compressed_max_tokens (5 chars with FixedTc(1)) after LLM call.
        assert!(
            tc.count_tokens(&messages[1].content) <= 5,
            "content must be capped to compressed_max_tokens after LLM summary"
        );
        // deferred_summary stores the full LLM output for future reuse.
        assert_eq!(
            messages[1].metadata.deferred_summary,
            Some("summary text".to_string()),
        );
    }

    // 16. LLM path skipped when provider is None → truncation used instead.
    #[tokio::test]
    async fn compress_llm_skipped_when_provider_none() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            enabled: true,
            full_threshold: 2.0,
            compressed_threshold: 0.0,
            compressed_max_tokens: 5,
            compress_provider: Some(ProviderName::new("mock")),
            ..make_cfg()
        };
        let tc = FixedTc(1);
        let content = "a".repeat(50);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, &content),
        ];

        scorer
            .score_and_apply(
                &mut messages,
                "some query text here",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;

        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Compressed),
        );
        // Truncation path: deferred_summary must NOT be set.
        assert!(
            messages[1].metadata.deferred_summary.is_none(),
            "deferred_summary must not be populated via truncation path"
        );
        // Content should be truncated to <= 5 chars.
        assert!(
            messages[1].content.len() <= 5,
            "content must be truncated, got len={}",
            messages[1].content.len()
        );
    }

    // 17. cosine_similarity unit tests.
    #[test]
    fn cosine_similarity_identical() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_zero_vector() {
        let a = vec![0.0f32, 0.0, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_empty() {
        assert!(cosine_similarity(&[], &[]).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_dimension_mismatch() {
        let a = vec![1.0f32, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    // 18. semantic_scoring_higher_for_similar_messages.
    //     Mock provider returns deterministic embeddings:
    //       "cat"  → [1.0, 0.0, 0.0]  (similar to query)
    //       "feline" → [0.9, 0.1, 0.0]  (similar to query)
    //       "stock" → [0.0, 0.0, 1.0]  (orthogonal to query)
    //       query "cat mat" → [1.0, 0.0, 0.0]
    #[tokio::test]
    async fn semantic_scoring_higher_for_similar_messages() {
        use zeph_llm::LlmError;
        use zeph_llm::provider::ChatStream;

        #[derive(Debug)]
        struct EmbedMockProvider;

        impl zeph_llm::provider::LlmProvider for EmbedMockProvider {
            async fn chat(&self, _: &[Message]) -> Result<String, LlmError> {
                Err(LlmError::Unavailable)
            }
            async fn chat_stream(&self, _: &[Message]) -> Result<ChatStream, LlmError> {
                Err(LlmError::Unavailable)
            }
            fn supports_streaming(&self) -> bool {
                false
            }
            async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
                let v = if text.contains("cat")
                    || text.contains("mat")
                    || text.contains("feline")
                    || text.contains("rug")
                {
                    if text.contains("feline") || text.contains("rug") {
                        vec![0.9f32, 0.1, 0.0]
                    } else {
                        vec![1.0f32, 0.0, 0.0]
                    }
                } else {
                    vec![0.0f32, 0.0, 1.0]
                };
                Ok(v)
            }
            fn supports_embeddings(&self) -> bool {
                true
            }
            fn name(&self) -> &'static str {
                "embed-mock"
            }
        }

        let provider = EmbedMockProvider;
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            enabled: true,
            semantic_scoring_provider: Some(ProviderName::new("embed-mock")),
            // Only semantic + temporal active; force Full for all so we can inspect scores
            // by checking which messages survive with Full vs not.
            // Use extreme thresholds so all messages pass through as Full.
            full_threshold: 0.0,
            compressed_threshold: 0.0,
            w_semantic: 1.0,
            w_temporal: 0.0,
            w_importance: 0.0,
            w_plan: 0.0,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let cat_msg = make_msg(Role::User, "The cat is on the mat");
        let feline_msg = make_msg(Role::User, "A feline rests on the rug");
        let stock_msg = make_msg(Role::User, "Stock prices fell today");
        let mut messages = vec![
            make_msg(Role::System, "system"),
            cat_msg,
            feline_msg,
            stock_msg,
        ];

        scorer
            .score_and_apply(
                &mut messages,
                "cat mat",
                &[],
                &cfg,
                &tc,
                0,
                false,
                Some(&provider),
                None,
            )
            .await;

        // All should be scored (Full threshold = 0.0 means everything is Full).
        // Embeddings must have been populated.
        assert!(
            messages[1].metadata.embedding.is_some(),
            "cat message must have embedding"
        );
        assert!(
            messages[2].metadata.embedding.is_some(),
            "feline message must have embedding"
        );
        assert!(
            messages[3].metadata.embedding.is_some(),
            "stock message must have embedding"
        );

        // Verify cosine directly: cat/feline should be > stock vs query [1,0,0].
        let query_emb = [1.0f32, 0.0, 0.0];
        let cat_emb = messages[1].metadata.embedding.as_ref().unwrap();
        let feline_emb = messages[2].metadata.embedding.as_ref().unwrap();
        let stock_emb = messages[3].metadata.embedding.as_ref().unwrap();
        assert!(
            cosine_similarity(cat_emb, &query_emb) > cosine_similarity(stock_emb, &query_emb),
            "cat message must be more similar to query than stock message"
        );
        assert!(
            cosine_similarity(feline_emb, &query_emb) > cosine_similarity(stock_emb, &query_emb),
            "feline message must be more similar to query than stock message"
        );
    }

    // 19. semantic_scoring_falls_back_to_keyword_when_provider_none.
    #[tokio::test]
    async fn semantic_scoring_falls_back_to_keyword_when_provider_none() {
        let scorer = FidelityScorer;
        let cfg = FidelityConfig {
            enabled: true,
            semantic_scoring_provider: None,
            ..make_cfg()
        };
        let tc = FixedTc(4);
        let mut messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "cat mat keyword test"),
            make_msg(Role::User, "something unrelated here"),
        ];

        scorer
            .score_and_apply(
                &mut messages,
                "query text here long",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;

        // No embeddings should be computed since provider is None.
        for msg in &messages {
            assert!(
                msg.metadata.embedding.is_none(),
                "no embedding must be computed when provider is None"
            );
        }
        // Scoring must still work (keyword path).
        for msg in &messages[1..] {
            assert!(
                msg.metadata.fidelity_tag.is_some(),
                "all non-system messages must be scored via keyword path"
            );
        }
    }

    // w_plan path: messages whose content overlaps planned tool keywords score higher.
    #[tokio::test]
    async fn w_plan_produces_nonzero_score_for_matching_message() {
        use zeph_common::PlannedToolHint;

        let scorer = FidelityScorer;
        // Only w_plan is active: all other weights zero so plan signal is isolated.
        // full_threshold = 0.5 so that a non-zero plan score reaches Full.
        let cfg = FidelityConfig {
            w_semantic: 0.0,
            w_temporal: 0.0,
            w_importance: 0.0,
            w_plan: 1.0,
            full_threshold: 0.5,
            compressed_threshold: 0.1,
            min_query_length: 100, // disable semantic signal
            ..make_cfg()
        };
        let tc = FixedTc(4);

        // Hint: tool_name="shell", keywords contain "cargo" and "build".
        let hint = PlannedToolHint::new("shell", vec!["cargo".to_string(), "build".to_string()], 1);

        // matching_msg contains words from the hint keywords.
        // non_matching_msg has no overlap with hint keywords.
        let mut messages = vec![
            make_msg(Role::System, "system prompt"),
            make_msg(Role::User, "run cargo build to compile"),
            make_msg(Role::User, "what is the weather today"),
        ];

        scorer
            .score_and_apply(
                &mut messages,
                "q", // short query to keep semantic inactive
                &[hint],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;

        // The matching message should be scored Full (plan overlap is high).
        assert_eq!(
            messages[1].metadata.fidelity_tag,
            Some(ContextFidelity::Full),
            "message matching planned tool keywords must reach Full fidelity"
        );

        // The non-matching message should not be Full (plan overlap is zero).
        assert_ne!(
            messages[2].metadata.fidelity_tag,
            Some(ContextFidelity::Full),
            "message with no keyword overlap must not reach Full fidelity via w_plan"
        );
    }

    // ── truncate_to_byte_limit ────────────────────────────────────────────────

    #[test]
    fn truncate_to_byte_limit_no_op_when_short() {
        assert_eq!(truncate_to_byte_limit("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_byte_limit_exact_limit_no_op() {
        assert_eq!(truncate_to_byte_limit("hello", 5), "hello");
    }

    #[test]
    fn truncate_to_byte_limit_over_limit() {
        let s = truncate_to_byte_limit("abcdefgh", 5);
        assert_eq!(s.len(), 5);
        assert_eq!(s, "abcde");
    }

    #[test]
    fn truncate_to_byte_limit_multibyte_boundary() {
        // "日本語" = 3 chars, each 3 bytes (9 bytes total). max_bytes=6 → "日本" (6 bytes).
        let s = truncate_to_byte_limit("日本語", 6);
        assert!(s.is_char_boundary(s.len()));
        assert_eq!(s, "日本");
    }

    // ── apply_input_cap ───────────────────────────────────────────────────────

    #[test]
    fn apply_input_cap_no_op_below_limit() {
        let mut s = "hello".to_string();
        apply_input_cap(&mut s, 10); // 10 * 4 = 40 bytes, well above 5
        assert_eq!(s, "hello");
    }

    #[test]
    fn apply_input_cap_truncates_over_limit() {
        // max_tokens=1 → max_bytes=4
        let mut s = "abcdefgh".to_string();
        apply_input_cap(&mut s, 1);
        assert_eq!(s, "abcd");
    }

    #[test]
    fn apply_input_cap_multibyte() {
        // "日本語" = 9 bytes. max_tokens=1 → max_bytes=4. floor_char_boundary(4) on "日本語"
        // lands at 3 (end of "日"). Result is "日".
        let mut s = "日本語".to_string();
        apply_input_cap(&mut s, 1);
        assert!(s.is_char_boundary(s.len()));
        assert_eq!(s, "日");
    }

    // ── embed_prepass ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn embed_prepass_returns_embeddings_for_non_exempt() {
        let messages = vec![
            make_msg(Role::System, "system prompt"), // exempt (idx=0, system)
            make_msg(Role::User, "user message"),
            make_msg(Role::Assistant, "assistant reply"),
        ];
        let cfg = FidelityConfig::default();
        let embed = |_text: &str| -> EmbedFuture { Box::pin(async { Ok(vec![1.0f32, 2.0, 3.0]) }) };
        let result = embed_prepass(&messages, &embed, &cfg, 0).await;
        // Index 0 (system) is exempt, indices 1 and 2 get embeddings.
        assert!(!result.contains_key(&0));
        assert_eq!(result[&1], vec![1.0, 2.0, 3.0]);
        assert_eq!(result[&2], vec![1.0, 2.0, 3.0]);
    }

    #[tokio::test]
    async fn embed_prepass_skips_empty_content() {
        let messages = vec![make_msg(Role::System, "system"), make_msg(Role::User, "")];
        let cfg = FidelityConfig::default();
        let embed = |_text: &str| -> EmbedFuture { Box::pin(async { Ok(vec![1.0f32]) }) };
        let result = embed_prepass(&messages, &embed, &cfg, 0).await;
        assert!(!result.contains_key(&1), "empty content must be skipped");
    }

    #[tokio::test]
    async fn embed_prepass_skips_inserted_memory() {
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "injected memory"),
            make_msg(Role::User, "real user message"),
        ];
        let cfg = FidelityConfig::default();
        let embed = |_text: &str| -> EmbedFuture { Box::pin(async { Ok(vec![1.0f32]) }) };
        // inserted_count=1 → idx 1 is exempt
        let result = embed_prepass(&messages, &embed, &cfg, 1).await;
        assert!(!result.contains_key(&0), "system is exempt");
        assert!(!result.contains_key(&1), "inserted memory is exempt");
        assert!(
            result.contains_key(&2),
            "real user message must be embedded"
        );
    }

    #[tokio::test]
    async fn embed_prepass_silently_skips_errors() {
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "user"),
        ];
        let cfg = FidelityConfig::default();
        let embed = |_text: &str| -> EmbedFuture {
            Box::pin(async {
                Err(zeph_llm::LlmError::EmbedUnsupported {
                    provider: "mock".to_string(),
                })
            })
        };
        let result = embed_prepass(&messages, &embed, &cfg, 0).await;
        assert!(result.is_empty(), "errors must be silently skipped");
    }

    #[tokio::test]
    async fn embed_prepass_truncates_content_when_cap_set() {
        // With max_embed_input_tokens=1 (max_bytes=4), content longer than 4 bytes is truncated.
        let long_content = "a".repeat(100);
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, &long_content),
        ];
        let cfg = FidelityConfig {
            max_embed_input_tokens: Some(1), // 1 * 4 = 4 bytes max
            ..FidelityConfig::default()
        };
        let seen_len = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let seen_len_clone = seen_len.clone();
        let embed = move |text: &str| -> EmbedFuture {
            let len = text.len();
            let seen = seen_len_clone.clone();
            Box::pin(async move {
                *seen.lock().unwrap() = len;
                Ok(vec![1.0f32])
            })
        };
        embed_prepass(&messages, &embed, &cfg, 0).await;
        assert_eq!(
            *seen_len.lock().unwrap(),
            4,
            "content must be truncated to max_embed_input_tokens * 4 bytes"
        );
    }

    #[tokio::test]
    async fn embed_prepass_concurrency_zero_clamped_to_one() {
        // embed_concurrency=0 must be clamped to 1 (with a warning) and still produce results.
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "user message"),
        ];
        let cfg = FidelityConfig {
            embed_concurrency: 0,
            ..FidelityConfig::default()
        };
        let embed = |_text: &str| -> EmbedFuture { Box::pin(async { Ok(vec![1.0f32]) }) };
        let result = embed_prepass(&messages, &embed, &cfg, 0).await;
        assert!(
            result.contains_key(&1),
            "result must be produced even with concurrency=0"
        );
    }

    #[tokio::test]
    async fn embed_prepass_skips_cached_embeddings() {
        let mut msg_with_cache = make_msg(Role::User, "already embedded");
        msg_with_cache.metadata.embedding = Some(vec![9.0f32]);
        let messages = vec![
            make_msg(Role::System, "system"),
            msg_with_cache,
            make_msg(Role::User, "needs embedding"),
        ];
        let cfg = FidelityConfig::default();
        let embed = |_text: &str| -> EmbedFuture { Box::pin(async { Ok(vec![1.0f32]) }) };
        let result = embed_prepass(&messages, &embed, &cfg, 0).await;
        assert!(
            !result.contains_key(&1),
            "message with cached embedding must be skipped"
        );
        assert!(
            result.contains_key(&2),
            "message without embedding must be processed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn embed_prepass_timeout_skips_message() {
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "user"),
        ];
        let cfg = FidelityConfig::default();
        let embed = |_text: &str| -> EmbedFuture {
            Box::pin(async {
                // Simulate a stalled embed provider.
                tokio::time::sleep(Duration::from_secs(45)).await;
                Ok(vec![1.0f32])
            })
        };
        // Run the pre-pass; the 30-second timeout fires before the 45-second sleep.
        let result = embed_prepass(&messages, &embed, &cfg, 0).await;
        assert!(result.is_empty(), "timed-out embed must be skipped");
    }

    #[tokio::test(start_paused = true)]
    async fn embed_prepass_custom_timeout_respected() {
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "user"),
        ];
        // embed_timeout_secs=5: provider takes 10 s → must be skipped.
        let cfg = FidelityConfig {
            embed_timeout_secs: 5,
            ..FidelityConfig::default()
        };
        let embed = |_text: &str| -> EmbedFuture {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(vec![1.0f32])
            })
        };
        let result = embed_prepass(&messages, &embed, &cfg, 0).await;
        assert!(
            result.is_empty(),
            "custom embed_timeout_secs must be honored"
        );
    }

    // ── FidelityConfig new fields ─────────────────────────────────────────────

    #[test]
    fn fidelity_config_new_fields_defaults() {
        let cfg = FidelityConfig::default();
        assert_eq!(cfg.embed_concurrency, 32);
        assert!(cfg.max_embed_input_tokens.is_none());
        assert!(cfg.max_compress_input_tokens.is_none());
    }

    #[test]
    fn fidelity_config_timeout_defaults() {
        let cfg = FidelityConfig::default();
        assert_eq!(cfg.embed_timeout_secs, 30);
        assert_eq!(cfg.compress_timeout_secs, 30);
    }

    #[test]
    fn fidelity_config_new_fields_custom() {
        let cfg = FidelityConfig {
            embed_concurrency: 8,
            max_embed_input_tokens: Some(512),
            max_compress_input_tokens: Some(1024),
            ..FidelityConfig::default()
        };
        assert_eq!(cfg.embed_concurrency, 8);
        assert_eq!(cfg.max_embed_input_tokens, Some(512));
        assert_eq!(cfg.max_compress_input_tokens, Some(1024));
    }

    // Post-compress truncation: deferred_summary exceeding compressed_max_tokens is trimmed.
    #[tokio::test]
    async fn render_compressed_truncates_oversized_deferred_summary() {
        let scorer = FidelityScorer;
        // full_threshold=2.0 unreachable, compressed_threshold=0.0 → everything Compressed.
        // compressed_max_tokens=3 (with FixedTc(1): 1 char = 1 token, so 3 tokens = 3 chars).
        let cfg = FidelityConfig {
            full_threshold: 2.0,
            compressed_threshold: 0.0,
            compressed_max_tokens: 3,
            ..make_cfg()
        };
        let tc = FixedTc(1);
        let mut msg = make_msg(Role::User, "original long content");
        // Simulate LLM returning a summary that is 10 chars (> 3 tokens).
        msg.metadata.deferred_summary = Some("ten chars!".to_string());
        let mut messages = vec![make_msg(Role::System, "sys"), msg];
        scorer
            .score_and_apply(
                &mut messages,
                "query text here long",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        let compressed = &messages[1];
        assert_eq!(
            compressed.metadata.fidelity_tag,
            Some(ContextFidelity::Compressed)
        );
        assert!(
            tc.count_tokens(&compressed.content) <= 3,
            "deferred_summary result must be truncated to compressed_max_tokens"
        );
    }

    // max_compress_input_tokens: content is capped before truncation when no deferred_summary.
    #[tokio::test]
    async fn render_compressed_applies_max_compress_input_tokens() {
        let scorer = FidelityScorer;
        // Force everything to Compressed. max_compress_input_tokens=2 → max_bytes=8.
        // FixedTc(1): 1 byte = 1 token. compressed_max_tokens=100 (no output cap needed here).
        let cfg = FidelityConfig {
            full_threshold: 2.0,
            compressed_threshold: 0.0,
            compressed_max_tokens: 100,
            max_compress_input_tokens: Some(2), // 2 * 4 = 8 bytes
            ..make_cfg()
        };
        let tc = FixedTc(1);
        let content_20 = "a".repeat(20); // 20 bytes, must be capped to 8
        let mut msg = make_msg(Role::User, &content_20);
        // No deferred_summary → input cap path applies.
        let mut messages = vec![make_msg(Role::System, "sys"), msg.clone()];
        scorer
            .score_and_apply(
                &mut messages,
                "query text here long",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        let compressed = &messages[1];
        assert_eq!(
            compressed.metadata.fidelity_tag,
            Some(ContextFidelity::Compressed)
        );
        // Input was capped to 8 bytes, then output cap of 100 is no-op, so content is 8 bytes.
        assert_eq!(
            compressed.content.len(),
            8,
            "content must be capped to max_compress_input_tokens * 4 bytes"
        );
        // Verify deferred_summary path is unaffected by input cap.
        msg.metadata.deferred_summary = Some("short".to_string());
        let mut messages2 = vec![make_msg(Role::System, "sys"), msg];
        scorer
            .score_and_apply(
                &mut messages2,
                "query text here long",
                &[],
                &cfg,
                &tc,
                0,
                false,
                None,
                None,
            )
            .await;
        assert_eq!(
            messages2[1].content, "short",
            "deferred_summary must bypass input cap"
        );
    }
}
