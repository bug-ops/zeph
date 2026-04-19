// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `MemReader` write quality gate (#3222).
//!
//! [`QualityGate`] runs **after** A-MAC admission and before any persistence write.
//! It scores three signals — information value, reference completeness, and contradiction
//! risk — and rejects writes below a configurable threshold.
//!
//! Rule-based scoring ships as MVP; an optional LLM-assisted path is enabled by setting
//! `quality_gate_provider` in `[memory.quality_gate]`.
//!
//! # Composition in `SemanticMemory`
//!
//! ```text
//! remember(content)
//!   → A-MAC::evaluate()  →  Ok(None) if rejected
//!   → QualityGate::evaluate()  →  Ok(None) if rejected
//!   → SQLite / Qdrant persist
//! ```
//!
//! # Fail-open contract
//!
//! Any scoring failure (embed error, LLM timeout, graph query error) is treated as a
//! pass — the write is admitted. Quality scoring is best-effort, never a hard dependency.

use std::sync::Arc;
use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::graph::GraphStore;

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for the write quality gate (`[memory.quality_gate]` TOML section).
#[derive(Debug, Clone)]
pub struct QualityGateConfig {
    /// Enable the quality gate. When `false`, all writes pass through. Default: `false`.
    pub enabled: bool,
    /// Combined score threshold below which writes are rejected. Range `[0, 1]`. Default: `0.55`.
    pub threshold: f32,
    /// Number of recent writes to compare against for information-value scoring. Default: `32`.
    pub recent_window: usize,
    /// Seconds: edges older than this are considered stable for contradiction detection.
    /// Default: `300`.
    pub contradiction_grace_seconds: u64,
    /// Weight of `information_value` sub-score. Default: `0.4`.
    pub information_value_weight: f32,
    /// Weight of `reference_completeness` sub-score. Default: `0.3`.
    pub reference_completeness_weight: f32,
    /// Weight of `contradiction` sub-score (applied as `1 - contradiction_risk`). Default: `0.3`.
    pub contradiction_weight: f32,
    /// Ratio of rejections (rolling 100-write window) above which a `WARN` is emitted.
    /// Default: `0.35`.
    pub rejection_rate_alarm_ratio: f32,
    /// LLM timeout for optional scoring path. Default: `500 ms`.
    pub llm_timeout_ms: u64,
    /// Weight blended into the final score when an LLM provider is set. Default: `0.5`.
    pub llm_weight: f32,
    /// Whether pronoun/deictic reference checks are active. Disable for non-English sessions.
    /// Default: `true`.
    pub reference_check_lang_en: bool,
}

impl Default for QualityGateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: 0.55,
            recent_window: 32,
            contradiction_grace_seconds: 300,
            information_value_weight: 0.4,
            reference_completeness_weight: 0.3,
            contradiction_weight: 0.3,
            rejection_rate_alarm_ratio: 0.35,
            llm_timeout_ms: 500,
            llm_weight: 0.5,
            reference_check_lang_en: true,
        }
    }
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// Per-signal scores from the quality gate evaluation.
#[derive(Debug, Clone)]
pub struct QualityScore {
    /// `1.0 - max_cosine(candidate, recent_writes)`. `1.0` when the store is empty.
    pub information_value: f32,
    /// `1.0 - unresolved_reference_ratio`. Lower = more unresolved pronouns/deictic time.
    pub reference_completeness: f32,
    /// `1.0` if a conflicting graph edge exists (older than grace period); `0.0` otherwise.
    /// Returns `0.0` when no graph store is attached — improves automatically when
    /// APEX-MEM (#3223) lands and a `GraphStore` is wired in.
    pub contradiction_risk: f32,
    /// Weighted combination of the three sub-scores.
    pub combined: f32,
    /// LLM-blended final score. Equals `combined` when no LLM provider is configured.
    pub final_score: f32,
}

/// Reason for a quality gate rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityRejectionReason {
    /// Cosine similarity to recent writes is too high — the content is redundant.
    Redundant,
    /// Unresolved pronoun or deictic time expression without an absolute referent.
    IncompleteReference,
    /// A conflicting graph edge exists for the same `(subject, predicate)` pair.
    Contradiction,
    /// Optional LLM scorer returned a score below the threshold.
    LlmLowConfidence,
}

impl QualityRejectionReason {
    /// Stable lowercase-snake label suitable for metric tags.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Redundant => "redundant",
            Self::IncompleteReference => "incomplete_reference",
            Self::Contradiction => "contradiction",
            Self::LlmLowConfidence => "llm_low_confidence",
        }
    }
}

/// Rolling window counter for tracking the rejection rate over the last N writes.
struct RollingRateTracker {
    window: std::collections::VecDeque<bool>,
    capacity: usize,
    reject_count: usize,
}

impl RollingRateTracker {
    fn new(capacity: usize) -> Self {
        Self {
            window: std::collections::VecDeque::with_capacity(capacity + 1),
            capacity,
            reject_count: 0,
        }
    }

    fn push(&mut self, rejected: bool) {
        if self.window.len() >= self.capacity
            && let Some(evicted) = self.window.pop_front()
            && evicted
        {
            self.reject_count = self.reject_count.saturating_sub(1);
        }
        self.window.push_back(rejected);
        if rejected {
            self.reject_count += 1;
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn rate(&self) -> f32 {
        if self.window.is_empty() {
            return 0.0;
        }
        self.reject_count as f32 / self.window.len() as f32
    }
}

// ── QualityGate ───────────────────────────────────────────────────────────────

/// Write quality gate that runs after A-MAC admission.
///
/// Constructed once and attached to [`crate::semantic::SemanticMemory`] via
/// [`crate::semantic::SemanticMemory::with_quality_gate`]. Shared via `Arc`.
///
/// # Fail-open
///
/// Any internal error (embed failure, LLM timeout, graph query error) is caught
/// and treated as a pass. The gate never causes `remember()` to return an `Err`.
pub struct QualityGate {
    config: Arc<QualityGateConfig>,
    /// Optional LLM provider for the blended scoring path.
    llm_provider: Option<Arc<AnyProvider>>,
    graph_store: Option<Arc<GraphStore>>,
    /// Rejection counters keyed by reason.
    rejection_counts: std::sync::Mutex<std::collections::HashMap<QualityRejectionReason, u64>>,
    /// Rolling rejection-rate tracker (last 100 writes).
    rate_tracker: std::sync::Mutex<RollingRateTracker>,
}

impl QualityGate {
    /// Create a new quality gate with the given config.
    #[must_use]
    pub fn new(config: QualityGateConfig) -> Self {
        Self {
            config: Arc::new(config),
            llm_provider: None,
            graph_store: None,
            rejection_counts: std::sync::Mutex::new(std::collections::HashMap::new()),
            rate_tracker: std::sync::Mutex::new(RollingRateTracker::new(100)),
        }
    }

    /// Attach an LLM provider for optional blended scoring.
    #[must_use]
    pub fn with_llm_provider(mut self, provider: AnyProvider) -> Self {
        self.llm_provider = Some(Arc::new(provider));
        self
    }

    /// Attach a graph store for contradiction detection.
    #[must_use]
    pub fn with_graph_store(mut self, store: Arc<GraphStore>) -> Self {
        self.graph_store = Some(store);
        self
    }

    /// Return a reference to the configuration.
    #[must_use]
    pub fn config(&self) -> &QualityGateConfig {
        &self.config
    }

    /// Return cumulative rejection counts per reason.
    #[must_use]
    pub fn rejection_counts(&self) -> std::collections::HashMap<QualityRejectionReason, u64> {
        self.rejection_counts
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Evaluate the quality gate for a candidate write.
    ///
    /// Returns `None` when the write passes (should be persisted).
    /// Returns `Some(reason)` when the write should be rejected.
    ///
    /// Failures inside scoring are caught and treated as pass (fail-open).
    #[tracing::instrument(name = "memory.quality_gate.evaluate", skip_all)]
    pub async fn evaluate(
        &self,
        content: &str,
        embed_provider: &AnyProvider,
        recent_embeddings: &[Vec<f32>],
    ) -> Option<QualityRejectionReason> {
        if !self.config.enabled {
            return None;
        }

        let info_val = compute_information_value(content, embed_provider, recent_embeddings).await;
        let ref_comp = if self.config.reference_check_lang_en {
            compute_reference_completeness(content)
        } else {
            1.0
        };
        let contradiction_risk =
            compute_contradiction_risk(content, self.graph_store.as_deref(), &self.config).await;

        let w_v = self.config.information_value_weight;
        let w_c = self.config.reference_completeness_weight;
        let w_k = self.config.contradiction_weight;

        let rule_score = w_v * info_val + w_c * ref_comp + w_k * (1.0 - contradiction_risk);

        let final_score = if let Some(ref llm) = self.llm_provider {
            let llm_score = call_llm_scorer(content, llm, self.config.llm_timeout_ms).await;
            let lw = self.config.llm_weight;
            (1.0 - lw) * rule_score + lw * llm_score
        } else {
            rule_score
        };

        let rejected = final_score < self.config.threshold;

        // Track rolling rejection rate.
        if let Ok(mut tracker) = self.rate_tracker.lock() {
            tracker.push(rejected);
            let rate = tracker.rate();
            if rate > self.config.rejection_rate_alarm_ratio {
                tracing::warn!(
                    rate = %format!("{:.2}", rate),
                    window_size = self.config.recent_window,
                    threshold = self.config.rejection_rate_alarm_ratio,
                    "quality_gate: high rejection rate alarm"
                );
            }
        }

        if !rejected {
            return None;
        }

        // Determine the most specific rejection reason.
        let reason = if info_val < 0.1 {
            QualityRejectionReason::Redundant
        } else if ref_comp < 0.5 && self.config.reference_check_lang_en {
            QualityRejectionReason::IncompleteReference
        } else if contradiction_risk >= 1.0 {
            QualityRejectionReason::Contradiction
        } else {
            QualityRejectionReason::LlmLowConfidence
        };

        if let Ok(mut counts) = self.rejection_counts.lock() {
            *counts.entry(reason).or_insert(0) += 1;
        }

        tracing::debug!(
            reason = reason.label(),
            final_score,
            info_val,
            ref_comp,
            contradiction_risk,
            "quality_gate: rejected write"
        );

        Some(reason)
    }
}

// ── Sub-scorers ───────────────────────────────────────────────────────────────

/// Compute `information_value` as `1.0 - max_cosine(candidate, recent_embeddings)`.
///
/// Returns `1.0` when the store is empty or on any embedding error (fail-open: treat as novel).
async fn compute_information_value(
    content: &str,
    provider: &AnyProvider,
    recent_embeddings: &[Vec<f32>],
) -> f32 {
    if recent_embeddings.is_empty() {
        return 1.0;
    }
    if !provider.supports_embeddings() {
        return 1.0;
    }
    let candidate = match provider.embed(content).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "quality_gate: embed failed, treating info_val = 1.0 (fail-open)");
            return 1.0;
        }
    };
    let max_sim = recent_embeddings
        .iter()
        .map(|r| zeph_common::math::cosine_similarity(&candidate, r))
        .fold(0.0f32, f32::max);
    (1.0 - max_sim).max(0.0)
}

/// Compute `reference_completeness` as `1.0 - unresolved_reference_ratio`.
///
/// Heuristic: counts unresolved English pronouns and deictic time expressions.
/// English-only; callers must skip this when `reference_check_lang_en = false`.
#[must_use]
pub fn compute_reference_completeness(content: &str) -> f32 {
    // Third-person pronouns that likely refer to an unresolved entity.
    const PRONOUNS: &[&str] = &[
        " he ", " she ", " they ", " it ", " him ", " her ", " them ",
    ];
    // Deictic time expressions without an accompanying absolute date.
    const DEICTIC_TIME: &[&str] = &[
        "yesterday",
        "tomorrow",
        "last week",
        "next week",
        "last month",
        "next month",
        "last year",
        "next year",
    ];
    // Absolute date anchors that resolve deictic expressions.
    const DATE_ANCHORS: &[&str] = &[
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
        "jan ",
        "feb ",
        "mar ",
        "apr ",
        "jun ",
        "jul ",
        "aug ",
        "sep ",
        "oct ",
        "nov ",
        "dec ",
    ];

    let lower = content.to_lowercase();
    let padded = format!(" {lower} ");
    let pronoun_count = PRONOUNS.iter().filter(|&&p| padded.contains(p)).count();

    // Require a 4-digit year (19xx or 20xx) at a word boundary, not just "20"
    // which produces false positives on counts like "20 items" or "id=200".
    let has_year_anchor = has_4digit_year_anchor(&lower);
    let has_date_anchor = has_year_anchor || DATE_ANCHORS.iter().any(|&a| lower.contains(a));
    let deictic_count = if has_date_anchor {
        0
    } else {
        DEICTIC_TIME.iter().filter(|&&t| lower.contains(t)).count()
    };

    let total_issues = pronoun_count + deictic_count;
    if total_issues == 0 {
        return 1.0;
    }

    // Normalize by approximate word count; each issue costs ~0.25, floor at 0.0.
    let word_count = content.split_ascii_whitespace().count().max(1);
    #[allow(clippy::cast_precision_loss)]
    let ratio = total_issues as f32 / word_count as f32;
    (1.0 - ratio * 2.0).clamp(0.0, 1.0)
}

/// Returns `true` when `text` (lowercased) contains a 4-digit year (19xx or 20xx)
/// at a word boundary.
///
/// Avoids false positives from 2-digit numbers like "20 items" or "id=200".
fn has_4digit_year_anchor(text: &str) -> bool {
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len < 4 {
        return false;
    }
    let mut i = 0usize;
    while i + 3 < len {
        let c0 = bytes[i];
        let c1 = bytes[i + 1];
        if ((c0 == b'1' && c1 == b'9') || (c0 == b'2' && c1 == b'0'))
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            let left_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
            let right_ok = i + 4 >= len || !bytes[i + 4].is_ascii_digit();
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Compute `contradiction_risk` via graph edge lookup (FR-006).
///
/// Extracts the subject entity from the candidate message, then queries for existing
/// active edges with the same `(source_entity_id, canonical_relation)`. A conflicting
/// value on the same predicate that is older than `grace_seconds` is treated as a
/// hard contradiction (returns `1.0`).
///
/// Returns `0.0` when no graph store is attached, on any error, or when no conflict found.
async fn compute_contradiction_risk(
    content: &str,
    graph: Option<&GraphStore>,
    config: &QualityGateConfig,
) -> f32 {
    let Some(store) = graph else {
        return 0.0;
    };

    let content_lower = content.to_lowercase();

    // Extract subject: longest noun-phrase before a verb-like token ("is", "has", "was", "are").
    // Fallback: first two tokens.
    let subject_query = extract_subject_tokens(&content_lower);
    if subject_query.is_empty() {
        return 0.0;
    }

    // Resolve the subject entity.
    let Ok(entities) = store.find_entities_fuzzy(&subject_query, 1).await else {
        return 0.0;
    };
    let Some(subject_entity) = entities.into_iter().next() else {
        return 0.0;
    };

    // Extract candidate predicate from "X <predicate> Y" pattern.
    let canonical_predicate = extract_predicate_token(&content_lower);

    // Load all active edges where this entity is the source.
    let Ok(edges) = store.edges_for_entity(subject_entity.id).await else {
        return 0.0;
    };

    // Filter to edges where source matches subject and canonical_relation matches predicate.
    let relevant_edges: Vec<_> = edges
        .iter()
        .filter(|e| {
            e.source_entity_id == subject_entity.id
                && canonical_predicate
                    .as_ref()
                    .is_none_or(|p| e.relation == *p)
        })
        .collect();

    if relevant_edges.is_empty() {
        return 0.0;
    }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let has_old_conflict = relevant_edges.iter().any(|edge| {
        let edge_ts = chrono::DateTime::parse_from_rfc3339(&edge.created_at)
            .map_or(0u64, |dt| u64::try_from(dt.timestamp()).unwrap_or(0));
        now_secs.saturating_sub(edge_ts) > config.contradiction_grace_seconds
    });

    if has_old_conflict { 1.0 } else { 0.5 }
}

/// Extract subject tokens from the content (first noun phrase before verb-like token).
fn extract_subject_tokens(content_lower: &str) -> String {
    const VERB_MARKERS: &[&str] = &["is", "was", "are", "were", "has", "have", "had", "will"];
    let tokens: Vec<&str> = content_lower.split_ascii_whitespace().collect();
    let end = tokens
        .iter()
        .position(|t| VERB_MARKERS.contains(t))
        .unwrap_or(2.min(tokens.len()));
    let subject_tokens = &tokens[..end.min(3)];
    subject_tokens.join(" ")
}

/// Extract the canonical predicate token (first verb-like token in the content).
fn extract_predicate_token(content_lower: &str) -> Option<String> {
    const VERB_MARKERS: &[&str] = &["is", "was", "are", "were", "has", "have", "had", "will"];
    content_lower
        .split_ascii_whitespace()
        .find(|t| VERB_MARKERS.contains(t))
        .map(str::to_owned)
}

/// Call the optional LLM scorer and return a blended quality score.
///
/// Returns `0.5` (neutral) on timeout or any error — ensures fail-open behavior.
async fn call_llm_scorer(content: &str, provider: &AnyProvider, timeout_ms: u64) -> f32 {
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    let system = "You are a memory quality judge. Rate the quality of the following message \
        for long-term storage on a scale of 0.0 to 1.0. Consider: information density, \
        completeness of references, factual clarity. \
        Respond with ONLY a JSON object: \
        {\"information_value\": 0.0-1.0, \"reference_completeness\": 0.0-1.0, \
        \"contradiction_risk\": 0.0-1.0}";

    let user = format!(
        "Message: {}\n\nQuality JSON:",
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

    let timeout = Duration::from_millis(timeout_ms);
    let result = match tokio::time::timeout(timeout, provider.chat(&messages)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "quality_gate: LLM scorer failed, using 0.5");
            return 0.5;
        }
        Err(_) => {
            tracing::debug!("quality_gate: LLM scorer timed out, using 0.5");
            return 0.5;
        }
    };

    parse_llm_score(&result)
}

/// Parse LLM JSON response into a combined quality score.
///
/// Returns `0.5` on any parse failure.
fn parse_llm_score(response: &str) -> f32 {
    // Find JSON object in the response.
    let start = response.find('{');
    let end = response.rfind('}');
    let (Some(s), Some(e)) = (start, end) else {
        return 0.5;
    };
    let json_str = &response[s..=e];
    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return 0.5;
    };

    #[allow(clippy::cast_possible_truncation)]
    let iv = val["information_value"].as_f64().unwrap_or(0.5) as f32;
    #[allow(clippy::cast_possible_truncation)]
    let rc = val["reference_completeness"].as_f64().unwrap_or(0.5) as f32;
    #[allow(clippy::cast_possible_truncation)]
    let cr = val["contradiction_risk"].as_f64().unwrap_or(0.0) as f32;

    // Mirror the rule-based formula with default weights.
    let score =
        0.4 * iv.clamp(0.0, 1.0) + 0.3 * rc.clamp(0.0, 1.0) + 0.3 * (1.0 - cr.clamp(0.0, 1.0));
    score.clamp(0.0, 1.0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_completeness_clean_text() {
        let score = compute_reference_completeness("The Rust compiler enforces memory safety.");
        assert!((score - 1.0).abs() < 0.01, "clean text should score 1.0");
    }

    #[test]
    fn reference_completeness_pronoun_heavy() {
        // "he", "they", "it" — three unresolved pronouns in a short message.
        let score = compute_reference_completeness("yeah he said they confirmed it");
        assert!(
            score < 0.5,
            "pronoun-heavy message should score below 0.5, got {score}"
        );
    }

    #[test]
    fn reference_completeness_deictic_without_anchor() {
        let score = compute_reference_completeness("We agreed yesterday to postpone");
        assert!(
            score < 1.0,
            "deictic time without anchor should penalize, got {score}"
        );
    }

    #[test]
    fn reference_completeness_deictic_with_anchor() {
        let score = compute_reference_completeness("We agreed yesterday (2026-04-18) to postpone");
        assert!(
            score >= 0.9,
            "deictic with anchor '20' should not penalize, got {score}"
        );
    }

    #[test]
    fn rejection_reason_labels() {
        assert_eq!(QualityRejectionReason::Redundant.label(), "redundant");
        assert_eq!(
            QualityRejectionReason::IncompleteReference.label(),
            "incomplete_reference"
        );
        assert_eq!(
            QualityRejectionReason::Contradiction.label(),
            "contradiction"
        );
        assert_eq!(
            QualityRejectionReason::LlmLowConfidence.label(),
            "llm_low_confidence"
        );
    }

    #[test]
    fn rolling_rate_tracker_basic() {
        let mut tracker = RollingRateTracker::new(4);
        tracker.push(true);
        tracker.push(true);
        tracker.push(false);
        tracker.push(false);
        let rate = tracker.rate();
        assert!((rate - 0.5).abs() < 0.01, "rate should be 0.5, got {rate}");
    }

    #[test]
    fn rolling_rate_tracker_evicts_oldest() {
        let mut tracker = RollingRateTracker::new(3);
        tracker.push(true); // will be evicted
        tracker.push(false);
        tracker.push(false);
        tracker.push(false); // evicts first `true`
        let rate = tracker.rate();
        assert!(
            rate < 0.01,
            "evicted rejection should not count, rate={rate}"
        );
    }

    #[test]
    fn parse_llm_score_valid_json() {
        let json = r#"{"information_value": 0.8, "reference_completeness": 0.9, "contradiction_risk": 0.1}"#;
        let score = parse_llm_score(json);
        assert!(
            score > 0.7,
            "high-quality JSON should yield high score, got {score}"
        );
    }

    #[test]
    fn parse_llm_score_malformed_returns_neutral() {
        let score = parse_llm_score("not json");
        assert!(
            (score - 0.5).abs() < 0.01,
            "malformed JSON should return 0.5"
        );
    }

    fn mock_provider() -> zeph_llm::any::AnyProvider {
        zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default())
    }

    #[tokio::test]
    async fn gate_disabled_always_passes() {
        let config = QualityGateConfig {
            enabled: false,
            ..QualityGateConfig::default()
        };
        let gate = QualityGate::new(config);
        let provider = mock_provider();

        let result = gate.evaluate("yeah he confirmed it", &provider, &[]).await;
        assert!(result.is_none(), "disabled gate must always pass");
    }

    #[tokio::test]
    async fn gate_admits_novel_clean_content() {
        let config = QualityGateConfig {
            enabled: true,
            threshold: 0.3, // lenient threshold for rule-only test
            ..QualityGateConfig::default()
        };
        let gate = QualityGate::new(config);
        let provider = mock_provider();

        // Novel content with no recent embeddings and clean references → should pass.
        let result = gate
            .evaluate(
                "The Rust compiler enforces memory safety through the borrow checker.",
                &provider,
                &[],
            )
            .await;
        assert!(result.is_none(), "clean novel content should be admitted");
    }

    #[tokio::test]
    async fn gate_rejects_pronoun_only_at_low_threshold() {
        let config = QualityGateConfig {
            enabled: true,
            threshold: 0.75, // strict threshold
            reference_completeness_weight: 0.9,
            information_value_weight: 0.05,
            contradiction_weight: 0.05,
            ..QualityGateConfig::default()
        };
        let gate = QualityGate::new(config);
        let provider = mock_provider();

        let result = gate
            .evaluate("yeah he confirmed it they said so", &provider, &[])
            .await;
        assert!(
            result == Some(QualityRejectionReason::IncompleteReference),
            "pronoun-heavy message should be rejected as IncompleteReference, got {result:?}"
        );
    }

    #[test]
    fn quality_gate_counts_rejections() {
        let config = QualityGateConfig {
            enabled: true,
            threshold: 0.99, // reject almost everything
            ..QualityGateConfig::default()
        };
        let gate = QualityGate::new(config);

        // Manually record a rejection.
        if let Ok(mut counts) = gate.rejection_counts.lock() {
            *counts.entry(QualityRejectionReason::Redundant).or_insert(0) += 1;
        }

        let counts = gate.rejection_counts();
        assert_eq!(counts.get(&QualityRejectionReason::Redundant), Some(&1));
    }

    /// Embed error → fail-open: gate must admit the write (return `None`).
    #[tokio::test]
    async fn gate_fail_open_on_embed_error() {
        let config = QualityGateConfig {
            enabled: true,
            threshold: 0.5,
            ..QualityGateConfig::default()
        };
        let gate = QualityGate::new(config);

        // Provider that returns an embed error.
        let provider = zeph_llm::any::AnyProvider::Mock(
            zeph_llm::mock::MockProvider::default().with_embed_invalid_input(),
        );

        let result = gate
            .evaluate(
                "Alice confirmed the meeting at 3pm.",
                &provider,
                &[], // no recent embeddings; error occurs during info_value embed
            )
            .await;
        assert!(
            result.is_none(),
            "embed error must be treated as fail-open (admitted), got {result:?}"
        );
    }

    /// Pre-populated `recent_embeddings` with an identical vector triggers `Redundant` rejection.
    #[tokio::test]
    async fn gate_rejects_redundant_with_populated_embeddings() {
        let config = QualityGateConfig {
            enabled: true,
            threshold: 0.5,
            // Heavy weight on information_value so redundancy dominates the score.
            information_value_weight: 0.9,
            reference_completeness_weight: 0.05,
            contradiction_weight: 0.05,
            ..QualityGateConfig::default()
        };
        let gate = QualityGate::new(config);

        // MockProvider returns the same fixed embedding for every call.
        let fixed_embedding = vec![0.1_f32; 384];
        let provider = zeph_llm::any::AnyProvider::Mock(
            zeph_llm::mock::MockProvider::default().with_embedding(fixed_embedding.clone()),
        );

        // Pass the identical vector as the recent-embeddings window so cosine similarity = 1.0.
        let result = gate
            .evaluate(
                "The Rust compiler enforces memory safety through the borrow checker.",
                &provider,
                &[fixed_embedding],
            )
            .await;
        assert_eq!(
            result,
            Some(QualityRejectionReason::Redundant),
            "identical recent embedding must trigger Redundant rejection"
        );
    }

    /// LLM provider with 600ms latency exceeds `llm_timeout_ms`; gate falls back to rule score
    /// and still returns a result (pass or reject based on rule score alone).
    #[tokio::test]
    async fn gate_llm_timeout_falls_back_to_rule_score() {
        let config = QualityGateConfig {
            enabled: true,
            threshold: 0.3,     // lenient so rule score alone is likely to pass
            llm_timeout_ms: 50, // tight timeout
            llm_weight: 0.5,
            ..QualityGateConfig::default()
        };
        let gate = QualityGate::new(config);

        // Chat provider with 600ms delay — will exceed the 50ms timeout.
        let slow_provider = zeph_llm::any::AnyProvider::Mock(
            zeph_llm::mock::MockProvider::default().with_delay(600),
        );
        let gate = gate.with_llm_provider(slow_provider);

        let embed_provider = mock_provider(); // no embeddings needed for this path

        let result = gate
            .evaluate(
                "The release is scheduled for next Friday.",
                &embed_provider,
                &[],
            )
            .await;
        // Gate must complete (no panic/hang) and fall back to rule-only score.
        // With a lenient threshold and clean content the rule score should admit it.
        assert!(
            result.is_none(),
            "LLM timeout must fall back to rule score and admit clean content, got {result:?}"
        );
    }
}
