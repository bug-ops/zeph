// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `MemFlow` tiered intent-driven retrieval pipeline (issue #3712).
//!
//! Classifies each recall query into one of three intent tiers and dispatches to the
//! cheapest sufficient backend, assembling evidence within a configurable token budget.
//!
//! # Tiers
//!
//! | Tier | Backend | Top-k | Graph hops |
//! |------|---------|-------|-----------|
//! | `ProfileLookup` | Keyword / persona | 3 | 0 |
//! | `TargetedRetrieval` | Hybrid | 10 | 1 |
//! | `DeepReasoning` | Hybrid + graph | 20 | 2 |
//!
//! The classifier maps the existing [`MemoryRoute`] to an [`IntentClass`]:
//! - `Keyword | Episodic` → `ProfileLookup`
//! - `Semantic | Hybrid` → `TargetedRetrieval`
//! - `Graph` → `DeepReasoning`
//!
//! When `classifier_provider` is set and the LLM call fails, the pipeline falls back to
//! [`HeuristicRouter`] (fail-open, logged at `warn`).
//!
//! # Token-budget assembly
//!
//! Recall results are truncated to fit within `token_budget`. An optional validation step
//! asks a lightweight LLM whether the gathered evidence is sufficient; on low confidence,
//! the pipeline escalates to the next heavier tier (up to `max_escalations`).

use std::collections::HashMap;
use std::sync::Arc;

use tracing::Instrument as _;
pub use zeph_config::memory::TieredRetrievalConfig;
use zeph_llm::any::AnyProvider;

use crate::embedding_store::SearchFilter;
use crate::error::MemoryError;
use crate::router::{HeuristicRouter, HybridRouter, MemoryRoute, MemoryRouter};
use crate::semantic::RecalledMessage;
use crate::semantic::SemanticMemory;
use crate::types::{ConversationId, MessageId};

// ── Intent classification ─────────────────────────────────────────────────────

/// Query intent tier for `MemFlow` tiered retrieval.
///
/// Maps to increasing levels of retrieval cost and depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IntentClass {
    /// Fast profile/attribute lookup — keyword search, top-k = 3.
    ProfileLookup,
    /// Standard semantic retrieval — hybrid search with MMR, top-k = 10.
    TargetedRetrieval,
    /// Multi-hop reasoning — hybrid + graph traversal, top-k = 20.
    DeepReasoning,
}

impl IntentClass {
    fn from_route(route: MemoryRoute) -> Self {
        match route {
            MemoryRoute::Keyword | MemoryRoute::Episodic => Self::ProfileLookup,
            MemoryRoute::Graph => Self::DeepReasoning,
            _ => Self::TargetedRetrieval,
        }
    }

    fn top_k(self) -> usize {
        match self {
            Self::ProfileLookup => 3,
            Self::TargetedRetrieval => 10,
            Self::DeepReasoning => 20,
        }
    }

    /// Returns the next heavier tier for escalation, or `None` if already at maximum.
    fn escalate(self) -> Option<Self> {
        match self {
            Self::ProfileLookup => Some(Self::TargetedRetrieval),
            Self::TargetedRetrieval => Some(Self::DeepReasoning),
            Self::DeepReasoning => None,
        }
    }
}

impl std::fmt::Display for IntentClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProfileLookup => f.write_str("ProfileLookup"),
            Self::TargetedRetrieval => f.write_str("TargetedRetrieval"),
            Self::DeepReasoning => f.write_str("DeepReasoning"),
        }
    }
}

// ── Result ────────────────────────────────────────────────────────────────────

/// Result of tiered retrieval, including evidence and tier metadata.
#[derive(Debug)]
pub struct TieredRetrievalResult {
    /// Retrieved memory entries ordered by relevance score.
    pub messages: Vec<RecalledMessage>,
    /// The intent class that produced this result.
    pub intent: IntentClass,
    /// Approximate token count of all message content.
    pub tokens_used: usize,
    /// Whether the pipeline escalated to a heavier tier due to validation.
    pub tier_escalated: bool,
}

// ── Tiered retrieval logic ─────────────────────────────────────────────────────

/// Execute `MemFlow` tiered retrieval for a single query.
///
/// Classifies intent, retrieves tier candidates, assembles evidence within budget, and
/// optionally validates + escalates if evidence is insufficient.
///
/// `classifier` should be the provider resolved from
/// [`TieredRetrievalConfig::classifier_provider`]. When `Some`, a [`HybridRouter`] is
/// used for LLM-backed intent classification (with [`HeuristicRouter`] as fallback on
/// LLM failure). When `None`, only the heuristic router is used.
///
/// `validator` should be the provider resolved from
/// [`TieredRetrievalConfig::validator_provider`]. When `Some` and
/// `config.validation_enabled` is `true`, the validator LLM judges evidence quality and
/// triggers tier escalation when confidence is low.
///
/// `conversation_id` scopes the search to a single conversation. Pass `None` to search globally.
///
/// # Errors
///
/// Returns an error if any underlying search or database operation fails.
#[tracing::instrument(name = "memory.tiered.retrieve", skip_all, fields(intent = tracing::field::Empty))]
pub async fn recall_tiered(
    memory: &SemanticMemory,
    query: &str,
    conversation_id: Option<ConversationId>,
    classifier: Option<&Arc<AnyProvider>>,
    validator: Option<&Arc<AnyProvider>>,
    config: &TieredRetrievalConfig,
    remaining_budget: Option<usize>,
) -> Result<TieredRetrievalResult, MemoryError> {
    let effective_budget =
        remaining_budget.map_or(config.token_budget, |rb| rb.min(config.token_budget));

    let initial_intent = if let Some(classifier_provider) = classifier {
        let hybrid = HybridRouter::new(
            Arc::clone(classifier_provider),
            MemoryRoute::Hybrid,
            // 0.7 is the codebase-wide default for HybridRouter confidence threshold
            0.7,
        );
        let decision = if let Ok(d) = tokio::time::timeout(
            std::time::Duration::from_secs(config.classifier_timeout_secs),
            hybrid.classify_async(query),
        )
        .await
        {
            d
        } else {
            tracing::warn!("tiered: classifier LLM timed out, falling back to heuristic");
            HeuristicRouter.route_with_confidence(query)
        };
        IntentClass::from_route(decision.route)
    } else {
        let decision = HeuristicRouter.route_with_confidence(query);
        IntentClass::from_route(decision.route)
    };

    tracing::debug!(intent = %initial_intent, query_len = query.len(), "tiered: classified intent");

    escalation_loop(
        memory,
        query,
        conversation_id,
        initial_intent,
        validator,
        config,
        effective_budget,
    )
    .await
}

/// Inner escalation loop shared across retrieval entry points.
///
/// Iterates through tiers starting at `initial_intent`, retrieving candidates and
/// validating evidence quality. Escalates to heavier tiers when validation indicates
/// insufficient evidence.
async fn escalation_loop(
    memory: &SemanticMemory,
    query: &str,
    conversation_id: Option<ConversationId>,
    initial_intent: IntentClass,
    validator: Option<&Arc<AnyProvider>>,
    config: &TieredRetrievalConfig,
    effective_budget: usize,
) -> Result<TieredRetrievalResult, MemoryError> {
    let mut intent = initial_intent;
    let mut escalations: u8 = 0;
    let mut tier_escalated = false;

    loop {
        let raw_candidates = retrieve_tier(memory, query, conversation_id, intent, config)
            .instrument(tracing::debug_span!("memory.tiered.retrieve_tier", tier = %intent))
            .await?;

        let candidates = score_candidates(memory, query, raw_candidates, config)
            .instrument(tracing::debug_span!("memory.tiered.score_candidates", tier = %intent))
            .await?;

        let (messages, tokens_used) = {
            let _span = tracing::debug_span!("memory.tiered.assemble").entered();
            assemble_within_budget(candidates, effective_budget)
        };

        // Validate evidence quality if enabled and a validator is available.
        if config.validation_enabled
            && escalations < config.max_escalations
            && let Some(validator_provider) = validator
            && let Some(next_tier) = intent.escalate()
        {
            let sufficient = validate_evidence(
                validator_provider,
                query,
                &messages,
                config.validation_threshold,
                config.validator_timeout_secs,
            )
            .instrument(tracing::debug_span!("memory.tiered.validate"))
            .await;
            if !sufficient {
                tracing::debug!(
                    current_tier = %intent,
                    next_tier = %next_tier,
                    escalations,
                    "tiered: evidence insufficient, escalating tier"
                );
                intent = next_tier;
                escalations += 1;
                tier_escalated = true;
                continue;
            }
        }

        return Ok(TieredRetrievalResult {
            messages,
            intent,
            tokens_used,
            tier_escalated,
        });
    }
}

/// Retrieve candidates for the given intent tier from `SemanticMemory`.
///
/// For `DeepReasoning` when `config.deep_reasoning_query_conditioned = true`, routes through
/// query-conditioned graph recall (HELA spreading activation) instead of static-weight BFS (#3994).
async fn retrieve_tier(
    memory: &SemanticMemory,
    query: &str,
    conversation_id: Option<ConversationId>,
    intent: IntentClass,
    config: &TieredRetrievalConfig,
) -> Result<Vec<RecalledMessage>, MemoryError> {
    let top_k = intent.top_k();
    let heuristic = HeuristicRouter;

    let filter = conversation_id.map(|cid| SearchFilter {
        conversation_id: Some(cid),
        role: None,
        category: None,
    });

    // DeepReasoning tier: optionally route through query-conditioned HELA recall (#3994).
    if intent == IntentClass::DeepReasoning && config.deep_reasoning_query_conditioned {
        use crate::graph::HelaSpreadParams;
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        let params = HelaSpreadParams::default();
        match memory.recall_graph_hela(query, top_k, params).await {
            Ok(hela_facts) if !hela_facts.is_empty() => {
                let messages: Vec<RecalledMessage> = hela_facts
                    .into_iter()
                    .map(|f| {
                        let content = format!(
                            "{} — {} — {}",
                            f.edge.relation, f.edge.fact, f.edge.canonical_relation
                        );
                        RecalledMessage {
                            message: Message {
                                role: Role::Assistant,
                                content,
                                parts: vec![],
                                metadata: MessageMetadata::default(),
                            },
                            score: f.score,
                        }
                    })
                    .collect();
                tracing::debug!(
                    count = messages.len(),
                    "tiered: DeepReasoning via query-conditioned HELA recall"
                );
                return Ok(messages);
            }
            Ok(_) => {
                tracing::debug!("tiered: HELA returned no results, falling back to recall_routed");
            }
            Err(e) => {
                tracing::warn!("tiered: HELA recall failed ({e:#}), falling back to recall_routed");
            }
        }
    }

    // All other tiers (and DeepReasoning fallback) route through recall_routed.
    memory
        .recall_routed(query, top_k, filter, &heuristic, None)
        .await
}

// ── Five-signal retrieval scoring ─────────────────────────────────────────────

/// Intermediate struct for multi-signal scoring of a single candidate.
struct ScoredCandidate {
    recalled: RecalledMessage,
}

/// Re-score `candidates` using up to five signals and return them sorted by final score.
///
/// Overwrites `RecalledMessage::score` with the combined weighted score.
/// When all signal weights are zero (mis-configuration), logs a debug warning and
/// returns candidates in original order with scores unchanged.
///
/// # Errors
///
/// Propagates store errors from timestamp or tier lookups.
#[allow(clippy::too_many_lines)]
#[tracing::instrument(name = "memory.tiered.score_candidates", skip_all)]
async fn score_candidates(
    memory: &SemanticMemory,
    query: &str,
    candidates: Vec<RecalledMessage>,
    config: &TieredRetrievalConfig,
) -> Result<Vec<RecalledMessage>, MemoryError> {
    if candidates.is_empty() {
        return Ok(candidates);
    }

    let total_weight = config.similarity_weight
        + config.recency_weight
        + config.tfidf_weight
        + config.cognitive_signal_weight
        + config.tier_boost_weight;

    if total_weight < f64::EPSILON {
        tracing::debug!("score_candidates: all signal weights are zero, returning original order");
        return Ok(candidates);
    }

    let ids: Vec<MessageId> = candidates
        .iter()
        .map(|c| MessageId(c.message.metadata.db_id.unwrap_or(0)))
        .collect();

    // Fetch timestamps and tiers only when their respective signals are active.
    let (timestamps_res, tiers_res) = tokio::join!(
        async {
            if config.recency_weight > 0.0 {
                memory.sqlite().message_timestamps(&ids).await
            } else {
                Ok(HashMap::new())
            }
        },
        async {
            if config.tier_boost_weight > 0.0 {
                memory.sqlite().fetch_tiers(&ids).await
            } else {
                Ok(HashMap::new())
            }
        },
    );
    let timestamps: HashMap<MessageId, i64> = timestamps_res.unwrap_or_else(|e| {
        tracing::warn!("score_candidates: failed to fetch timestamps: {e:#}");
        HashMap::new()
    });
    let tiers: HashMap<MessageId, String> = tiers_res.unwrap_or_else(|e| {
        tracing::warn!("score_candidates: failed to fetch tiers: {e:#}");
        HashMap::new()
    });

    // Fetch access counts for cognitive signal.
    let access_counts: HashMap<MessageId, i64> = if config.cognitive_signal_weight > 0.0 {
        memory
            .sqlite()
            .message_access_counts(&ids)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("score_candidates: failed to fetch access counts: {e:#}");
                HashMap::new()
            })
    } else {
        HashMap::new()
    };

    let tfidf_scores = if config.tfidf_weight > 0.0 {
        compute_tfidf_scores(query, &candidates)
    } else {
        vec![0.0_f64; candidates.len()]
    };

    let max_access: i64 = access_counts.values().copied().max().unwrap_or(0);

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));

    let mut scored: Vec<ScoredCandidate> = candidates
        .into_iter()
        .zip(tfidf_scores)
        .map(|(recalled, tfidf)| {
            let msg_id = MessageId(recalled.message.metadata.db_id.unwrap_or(0));

            let similarity = f64::from(recalled.score);
            let recency = if config.recency_weight > 0.0 && config.recency_half_life_days > 0 {
                let ts = timestamps.get(&msg_id).copied().unwrap_or(now_secs);
                compute_recency(ts, now_secs, config.recency_half_life_days)
            } else {
                0.0
            };

            let cognitive = if config.cognitive_signal_weight > 0.0 && max_access > 0 {
                let count = access_counts.get(&msg_id).copied().unwrap_or(0);
                // Both are i64; precision loss is acceptable for a normalized ratio.
                #[allow(clippy::cast_precision_loss)]
                let ratio = count as f64 / max_access as f64;
                ratio
            } else {
                0.0
            };

            let tier_signal = if config.tier_boost_weight > 0.0 {
                let tier = tiers.get(&msg_id).map_or("episodic", String::as_str);
                if tier == "semantic" {
                    config.semantic_tier_boost
                } else {
                    0.0
                }
            } else {
                0.0
            };

            let final_score = config.similarity_weight * similarity
                + config.recency_weight * recency
                + config.tfidf_weight * tfidf
                + config.cognitive_signal_weight * cognitive
                + config.tier_boost_weight * tier_signal;

            ScoredCandidate {
                recalled: RecalledMessage {
                    // f64 → f32: deliberate truncation, score precision is adequate.
                    #[allow(clippy::cast_possible_truncation)]
                    score: final_score as f32,
                    ..recalled
                },
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        b.recalled
            .score
            .partial_cmp(&a.recalled.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(scored.into_iter().map(|s| s.recalled).collect())
}

/// Compute recency score in `[0.0, 1.0]` using exponential half-life decay.
///
/// Returns `1.0` for a message created right now and approaches `0.0` for very old messages.
/// A message that is exactly `half_life_days` old receives a score of `0.5`.
///
/// # Precondition
///
/// `half_life_days` must be greater than zero. Passing `0` is a programming error and will
/// panic in debug builds.
fn compute_recency(created_at_secs: i64, now_secs: i64, half_life_days: u32) -> f64 {
    debug_assert!(half_life_days > 0, "half_life_days must be > 0");
    // Precision loss is acceptable: age is a time delta in days, not a financial value.
    #[allow(clippy::cast_precision_loss)]
    let age_days = (now_secs - created_at_secs).max(0) as f64 / 86_400.0;
    let lambda = std::f64::consts::LN_2 / f64::from(half_life_days);
    (-lambda * age_days).exp()
}

/// Compute per-candidate TF-IDF scores against `query`, normalised to `[0.0, 1.0]`.
///
/// Uses a simplified TF-IDF with BM25-style parameters (k1 = 1.2, b = 0.75).
/// Scores are normalised by dividing by the maximum score in the batch.
fn compute_tfidf_scores(query: &str, candidates: &[RecalledMessage]) -> Vec<f64> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;

    let query_terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();

    if query_terms.is_empty() || candidates.is_empty() {
        return vec![0.0; candidates.len()];
    }

    // Tokenise each candidate document.
    let docs: Vec<Vec<String>> = candidates
        .iter()
        .map(|c| {
            c.message
                .content
                .split_whitespace()
                .map(str::to_lowercase)
                .collect()
        })
        .collect();

    // Precision loss is acceptable for term-frequency ratios over small candidate sets.
    #[allow(clippy::cast_precision_loss)]
    let n = docs.len() as f64;
    #[allow(clippy::cast_precision_loss)]
    let avg_dl = docs.iter().map(|d| d.len() as f64).sum::<f64>().max(1.0) / n;

    let mut scores = vec![0.0_f64; docs.len()];

    for term in &query_terms {
        // Document frequency across the candidate set.
        #[allow(clippy::cast_precision_loss)]
        let df = docs.iter().filter(|d| d.contains(term)).count() as f64;
        if df == 0.0 {
            continue;
        }
        // IDF with smoothing.
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

        for (i, doc) in docs.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let dl = doc.len() as f64;
            #[allow(clippy::cast_precision_loss)]
            let tf = doc.iter().filter(|t| *t == term).count() as f64;
            let bm25_tf = (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * dl / avg_dl));
            scores[i] += idf * bm25_tf;
        }
    }

    // Normalise to [0.0, 1.0].
    let max_score = scores.iter().copied().fold(0.0_f64, f64::max);
    if max_score > 0.0 {
        for s in &mut scores {
            *s /= max_score;
        }
    }

    scores
}

/// Truncate `candidates` to fit within `budget` tokens.
///
/// Uses the same 4 chars-per-token approximation as the rest of the codebase.
/// Returns the retained messages and the total token count consumed.
fn assemble_within_budget(
    candidates: Vec<RecalledMessage>,
    budget: usize,
) -> (Vec<RecalledMessage>, usize) {
    let mut retained = Vec::with_capacity(candidates.len());
    let mut total_tokens: usize = 0;

    for msg in candidates {
        let msg_tokens = zeph_common::text::estimate_tokens(&msg.message.content);
        if total_tokens.saturating_add(msg_tokens) > budget {
            break;
        }
        total_tokens += msg_tokens;
        retained.push(msg);
    }

    (retained, total_tokens)
}

/// Ask the validator LLM whether the gathered evidence is sufficient for the query.
///
/// Returns `true` when the validator's confidence is >= `threshold` or when the
/// call fails (fail-open: prefer serving potentially incomplete evidence over blocking).
async fn validate_evidence(
    provider: &Arc<AnyProvider>,
    query: &str,
    messages: &[RecalledMessage],
    threshold: f32,
    timeout_secs: u64,
) -> bool {
    use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

    if messages.is_empty() {
        return false;
    }

    let evidence_snippet = messages
        .iter()
        .take(5)
        .map(|m| {
            zeph_common::sanitize::strip_control_chars_preserve_whitespace(&m.message.content)
                .chars()
                .take(200)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n---\n");

    let system = "You are an evidence quality judge. \
        Given a query and evidence snippets, decide if the evidence is sufficient to answer the query. \
        Respond ONLY with a JSON object: {\"sufficient\": true|false, \"confidence\": 0.0-1.0}";

    let sanitized_query = zeph_common::sanitize::strip_control_chars_preserve_whitespace(query);
    let user = format!(
        "<query>{}</query>\n<evidence>{}</evidence>",
        sanitized_query.chars().take(500).collect::<String>(),
        evidence_snippet
    );

    let msgs = vec![
        Message {
            role: Role::System,
            content: system.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: user,
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];

    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        provider.chat(&msgs),
    )
    .await
    {
        Ok(Ok(raw)) => parse_validation_response(&raw, threshold),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "tiered: validator LLM call failed, treating as sufficient");
            true
        }
        Err(_) => {
            tracing::warn!("tiered: validator LLM call timed out, treating as sufficient");
            true
        }
    }
}

fn parse_validation_response(raw: &str, threshold: f32) -> bool {
    let json_str = raw
        .find('{')
        .and_then(|s| raw[s..].rfind('}').map(|e| &raw[s..=s + e]))
        .unwrap_or("");

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
        let sufficient = v
            .get("sufficient")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        #[allow(clippy::cast_possible_truncation)]
        let confidence = v
            .get("confidence")
            .and_then(serde_json::Value::as_f64)
            .map_or(1.0, |c| c.clamp(0.0, 1.0) as f32);

        return sufficient && confidence >= threshold;
    }

    tracing::debug!("tiered: could not parse validator response, treating as sufficient");
    true
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::MemoryRoute;
    use crate::semantic::RecalledMessage;
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    fn make_message(content: &str) -> RecalledMessage {
        RecalledMessage {
            message: Message {
                role: Role::User,
                content: content.to_owned(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            score: 1.0,
        }
    }

    // ── Signal scoring unit tests ─────────────────────────────────────────────

    #[test]
    fn compute_recency_zero_age_returns_one() {
        let now = 1_000_000_i64;
        let score = compute_recency(now, now, 7);
        assert!((score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn compute_recency_half_life_returns_half() {
        let now = 1_000_000_i64;
        let half_life_days = 7_u32;
        let age_secs = i64::from(half_life_days) * 86_400;
        let score = compute_recency(now - age_secs, now, half_life_days);
        assert!((score - 0.5).abs() < 1e-9);
    }

    #[test]
    fn compute_recency_large_age_approaches_zero() {
        // 1000 days with 7-day half-life: score ≈ 2^(-1000/7) ≈ 1e-43
        let now = 1_000_i64 * 86_400;
        let score = compute_recency(0, now, 7);
        assert!(score < 1e-6, "score was {score}");
    }

    #[test]
    fn compute_recency_future_timestamp_clamped_to_one() {
        let now = 1_000_000_i64;
        // created_at in the future → age < 0 → clamped to 0 → score = 1.0
        let score = compute_recency(now + 86_400, now, 7);
        assert!((score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn compute_tfidf_empty_candidates_returns_empty() {
        let scores = compute_tfidf_scores("hello", &[]);
        assert!(scores.is_empty());
    }

    #[test]
    fn compute_tfidf_empty_query_returns_zeros() {
        let candidates = vec![make_message("hello world")];
        let scores = compute_tfidf_scores("", &candidates);
        assert_eq!(scores.len(), 1);
        assert!(scores[0].abs() < f64::EPSILON);
    }

    #[test]
    fn compute_tfidf_exact_match_scores_nonzero() {
        let candidates = vec![
            make_message("the quick brown fox"),
            make_message("completely unrelated content"),
        ];
        let scores = compute_tfidf_scores("fox", &candidates);
        assert_eq!(scores.len(), 2);
        // The message containing "fox" must score higher.
        assert!(scores[0] > scores[1]);
    }

    #[test]
    fn compute_tfidf_no_match_returns_zeros() {
        let candidates = vec![make_message("apple banana cherry")];
        let scores = compute_tfidf_scores("zzz xyz", &candidates);
        assert_eq!(scores.len(), 1);
        assert!(scores[0].abs() < f64::EPSILON);
    }

    #[test]
    fn compute_tfidf_max_score_normalised_to_one() {
        let candidates = vec![
            make_message("rust programming language"),
            make_message("python programming language"),
            make_message("java is a drink"),
        ];
        let scores = compute_tfidf_scores("rust programming", &candidates);
        let max = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!((max - 1.0).abs() < 1e-9, "max score must be 1.0, got {max}");
    }

    #[test]
    fn score_candidates_empty_input_returns_empty() {
        // Pure sync test via tokio runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let memory = crate::testing::mock_semantic_memory()
                .await
                .expect("mock_semantic_memory");
            let config = TieredRetrievalConfig::default();
            let result = score_candidates(&memory, "query", vec![], &config)
                .await
                .expect("score_candidates must not fail on empty input");
            assert!(result.is_empty());
        });
    }

    #[test]
    fn score_candidates_similarity_weight_reorders_by_score() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let memory = crate::testing::mock_semantic_memory()
                .await
                .expect("mock_semantic_memory");
            // similarity_weight = 1.0 activates the scoring formula; candidates provided in
            // ascending score order to verify that sort_by reorders them descending.
            let config = TieredRetrievalConfig {
                similarity_weight: 1.0,
                ..TieredRetrievalConfig::default()
            };
            let candidates = vec![
                RecalledMessage {
                    message: make_message("low score").message,
                    score: 0.1,
                },
                RecalledMessage {
                    message: make_message("high score").message,
                    score: 0.9,
                },
                RecalledMessage {
                    message: make_message("mid score").message,
                    score: 0.5,
                },
            ];
            let result = score_candidates(&memory, "query", candidates, &config)
                .await
                .expect("score_candidates must not fail");
            assert_eq!(result.len(), 3);
            // Descending order: 0.9 → 0.5 → 0.1
            assert!(
                result[0].score >= result[1].score,
                "first score {} must be >= second score {}",
                result[0].score,
                result[1].score
            );
            assert!(
                result[1].score >= result[2].score,
                "second score {} must be >= third score {}",
                result[1].score,
                result[2].score
            );
            // Highest original score should be ranked first.
            assert!(
                (result[0].score - 0.9_f32).abs() < 1e-4,
                "expected first score ~0.9, got {}",
                result[0].score
            );
        });
    }

    #[test]
    fn score_candidates_all_zero_weights_returns_original_order() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let memory = crate::testing::mock_semantic_memory()
                .await
                .expect("mock_semantic_memory");
            // All weights zero: score_candidates must return candidates unchanged.
            let config = TieredRetrievalConfig {
                similarity_weight: 0.0,
                recency_weight: 0.0,
                tfidf_weight: 0.0,
                cognitive_signal_weight: 0.0,
                tier_boost_weight: 0.0,
                ..TieredRetrievalConfig::default()
            };
            let candidates = vec![
                RecalledMessage {
                    message: make_message("first").message,
                    score: 0.9,
                },
                RecalledMessage {
                    message: make_message("second").message,
                    score: 0.1,
                },
            ];
            let result = score_candidates(&memory, "query", candidates, &config)
                .await
                .expect("score_candidates must not fail");
            // Original order preserved because all-zero weights triggers early return.
            assert!((f64::from(result[0].score) - 0.9).abs() < 1e-6);
            assert!((f64::from(result[1].score) - 0.1).abs() < 1e-6);
        });
    }

    #[test]
    fn tiered_retrieval_config_signal_weight_defaults() {
        let cfg = TieredRetrievalConfig::default();
        assert!((cfg.similarity_weight - 1.0).abs() < f64::EPSILON);
        assert!(cfg.recency_weight.abs() < f64::EPSILON);
        assert_eq!(cfg.recency_half_life_days, 7);
        assert!(cfg.tfidf_weight.abs() < f64::EPSILON);
        assert!(cfg.cognitive_signal_weight.abs() < f64::EPSILON);
        assert!(cfg.tier_boost_weight.abs() < f64::EPSILON);
        assert!((cfg.semantic_tier_boost - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn intent_class_from_route_mapping() {
        assert_eq!(
            IntentClass::from_route(MemoryRoute::Keyword),
            IntentClass::ProfileLookup
        );
        assert_eq!(
            IntentClass::from_route(MemoryRoute::Episodic),
            IntentClass::ProfileLookup
        );
        assert_eq!(
            IntentClass::from_route(MemoryRoute::Semantic),
            IntentClass::TargetedRetrieval
        );
        assert_eq!(
            IntentClass::from_route(MemoryRoute::Hybrid),
            IntentClass::TargetedRetrieval
        );
        assert_eq!(
            IntentClass::from_route(MemoryRoute::Graph),
            IntentClass::DeepReasoning
        );
    }

    #[test]
    fn intent_class_top_k() {
        assert_eq!(IntentClass::ProfileLookup.top_k(), 3);
        assert_eq!(IntentClass::TargetedRetrieval.top_k(), 10);
        assert_eq!(IntentClass::DeepReasoning.top_k(), 20);
    }

    #[test]
    fn intent_class_escalate_chain() {
        assert_eq!(
            IntentClass::ProfileLookup.escalate(),
            Some(IntentClass::TargetedRetrieval)
        );
        assert_eq!(
            IntentClass::TargetedRetrieval.escalate(),
            Some(IntentClass::DeepReasoning)
        );
        assert_eq!(IntentClass::DeepReasoning.escalate(), None);
    }

    #[test]
    fn assemble_within_budget_empty_input() {
        let (retained, tokens) = assemble_within_budget(vec![], 4096);
        assert!(retained.is_empty());
        assert_eq!(tokens, 0);
    }

    #[test]
    fn assemble_within_budget_zero_budget_returns_nothing() {
        let candidates = vec![make_message("hello"), make_message("world")];
        let (retained, tokens) = assemble_within_budget(candidates, 0);
        assert!(retained.is_empty(), "budget=0 must retain no messages");
        assert_eq!(tokens, 0);
    }

    #[test]
    fn assemble_within_budget_truncates_at_limit() {
        // estimate_tokens = chars / 4. Each message: "a " * 400 = 800 chars = 200 tokens.
        // Budget 250 fits exactly one (200 <= 250) but not two (200 + 200 = 400 > 250).
        let msg = "a ".repeat(400);
        let candidates = vec![make_message(&msg), make_message(&msg)];
        let (retained, tokens) = assemble_within_budget(candidates, 250);
        assert_eq!(
            retained.len(),
            1,
            "tight budget must keep only first message"
        );
        assert_eq!(tokens, 200);
    }

    #[test]
    fn parse_validation_response_missing_fields_defaults_to_sufficient() {
        // Neither "sufficient" nor "confidence" present → defaults: sufficient=true, confidence=1.0
        let raw = "{}";
        assert!(
            parse_validation_response(raw, 0.6),
            "missing fields must default to sufficient"
        );
    }

    #[test]
    fn tiered_retrieval_config_defaults() {
        let cfg = TieredRetrievalConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.token_budget, 4096);
        assert!(!cfg.validation_enabled);
        assert_eq!(cfg.max_escalations, 1);
        // Verify config-driven timeout defaults (fix #4250).
        assert_eq!(cfg.classifier_timeout_secs, 5);
        assert_eq!(cfg.validator_timeout_secs, 5);
    }

    #[test]
    fn tiered_retrieval_config_timeout_fields_propagate() {
        // Verify that custom timeout values survive a round-trip through the struct.
        let cfg = TieredRetrievalConfig {
            classifier_timeout_secs: 10,
            validator_timeout_secs: 15,
            ..TieredRetrievalConfig::default()
        };
        assert_eq!(cfg.classifier_timeout_secs, 10);
        assert_eq!(cfg.validator_timeout_secs, 15);
        // Confirm the durations would be built correctly from the fields.
        let classifier_dur = std::time::Duration::from_secs(cfg.classifier_timeout_secs);
        let validator_dur = std::time::Duration::from_secs(cfg.validator_timeout_secs);
        assert_eq!(classifier_dur.as_secs(), 10);
        assert_eq!(validator_dur.as_secs(), 15);
    }

    #[test]
    fn parse_validation_response_sufficient() {
        let raw = r#"{"sufficient": true, "confidence": 0.9}"#;
        assert!(parse_validation_response(raw, 0.6));
    }

    #[test]
    fn parse_validation_response_insufficient() {
        let raw = r#"{"sufficient": false, "confidence": 0.4}"#;
        assert!(!parse_validation_response(raw, 0.6));
    }

    #[test]
    fn parse_validation_response_low_confidence() {
        let raw = r#"{"sufficient": true, "confidence": 0.3}"#;
        // threshold = 0.6, confidence 0.3 < 0.6 → insufficient
        assert!(!parse_validation_response(raw, 0.6));
    }

    #[test]
    fn parse_validation_response_malformed_json_treats_as_sufficient() {
        let raw = "not json at all";
        assert!(parse_validation_response(raw, 0.6));
    }

    #[test]
    fn intent_class_display() {
        assert_eq!(IntentClass::ProfileLookup.to_string(), "ProfileLookup");
        assert_eq!(
            IntentClass::TargetedRetrieval.to_string(),
            "TargetedRetrieval"
        );
        assert_eq!(IntentClass::DeepReasoning.to_string(), "DeepReasoning");
    }

    // ── Async tests ───────────────────────────────────────────────────────────

    /// Test 1: `recall_tiered` with `classifier = None` uses the `HeuristicRouter` path.
    ///
    /// With no classifier provider, the pipeline must route via heuristic, complete without
    /// error, and return a result whose intent maps from the heuristic route.
    #[tokio::test]
    async fn recall_tiered_no_classifier_uses_heuristic_router() {
        let memory = crate::testing::mock_semantic_memory()
            .await
            .expect("mock_semantic_memory");
        let config = TieredRetrievalConfig {
            enabled: true,
            validation_enabled: false,
            ..TieredRetrievalConfig::default()
        };

        let result = recall_tiered(&memory, "what is my name", None, None, None, &config, None)
            .await
            .expect("recall_tiered must not fail");

        // HeuristicRouter classifies "what is my name" via keyword/semantic heuristic.
        // The exact tier depends on the heuristic, but the pipeline must complete.
        assert!(
            !result.tier_escalated,
            "no escalation when validation is off"
        );
        assert!(result.tokens_used <= config.token_budget);
    }

    /// Test 2: `recall_tiered` with `classifier = Some(...)` exercises the `HybridRouter` path.
    ///
    /// The mock LLM returns a JSON route decision; the pipeline must parse it and use the
    /// resulting intent class.
    #[tokio::test]
    async fn recall_tiered_with_classifier_uses_hybrid_router() {
        use zeph_llm::mock::MockProvider;

        let memory = crate::testing::mock_semantic_memory()
            .await
            .expect("mock_semantic_memory");

        // HybridRouter asks the LLM for a route; respond with a valid JSON route decision.
        let route_json = r#"{"route": "Semantic", "confidence": 0.9}"#.to_owned();
        let mut mock = MockProvider::with_responses(vec![route_json]);
        mock.supports_embeddings = true;
        mock.embedding = vec![0.1_f32; 384];
        let classifier = Arc::new(AnyProvider::Mock(mock));

        let config = TieredRetrievalConfig {
            enabled: true,
            validation_enabled: false,
            ..TieredRetrievalConfig::default()
        };

        let result = recall_tiered(
            &memory,
            "semantic query about the user",
            None,
            Some(&classifier),
            None,
            &config,
            None,
        )
        .await
        .expect("recall_tiered with classifier must not fail");

        assert!(!result.tier_escalated);
        assert!(result.tokens_used <= config.token_budget);
    }

    /// Test 3: Escalation loop sets `tier_escalated = true` when the validator returns
    /// insufficient evidence and a heavier tier is available.
    ///
    /// Validator response with `{"sufficient": false, "confidence": 0.2}` triggers escalation.
    /// After escalation, the second-tier retrieve runs and the result has `tier_escalated = true`.
    #[tokio::test]
    async fn recall_tiered_escalates_when_evidence_insufficient() {
        use zeph_llm::mock::MockProvider;

        let memory = crate::testing::mock_semantic_memory()
            .await
            .expect("mock_semantic_memory");

        // First validator response: insufficient. Second: sufficient (prevents infinite loop).
        let insufficient = r#"{"sufficient": false, "confidence": 0.1}"#.to_owned();
        let sufficient = r#"{"sufficient": true, "confidence": 0.95}"#.to_owned();
        let mut validator_mock = MockProvider::with_responses(vec![insufficient, sufficient]);
        validator_mock.supports_embeddings = true;
        let validator = Arc::new(AnyProvider::Mock(validator_mock));

        let config = TieredRetrievalConfig {
            enabled: true,
            validation_enabled: true,
            validation_threshold: 0.6,
            max_escalations: 2,
            ..TieredRetrievalConfig::default()
        };

        let result = recall_tiered(
            &memory,
            "deep query",
            None,
            None,
            Some(&validator),
            &config,
            None,
        )
        .await
        .expect("escalation path must not fail");

        assert!(
            result.tier_escalated,
            "must set tier_escalated when validator triggers escalation"
        );
    }

    /// Test 4a: `validate_evidence` returns `true` (fail-open) when the validator LLM times out.
    ///
    /// Uses `with_delay` to force the validator past the configured timeout threshold.
    /// The pipeline must treat a timed-out validator as sufficient (fail-open) and not escalate.
    #[tokio::test]
    async fn validate_evidence_timeout_is_fail_open() {
        use zeph_llm::mock::MockProvider;

        let memory = crate::testing::mock_semantic_memory()
            .await
            .expect("mock_semantic_memory");

        // Store a message so validate_evidence gets a non-empty slice and actually calls the LLM.
        let conv_id = memory
            .sqlite()
            .create_conversation()
            .await
            .expect("create_conversation");
        memory
            .remember(conv_id, "user", "some evidence content", None)
            .await
            .expect("remember");

        // Delay > validator_timeout_secs causes the internal tokio::time::timeout to fire.
        let slow_mock = MockProvider::default().with_delay(6_000);
        let validator = Arc::new(AnyProvider::Mock(slow_mock));

        let config = TieredRetrievalConfig {
            enabled: true,
            validation_enabled: true,
            validation_threshold: 0.6,
            max_escalations: 1,
            validator_timeout_secs: 5,
            ..TieredRetrievalConfig::default()
        };

        // The slow validator should time out and be treated as sufficient → no escalation.
        let result = recall_tiered(
            &memory,
            "evidence",
            None,
            None,
            Some(&validator),
            &config,
            None,
        )
        .await
        .expect("timeout path must not propagate as error");

        // Fail-open: timed-out validator means no escalation.
        assert!(
            !result.tier_escalated,
            "validator timeout must be treated as sufficient (fail-open)"
        );
    }

    /// Test 4b: `validate_evidence` returns `true` (fail-open) when the validator LLM errors.
    ///
    /// A failing provider simulates a transient API error. The pipeline must not escalate.
    #[tokio::test]
    async fn validate_evidence_llm_error_is_fail_open() {
        use zeph_llm::mock::MockProvider;

        let memory = crate::testing::mock_semantic_memory()
            .await
            .expect("mock_semantic_memory");

        // Store a message so validate_evidence gets a non-empty slice and actually calls the LLM.
        let conv_id = memory
            .sqlite()
            .create_conversation()
            .await
            .expect("create_conversation");
        memory
            .remember(conv_id, "user", "some evidence content", None)
            .await
            .expect("remember");

        let failing_mock = MockProvider::failing();
        let validator = Arc::new(AnyProvider::Mock(failing_mock));

        let config = TieredRetrievalConfig {
            enabled: true,
            validation_enabled: true,
            validation_threshold: 0.6,
            max_escalations: 1,
            ..TieredRetrievalConfig::default()
        };

        let result = recall_tiered(
            &memory,
            "evidence",
            None,
            None,
            Some(&validator),
            &config,
            None,
        )
        .await
        .expect("LLM error path must not propagate as retrieval error");

        assert!(
            !result.tier_escalated,
            "validator LLM error must be treated as sufficient (fail-open)"
        );
    }

    /// Test 5: `recall_tiered` with a `conversation_id` filter passes it to `retrieve_tier`,
    /// which in turn applies a `SearchFilter` scoping the search to that conversation.
    ///
    /// The pipeline must complete successfully even when the filter yields zero results.
    #[tokio::test]
    async fn recall_tiered_with_conversation_id_filter() {
        let memory = crate::testing::mock_semantic_memory()
            .await
            .expect("mock_semantic_memory");

        let conv_id = ConversationId(42);
        let config = TieredRetrievalConfig {
            enabled: true,
            validation_enabled: false,
            ..TieredRetrievalConfig::default()
        };

        let result = recall_tiered(
            &memory,
            "what did we discuss",
            Some(conv_id),
            None,
            None,
            &config,
            None,
        )
        .await
        .expect("conversation-scoped recall must not fail");

        // No messages stored for this conversation — result must be empty but valid.
        assert!(result.messages.is_empty());
        assert_eq!(result.tokens_used, 0);
        assert!(!result.tier_escalated);
    }

    /// Test 6: `deep_reasoning_query_conditioned = true` with an empty HELA graph falls back to
    /// `recall_routed` without panicking.
    ///
    /// `mock_semantic_memory` has no `graph_store`, so `recall_graph_hela` returns
    /// `Ok(Vec::new())`.  The code at `tiered_retrieval.rs:307` logs "no results" and falls
    /// through to `recall_routed`, which must succeed and return a valid `TieredRetrievalResult`.
    #[tokio::test]
    async fn deep_reasoning_query_conditioned_true_falls_back_when_hela_empty() {
        let memory = crate::testing::mock_semantic_memory()
            .await
            .expect("mock_semantic_memory");

        let config = TieredRetrievalConfig {
            deep_reasoning_query_conditioned: true,
            validation_enabled: false,
            ..TieredRetrievalConfig::default()
        };

        let result = retrieve_tier(
            &memory,
            "multi-hop reasoning query",
            None,
            IntentClass::DeepReasoning,
            &config,
        )
        .await
        .expect("retrieve_tier with empty HELA must not fail");

        // HELA returned nothing → fallback to recall_routed → empty result (no stored msgs).
        assert!(
            result.is_empty(),
            "expected empty result from fallback recall_routed, got {}",
            result.len()
        );
    }

    /// Test 7: `deep_reasoning_query_conditioned = false` with `DeepReasoning` intent completes
    /// without panicking.
    ///
    /// Verifies that the pipeline completes successfully and returns a valid (empty) result when
    /// the flag is disabled. The mock has no stored messages, so `recall_routed` returns empty.
    #[tokio::test]
    async fn deep_reasoning_query_conditioned_false_completes_without_panic() {
        let memory = crate::testing::mock_semantic_memory()
            .await
            .expect("mock_semantic_memory");

        let config = TieredRetrievalConfig {
            deep_reasoning_query_conditioned: false,
            validation_enabled: false,
            ..TieredRetrievalConfig::default()
        };

        let result = retrieve_tier(
            &memory,
            "multi-hop reasoning query",
            None,
            IntentClass::DeepReasoning,
            &config,
        )
        .await
        .expect("retrieve_tier with deep_reasoning_query_conditioned=false must not fail");

        // recall_routed path used; no messages stored so result is empty.
        assert!(
            result.is_empty(),
            "expected empty result from recall_routed path, got {}",
            result.len()
        );
    }
}
