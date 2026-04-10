// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A-MAC adaptive memory admission control (#2317).
//!
//! Write-time gate inserted before `SQLite` persistence in `remember()` and `remember_with_parts()`.
//! Evaluates 5 factors and rejects messages below the configured threshold.

use std::sync::Arc;
use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::embedding_store::EmbeddingStore;
use zeph_common::math::cosine_similarity;

/// Per-factor scores for the admission decision.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdmissionFactors {
    /// LLM-estimated reuse probability. `[0, 1]`. Set to 0.5 on fast path or LLM failure.
    pub future_utility: f32,
    /// Inverse hedging heuristic: high confidence → high score. `[0, 1]`.
    pub factual_confidence: f32,
    /// `1.0 - max_similarity_to_top3_neighbors`. `[0, 1]`. 1.0 when memory is empty.
    pub semantic_novelty: f32,
    /// Always `1.0` at write time (decay applied at recall). `[0, 1]`.
    pub temporal_recency: f32,
    /// Prior based on message role. `[0, 1]`.
    pub content_type_prior: f32,
    /// Goal-conditioned utility (#2408). Cosine similarity between goal embedding and
    /// candidate memory. `0.0` when `goal_conditioned_write = false` or goal text is absent/trivial.
    pub goal_utility: f32,
}

/// Result of an admission evaluation.
#[derive(Debug, Clone)]
pub struct AdmissionDecision {
    pub admitted: bool,
    pub composite_score: f32,
    pub factors: AdmissionFactors,
}

/// Normalized weights for the composite score.
#[derive(Debug, Clone, Copy)]
pub struct AdmissionWeights {
    pub future_utility: f32,
    pub factual_confidence: f32,
    pub semantic_novelty: f32,
    pub temporal_recency: f32,
    pub content_type_prior: f32,
    /// Goal-conditioned utility weight. `0.0` when goal gate is disabled.
    pub goal_utility: f32,
}

impl AdmissionWeights {
    /// Return a copy with all fields clamped to `>= 0.0` and normalized so they sum to `1.0`.
    ///
    /// Falls back to equal weights when the sum is effectively zero (all fields were zero/negative).
    #[must_use]
    pub fn normalized(&self) -> Self {
        let fu = self.future_utility.max(0.0);
        let fc = self.factual_confidence.max(0.0);
        let sn = self.semantic_novelty.max(0.0);
        let tr = self.temporal_recency.max(0.0);
        let cp = self.content_type_prior.max(0.0);
        let gu = self.goal_utility.max(0.0);
        let sum = fu + fc + sn + tr + cp + gu;
        if sum <= f32::EPSILON {
            // Equal fallback weights (6 factors when goal gate is enabled).
            return Self {
                future_utility: 0.2,
                factual_confidence: 0.2,
                semantic_novelty: 0.2,
                temporal_recency: 0.2,
                content_type_prior: 0.2,
                goal_utility: 0.0,
            };
        }
        Self {
            future_utility: fu / sum,
            factual_confidence: fc / sum,
            semantic_novelty: sn / sum,
            temporal_recency: tr / sum,
            content_type_prior: cp / sum,
            goal_utility: gu / sum,
        }
    }
}

/// Goal-conditioned write gate configuration for `AdmissionControl`.
#[derive(Debug, Clone)]
pub struct GoalGateConfig {
    /// Minimum cosine similarity to consider memory goal-relevant.
    pub threshold: f32,
    /// LLM provider for borderline refinement (similarity within 0.1 of threshold).
    pub provider: Option<AnyProvider>,
    /// Weight of the `goal_utility` factor in the composite score.
    pub weight: f32,
}

/// A-MAC adaptive memory admission controller (#2317).
///
/// Evaluates five factors (future utility, factual confidence, semantic novelty,
/// temporal recency, content-type prior) and rejects messages below the configured
/// composite score threshold before they are persisted.
///
/// Optionally extended with a goal-conditioned write gate (#2408) that adds a
/// sixth factor based on the cosine similarity between the current goal embedding
/// and the candidate memory.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_memory::{AdmissionControl, AdmissionWeights};
///
/// let weights = AdmissionWeights {
///     future_utility: 0.3,
///     factual_confidence: 0.2,
///     semantic_novelty: 0.2,
///     temporal_recency: 0.1,
///     content_type_prior: 0.2,
///     goal_utility: 0.0,
/// };
/// let controller = AdmissionControl::new(0.4, 0.1, weights);
/// ```
pub struct AdmissionControl {
    threshold: f32,
    fast_path_margin: f32,
    weights: AdmissionWeights,
    /// Dedicated provider for LLM-based evaluation. Falls back to the caller-supplied provider
    /// when `None` (e.g. in tests or when `admission_provider` is not configured).
    provider: Option<AnyProvider>,
    /// Goal-conditioned write gate. `None` when `goal_conditioned_write = false`.
    goal_gate: Option<GoalGateConfig>,
}

impl AdmissionControl {
    /// Create a new admission controller.
    ///
    /// - `threshold` — composite score `[0, 1]` below which messages are rejected.
    /// - `fast_path_margin` — when all non-LLM factors already push the score far above
    ///   the threshold (by at least this margin), the LLM `future_utility` call is skipped.
    /// - `weights` — factor weights; normalized automatically so they sum to `1.0`.
    #[must_use]
    pub fn new(threshold: f32, fast_path_margin: f32, weights: AdmissionWeights) -> Self {
        Self {
            threshold,
            fast_path_margin,
            weights: weights.normalized(),
            provider: None,
            goal_gate: None,
        }
    }

    /// Attach a dedicated LLM provider for `future_utility` evaluation.
    ///
    /// When set, this provider is used instead of the caller-supplied fallback.
    #[must_use]
    pub fn with_provider(mut self, provider: AnyProvider) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Enable goal-conditioned write gate (#2408).
    #[must_use]
    pub fn with_goal_gate(mut self, config: GoalGateConfig) -> Self {
        // Redistribute goal_utility weight from future_utility.
        let gu = config.weight.clamp(0.0, 1.0);
        let mut weights = self.weights;
        weights.goal_utility = gu;
        // Reduce future_utility by the same amount (soft redistribution).
        weights.future_utility = (weights.future_utility - gu).max(0.0);
        self.weights = weights.normalized();
        self.goal_gate = Some(config);
        self
    }

    /// Return the configured admission threshold.
    #[must_use]
    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// Evaluate admission for a message.
    ///
    /// `goal_text`: optional current-turn goal context for goal-conditioned scoring.
    /// Ignored when the goal gate is disabled or `goal_text` is `None`/trivial (< 10 chars).
    ///
    /// Fast path: skips LLM when heuristic-only score is already above `threshold + fast_path_margin`.
    /// Slow path: calls LLM for `future_utility` when borderline.
    ///
    /// On LLM failure, `future_utility` defaults to `0.5` (neutral).
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.admission", skip_all)
    )]
    pub async fn evaluate(
        &self,
        content: &str,
        role: &str,
        fallback_provider: &AnyProvider,
        qdrant: Option<&Arc<EmbeddingStore>>,
        goal_text: Option<&str>,
    ) -> AdmissionDecision {
        let effective_provider = self.provider.as_ref().unwrap_or(fallback_provider);
        let factual_confidence = compute_factual_confidence(content);
        let temporal_recency = 1.0f32;
        let content_type_prior = compute_content_type_prior(role);

        // Semantic novelty requires an async embedding search.
        let semantic_novelty = compute_semantic_novelty(content, effective_provider, qdrant).await;

        // Goal-conditioned utility (W3.1 fix: skip trivial goal text < 10 chars).
        let goal_utility = match &self.goal_gate {
            Some(gate) => {
                let effective_goal = goal_text.filter(|t| t.trim().len() >= 10);
                if let Some(goal) = effective_goal {
                    compute_goal_utility(content, goal, gate, effective_provider, qdrant).await
                } else {
                    0.0
                }
            }
            None => 0.0,
        };

        // Heuristic-only composite (future_utility treated as 0.5 neutral placeholder).
        let heuristic_score = self.weighted_score(
            0.5,
            factual_confidence,
            semantic_novelty,
            temporal_recency,
            content_type_prior,
            goal_utility,
        );

        // Fast path: admit without LLM if score is clearly above threshold + margin.
        let future_utility = if heuristic_score >= self.threshold + self.fast_path_margin {
            0.5 // not used in final score since we admit early, but kept for audit
        } else {
            compute_future_utility(content, role, effective_provider).await
        };

        let composite_score = self.weighted_score(
            future_utility,
            factual_confidence,
            semantic_novelty,
            temporal_recency,
            content_type_prior,
            goal_utility,
        );

        let admitted = composite_score >= self.threshold
            || heuristic_score >= self.threshold + self.fast_path_margin;

        AdmissionDecision {
            admitted,
            composite_score,
            factors: AdmissionFactors {
                future_utility,
                factual_confidence,
                semantic_novelty,
                temporal_recency,
                content_type_prior,
                goal_utility,
            },
        }
    }

    fn weighted_score(
        &self,
        future_utility: f32,
        factual_confidence: f32,
        semantic_novelty: f32,
        temporal_recency: f32,
        content_type_prior: f32,
        goal_utility: f32,
    ) -> f32 {
        future_utility * self.weights.future_utility
            + factual_confidence * self.weights.factual_confidence
            + semantic_novelty * self.weights.semantic_novelty
            + temporal_recency * self.weights.temporal_recency
            + content_type_prior * self.weights.content_type_prior
            + goal_utility * self.weights.goal_utility
    }
}

/// Heuristic: detect hedging markers and compute confidence score.
///
/// Returns `1.0` for confident content, lower for content with hedging language.
#[must_use]
pub fn compute_factual_confidence(content: &str) -> f32 {
    // Common English hedging markers. Content in other languages scores 1.0 (no penalty).
    const HEDGING_MARKERS: &[&str] = &[
        "maybe",
        "might",
        "perhaps",
        "i think",
        "i believe",
        "not sure",
        "could be",
        "possibly",
        "probably",
        "uncertain",
        "not certain",
        "i'm not sure",
        "im not sure",
        "not confident",
    ];
    let lower = content.to_lowercase();
    let matches = HEDGING_MARKERS
        .iter()
        .filter(|&&m| lower.contains(m))
        .count();
    // Each hedging marker reduces confidence by 0.1, min 0.2.
    #[allow(clippy::cast_precision_loss)]
    let penalty = (matches as f32) * 0.1;
    (1.0 - penalty).max(0.2)
}

/// Prior score based on message role.
///
/// Tool results (role "tool") are treated as high-value since they contain factual data.
/// The table is not symmetric to role importance — it's calibrated by typical content density.
#[must_use]
pub fn compute_content_type_prior(role: &str) -> f32 {
    match role {
        "user" => 0.7,
        "assistant" => 0.6,
        "tool" | "tool_result" => 0.8,
        "system" => 0.3,
        _ => 0.5,
    }
}

/// Compute semantic novelty as `1.0 - max_cosine_similarity_to_top3_neighbors`.
///
/// Returns `1.0` when the memory is empty (everything is novel at cold start).
async fn compute_semantic_novelty(
    content: &str,
    provider: &AnyProvider,
    qdrant: Option<&Arc<EmbeddingStore>>,
) -> f32 {
    let Some(store) = qdrant else {
        return 1.0;
    };
    if !provider.supports_embeddings() {
        return 1.0;
    }
    let vector = match provider.embed(content).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "A-MAC: failed to embed for novelty, using 1.0");
            return 1.0;
        }
    };
    let Ok(vector_size) = u64::try_from(vector.len()) else {
        return 1.0;
    };
    if let Err(e) = store.ensure_collection(vector_size).await {
        tracing::debug!(error = %e, "A-MAC: collection not ready for novelty check");
        return 1.0;
    }
    let results = match store.search(&vector, 3, None).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "A-MAC: novelty search failed, using 1.0");
            return 1.0;
        }
    };
    let max_sim = results.iter().map(|r| r.score).fold(0.0f32, f32::max);
    (1.0 - max_sim).max(0.0)
}

/// LLM-based future utility estimate.
///
/// On timeout or error, returns `0.5` (neutral — no bias toward admit or reject).
async fn compute_future_utility(content: &str, role: &str, provider: &AnyProvider) -> f32 {
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    let system = "You are a memory relevance judge. Rate how likely this message will be \
        referenced in future conversations on a scale of 0.0 to 1.0. \
        Respond with ONLY a decimal number between 0.0 and 1.0, nothing else.";

    let user = format!(
        "Role: {role}\nContent: {}\n\nFuture utility score (0.0-1.0):",
        content.chars().take(500).collect::<String>()
    );

    let messages = vec![
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

    let result = match tokio::time::timeout(Duration::from_secs(8), provider.chat(&messages)).await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "A-MAC: future_utility LLM call failed, using 0.5");
            return 0.5;
        }
        Err(_) => {
            tracing::debug!("A-MAC: future_utility LLM timed out, using 0.5");
            return 0.5;
        }
    };

    result.trim().parse::<f32>().unwrap_or(0.5).clamp(0.0, 1.0)
}

/// Compute goal-conditioned utility for a candidate memory.
///
/// Embeds the goal text and candidate content, then returns cosine similarity.
/// For borderline cases (similarity within 0.1 of threshold), optionally calls
/// the LLM for refinement if a `goal_utility_provider` is configured.
///
/// Returns a soft-floored score: min similarity is 0.1 to avoid fully eliminating
/// memories that are somewhat off-goal but otherwise high-value (W3.4 fix).
///
/// Returns `0.0` on embedding failure (safe admission without goal factor).
async fn compute_goal_utility(
    content: &str,
    goal_text: &str,
    gate: &GoalGateConfig,
    provider: &AnyProvider,
    qdrant: Option<&Arc<EmbeddingStore>>,
) -> f32 {
    use zeph_llm::provider::LlmProvider as _;

    if !provider.supports_embeddings() {
        return 0.0;
    }

    let goal_emb = match provider.embed(goal_text).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "goal_utility: failed to embed goal text, using 0.0");
            return 0.0;
        }
    };
    let content_emb = match provider.embed(content).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "goal_utility: failed to embed content, using 0.0");
            return 0.0;
        }
    };

    // Qdrant is used for novelty search, not for goal utility — we compute cosine directly.
    let _ = qdrant; // unused here; kept for API symmetry

    let similarity = cosine_similarity(&goal_emb, &content_emb);

    // Borderline: call LLM for refinement when configured (W3.5: skipped when no provider).
    let borderline_lo = gate.threshold - 0.1;
    let borderline_hi = gate.threshold + 0.1;
    let in_borderline = similarity >= borderline_lo && similarity <= borderline_hi;

    let final_similarity = if in_borderline {
        if let Some(ref goal_provider) = gate.provider {
            refine_goal_utility_llm(content, goal_text, similarity, goal_provider).await
        } else {
            similarity
        }
    } else {
        similarity
    };

    // Hard gate below threshold; soft floor of 0.1 above threshold (W3.4 fix).
    if final_similarity < gate.threshold {
        0.0
    } else {
        final_similarity.max(0.1)
    }
}

/// LLM-based goal utility refinement for borderline cases.
///
/// Returns the original `embedding_sim` on failure (safe fallback).
async fn refine_goal_utility_llm(
    content: &str,
    goal_text: &str,
    embedding_sim: f32,
    provider: &AnyProvider,
) -> f32 {
    use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

    let system = "You are a memory relevance judge. Given a task goal and a candidate memory, \
        rate how relevant the memory is to the goal on a scale of 0.0 to 1.0. \
        Respond with ONLY a decimal number between 0.0 and 1.0, nothing else.";

    let user = format!(
        "Goal: {}\nMemory: {}\n\nRelevance score (0.0-1.0):",
        goal_text.chars().take(200).collect::<String>(),
        content.chars().take(300).collect::<String>(),
    );

    let messages = vec![
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

    let result = match tokio::time::timeout(Duration::from_secs(6), provider.chat(&messages)).await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "goal_utility LLM refinement failed, using embedding sim");
            return embedding_sim;
        }
        Err(_) => {
            tracing::debug!("goal_utility LLM refinement timed out, using embedding sim");
            return embedding_sim;
        }
    };

    result
        .trim()
        .parse::<f32>()
        .unwrap_or(embedding_sim)
        .clamp(0.0, 1.0)
}

/// Log an admission decision to the audit log via `tracing`.
///
/// Rejections are always logged at debug level. Admissions are trace-level.
pub fn log_admission_decision(
    decision: &AdmissionDecision,
    content_preview: &str,
    role: &str,
    threshold: f32,
) {
    if decision.admitted {
        tracing::trace!(
            role,
            composite_score = decision.composite_score,
            threshold,
            content_preview,
            "A-MAC: admitted"
        );
    } else {
        tracing::debug!(
            role,
            composite_score = decision.composite_score,
            threshold,
            future_utility = decision.factors.future_utility,
            factual_confidence = decision.factors.factual_confidence,
            semantic_novelty = decision.factors.semantic_novelty,
            content_type_prior = decision.factors.content_type_prior,
            content_preview,
            "A-MAC: rejected"
        );
    }
}

/// Error type for admission-rejected persists.
#[derive(Debug)]
pub struct AdmissionRejected {
    pub composite_score: f32,
    pub threshold: f32,
}

impl std::fmt::Display for AdmissionRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "A-MAC admission rejected (score={:.3} < threshold={:.3})",
            self.composite_score, self.threshold
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factual_confidence_no_hedging() {
        assert!((compute_factual_confidence("The server uses TLS 1.3.") - 1.0).abs() < 0.01);
    }

    #[test]
    fn factual_confidence_with_one_marker() {
        let score = compute_factual_confidence("Maybe we should use TLS 1.3.");
        assert!((score - 0.9).abs() < 0.01);
    }

    #[test]
    fn factual_confidence_many_markers_floors_at_0_2() {
        let content = "maybe i think perhaps possibly might not sure i believe";
        let score = compute_factual_confidence(content);
        assert!(score >= 0.2);
        assert!(score < 0.5);
    }

    #[test]
    fn content_type_prior_values() {
        assert!((compute_content_type_prior("user") - 0.7).abs() < 0.01);
        assert!((compute_content_type_prior("assistant") - 0.6).abs() < 0.01);
        assert!((compute_content_type_prior("tool") - 0.8).abs() < 0.01);
        assert!((compute_content_type_prior("system") - 0.3).abs() < 0.01);
        assert!((compute_content_type_prior("unknown") - 0.5).abs() < 0.01);
    }

    #[test]
    fn admission_control_admits_high_score() {
        let weights = AdmissionWeights {
            future_utility: 0.30,
            factual_confidence: 0.15,
            semantic_novelty: 0.30,
            temporal_recency: 0.10,
            content_type_prior: 0.15,
            goal_utility: 0.0,
        };
        let ctrl = AdmissionControl::new(0.40, 0.15, weights);
        // Score all factors at 1.0 → composite = 1.0.
        let score = ctrl.weighted_score(1.0, 1.0, 1.0, 1.0, 1.0, 0.0);
        assert!(score >= 0.99);
        // Admitted when score >= threshold.
        let admitted = score >= ctrl.threshold;
        assert!(admitted);
    }

    #[test]
    fn admission_control_rejects_low_score() {
        let weights = AdmissionWeights {
            future_utility: 0.30,
            factual_confidence: 0.15,
            semantic_novelty: 0.30,
            temporal_recency: 0.10,
            content_type_prior: 0.15,
            goal_utility: 0.0,
        };
        let ctrl = AdmissionControl::new(0.40, 0.15, weights);
        // Score all factors at 0.0 → composite = 0.0.
        let score = ctrl.weighted_score(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        assert!(score < ctrl.threshold);
    }

    // Test: fast-path score above threshold + margin bypasses slow-path (LLM call skipped).
    // We verify the branch logic in weighted_score: if heuristic >= threshold + margin, admitted.
    #[test]
    fn fast_path_admits_when_heuristic_above_threshold_plus_margin() {
        let weights = AdmissionWeights {
            future_utility: 0.20,
            factual_confidence: 0.20,
            semantic_novelty: 0.20,
            temporal_recency: 0.20,
            content_type_prior: 0.20,
            goal_utility: 0.0,
        };
        let threshold = 0.40f32;
        let margin = 0.15f32;
        let ctrl = AdmissionControl::new(threshold, margin, weights);

        // All non-future_utility factors at 1.0; future_utility treated as 0.5 (fast path neutral).
        let heuristic = ctrl.weighted_score(0.5, 1.0, 1.0, 1.0, 1.0, 0.0);
        // heuristic = 0.5*0.2 + 1.0*0.2 + 1.0*0.2 + 1.0*0.2 + 1.0*0.2 = 0.1 + 0.8 = 0.9
        assert!(
            heuristic >= threshold + margin,
            "heuristic {heuristic} must exceed threshold+margin {}",
            threshold + margin
        );
        // In evaluate(), admitted = composite >= threshold || heuristic >= threshold + margin.
        let admitted = heuristic >= threshold + margin;
        assert!(admitted, "fast path must admit without LLM call");
    }

    // Test: slow-path engages when heuristic is below threshold + margin.
    #[test]
    fn slow_path_required_when_heuristic_below_threshold_plus_margin() {
        let weights = AdmissionWeights {
            future_utility: 0.40,
            factual_confidence: 0.15,
            semantic_novelty: 0.15,
            temporal_recency: 0.15,
            content_type_prior: 0.15,
            goal_utility: 0.0,
        };
        let threshold = 0.50f32;
        let margin = 0.20f32;
        let ctrl = AdmissionControl::new(threshold, margin, weights);

        // All factors low — heuristic will be below threshold + margin.
        let heuristic = ctrl.weighted_score(0.5, 0.3, 0.3, 0.3, 0.3, 0.0);
        assert!(
            heuristic < threshold + margin,
            "heuristic {heuristic} must be below threshold+margin {}",
            threshold + margin
        );
    }

    // Test: log_admission_decision runs without panic for both admitted and rejected.
    #[test]
    fn log_admission_decision_does_not_panic() {
        let admitted_decision = AdmissionDecision {
            admitted: true,
            composite_score: 0.75,
            factors: AdmissionFactors {
                future_utility: 0.8,
                factual_confidence: 0.9,
                semantic_novelty: 0.7,
                temporal_recency: 1.0,
                content_type_prior: 0.7,
                goal_utility: 0.0,
            },
        };
        log_admission_decision(&admitted_decision, "preview text", "user", 0.40);

        let rejected_decision = AdmissionDecision {
            admitted: false,
            composite_score: 0.20,
            factors: AdmissionFactors {
                future_utility: 0.1,
                factual_confidence: 0.2,
                semantic_novelty: 0.3,
                temporal_recency: 1.0,
                content_type_prior: 0.3,
                goal_utility: 0.0,
            },
        };
        log_admission_decision(&rejected_decision, "maybe short content", "assistant", 0.40);
    }

    // Test: AdmissionRejected Display formats correctly.
    #[test]
    fn admission_rejected_display() {
        let err = AdmissionRejected {
            composite_score: 0.25,
            threshold: 0.45,
        };
        let msg = format!("{err}");
        assert!(msg.contains("0.250"));
        assert!(msg.contains("0.450"));
    }

    // Test: threshold() accessor returns the configured value.
    #[test]
    fn threshold_accessor() {
        let weights = AdmissionWeights {
            future_utility: 0.20,
            factual_confidence: 0.20,
            semantic_novelty: 0.20,
            temporal_recency: 0.20,
            content_type_prior: 0.20,
            goal_utility: 0.0,
        };
        let ctrl = AdmissionControl::new(0.55, 0.10, weights);
        assert!((ctrl.threshold() - 0.55).abs() < 0.001);
    }

    // Test: content_type_prior for "tool_result" alias.
    #[test]
    fn content_type_prior_tool_result_alias() {
        assert!((compute_content_type_prior("tool_result") - 0.8).abs() < 0.01);
    }

    // ── cosine_similarity tests ───────────────────────────────────────────────

    #[test]
    fn cosine_similarity_identical_vectors() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_zero_vector_returns_zero() {
        let z = vec![0.0f32, 0.0, 0.0];
        let v = vec![1.0f32, 2.0, 3.0];
        assert!(cosine_similarity(&z, &v).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_length_mismatch_returns_zero() {
        let a = vec![1.0f32, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    // ── with_goal_gate tests ──────────────────────────────────────────────────

    #[test]
    fn with_goal_gate_sets_goal_utility_weight() {
        let weights = AdmissionWeights {
            future_utility: 0.30,
            factual_confidence: 0.15,
            semantic_novelty: 0.30,
            temporal_recency: 0.10,
            content_type_prior: 0.15,
            goal_utility: 0.0,
        };
        let ctrl = AdmissionControl::new(0.40, 0.15, weights);
        let config = GoalGateConfig {
            weight: 0.20,
            threshold: 0.5,
            provider: None,
        };
        let ctrl = ctrl.with_goal_gate(config);
        assert!(
            ctrl.weights.goal_utility > 0.0,
            "goal_utility must be nonzero after with_goal_gate"
        );
        // Normalized weights must sum to ~1.0.
        let w = &ctrl.weights;
        let total = w.future_utility
            + w.factual_confidence
            + w.semantic_novelty
            + w.temporal_recency
            + w.content_type_prior
            + w.goal_utility;
        assert!(
            (total - 1.0).abs() < 0.01,
            "normalized weights must sum to 1.0, got {total}"
        );
    }

    #[test]
    fn with_goal_gate_zero_weight_leaves_goal_utility_at_zero() {
        let weights = AdmissionWeights {
            future_utility: 0.30,
            factual_confidence: 0.15,
            semantic_novelty: 0.30,
            temporal_recency: 0.10,
            content_type_prior: 0.15,
            goal_utility: 0.0,
        };
        let ctrl = AdmissionControl::new(0.40, 0.15, weights);
        let config = GoalGateConfig {
            weight: 0.0,
            threshold: 0.5,
            provider: None,
        };
        let ctrl = ctrl.with_goal_gate(config);
        assert!(ctrl.weights.goal_utility.abs() < f32::EPSILON);
    }
}
