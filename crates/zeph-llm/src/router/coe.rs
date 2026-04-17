// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Collaborative Entropy (`CoE`) router module.
//!
//! Detects uncertain primary responses via two orthogonal signals:
//! - **Intra-entropy** — mean negative log-probability from a single model's logprobs.
//! - **Inter-divergence** — normalised `(1-cosine)/2` between primary and secondary embeddings.
//!
//! When either signal crosses its threshold, `CoE` escalates to the secondary provider.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use zeph_common::math::cosine_similarity;

use crate::any::AnyProvider;
use crate::error::LlmError;
use crate::provider::{LlmProvider, Message};

/// Configuration for the `CoE` subsystem (mirrors `[llm.coe]` in TOML).
#[derive(Debug, Clone)]
pub struct CoeConfig {
    /// Mean negative log-prob threshold; responses above this trigger intra escalation.
    pub intra_threshold: f64,
    /// Divergence threshold in `[0.0, 1.0]`; responses above this trigger inter escalation.
    pub inter_threshold: f64,
    /// Baseline rate at which secondary is called even when intra is low.
    pub shadow_sample_rate: f64,
}

impl Default for CoeConfig {
    fn default() -> Self {
        Self {
            intra_threshold: 0.8,
            inter_threshold: 0.20,
            shadow_sample_rate: 0.1,
        }
    }
}

/// Session-level `CoE` statistics (atomic counters, not persisted).
#[derive(Debug, Default)]
pub struct CoeMetrics {
    /// Turns where the primary was kept (no escalation).
    pub kept_primary: AtomicU64,
    /// Escalations triggered by intra-entropy exceeding threshold.
    pub intra_escalations: AtomicU64,
    /// Escalations triggered by inter-divergence exceeding threshold.
    pub inter_escalations: AtomicU64,
    /// Turns where secondary call failed — primary was returned as fallback.
    pub embed_failures: AtomicU64,
}

/// Runtime `CoE` state bundled into `RouterProvider`.
#[derive(Debug, Clone)]
pub struct CoeRouter {
    pub config: CoeConfig,
    /// The provider used as the secondary/escalation target.
    pub secondary: AnyProvider,
    /// Provider used for computing inter-divergence embeddings.
    pub embed: AnyProvider,
    pub metrics: Arc<CoeMetrics>,
}

/// Minimum response length (bytes) for inter-divergence to be meaningful.
const MIN_INTER_LEN: usize = 50;

/// Compute inter-divergence in `[0.0, 1.0]` between two response texts.
///
/// Returns `None` when either text is shorter than the minimum guard or embedding fails.
/// `0.0` = identical, `0.5` = orthogonal, `1.0` = opposite.
pub async fn inter_divergence(primary: &str, secondary: &str, embed: &AnyProvider) -> Option<f32> {
    use crate::provider::LlmProvider;

    if primary.len().min(secondary.len()) < MIN_INTER_LEN {
        return None;
    }
    let (a, b) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::try_join!(embed.embed(primary), embed.embed(secondary))
    })
    .await
    .ok()
    .and_then(Result::ok)?;
    let cos = cosine_similarity(&a, &b);
    Some(((1.0 - cos) * 0.5).clamp(0.0, 1.0))
}

/// `CoE` decision result for a single turn.
pub enum CoeDecision {
    /// Keep the primary response.
    KeepPrimary,
    /// Escalate to secondary due to intra-entropy.
    EscalateIntra,
    /// Escalate to secondary due to inter-divergence.
    EscalateInter,
}

/// Determine whether to shadow-call the secondary based on intra-entropy.
///
/// Returns `true` if a shadow call should be made.
#[must_use]
pub fn should_shadow(entropy: Option<f64>, config: &CoeConfig) -> bool {
    let uncertain = entropy.is_some_and(|e| e >= config.intra_threshold);
    if uncertain {
        return true;
    }
    // Borderline intra trigger: entropy in [50% threshold, threshold) still shadows
    let borderline = entropy.is_none_or(|e| e >= config.intra_threshold * 0.5);
    borderline && rand::random::<f64>() < config.shadow_sample_rate
}

/// Apply the `CoE` decision logic after obtaining both primary and secondary texts.
///
/// Returns the [`CoeDecision`] for this turn.
#[must_use]
pub fn decide(entropy: Option<f64>, divergence: Option<f32>, config: &CoeConfig) -> CoeDecision {
    if entropy.is_some_and(|e| e >= config.intra_threshold) {
        return CoeDecision::EscalateIntra;
    }
    if f64::from(divergence.unwrap_or(0.0)) >= config.inter_threshold {
        return CoeDecision::EscalateInter;
    }
    CoeDecision::KeepPrimary
}

/// Run the full `CoE` pipeline for a single turn.
///
/// Takes the already-obtained primary response to avoid a redundant LLM call.
/// Optionally shadows via secondary and returns the final response text.
/// Also returns the primary provider name for Thompson updates.
///
/// Returns `(response_text, primary_name, decision)`.
///
/// # Errors
///
/// Secondary/embed failures are swallowed and cause fallback to the primary response (COE-02).
pub async fn run_coe(
    coe: &CoeRouter,
    primary_name: String,
    primary_text: String,
    primary_extras: crate::provider::ChatExtras,
    messages: &[Message],
) -> Result<(String, String, CoeDecision), LlmError> {
    let entropy = primary_extras.entropy;

    if !should_shadow(entropy, &coe.config) {
        coe.metrics.kept_primary.fetch_add(1, Ordering::Relaxed);
        return Ok((primary_text, primary_name, CoeDecision::KeepPrimary));
    }

    // Shadow call to secondary.
    let secondary_result = coe.secondary.chat_with_extras(messages).await;
    let secondary_text = match secondary_result {
        Ok((t, _)) => t,
        Err(e) => {
            tracing::warn!(error = %e, "coe: secondary call failed, keeping primary");
            coe.metrics.embed_failures.fetch_add(1, Ordering::Relaxed);
            return Ok((primary_text, primary_name, CoeDecision::KeepPrimary));
        }
    };

    // Compute inter-divergence.
    let divergence = inter_divergence(&primary_text, &secondary_text, &coe.embed).await;

    let decision = decide(entropy, divergence, &coe.config);

    match decision {
        CoeDecision::EscalateIntra => {
            coe.metrics
                .intra_escalations
                .fetch_add(1, Ordering::Relaxed);
            Ok((secondary_text, primary_name, CoeDecision::EscalateIntra))
        }
        CoeDecision::EscalateInter => {
            coe.metrics
                .inter_escalations
                .fetch_add(1, Ordering::Relaxed);
            Ok((secondary_text, primary_name, CoeDecision::EscalateInter))
        }
        CoeDecision::KeepPrimary => {
            coe.metrics.kept_primary.fetch_add(1, Ordering::Relaxed);
            Ok((primary_text, primary_name, CoeDecision::KeepPrimary))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_shadow_returns_true_when_above_threshold() {
        let config = CoeConfig {
            intra_threshold: 0.8,
            shadow_sample_rate: 0.0, // disable random gating
            ..CoeConfig::default()
        };
        assert!(should_shadow(Some(0.9), &config));
        assert!(should_shadow(Some(0.8), &config));
    }

    #[test]
    fn should_shadow_returns_false_below_borderline() {
        let config = CoeConfig {
            intra_threshold: 0.8,
            shadow_sample_rate: 0.0,
            ..CoeConfig::default()
        };
        // Below 50% of threshold = 0.4
        assert!(!should_shadow(Some(0.3), &config));
    }

    #[test]
    fn decide_escalates_intra() {
        let config = CoeConfig::default();
        assert!(matches!(
            decide(Some(1.0), None, &config),
            CoeDecision::EscalateIntra
        ));
    }

    #[test]
    fn decide_escalates_inter() {
        let config = CoeConfig::default();
        assert!(matches!(
            decide(None, Some(0.5), &config),
            CoeDecision::EscalateInter
        ));
    }

    #[test]
    fn decide_keeps_primary_when_below_thresholds() {
        let config = CoeConfig::default();
        assert!(matches!(
            decide(Some(0.1), Some(0.05), &config),
            CoeDecision::KeepPrimary
        ));
    }

    fn make_coe_router(
        secondary: crate::any::AnyProvider,
        embed: crate::any::AnyProvider,
    ) -> CoeRouter {
        CoeRouter {
            config: CoeConfig {
                intra_threshold: 0.8,
                inter_threshold: 0.20,
                shadow_sample_rate: 0.0, // disable random shadow unless explicitly set
            },
            secondary,
            embed,
            metrics: Arc::new(CoeMetrics::default()),
        }
    }

    #[tokio::test]
    async fn run_coe_keeps_primary_when_shadow_disabled() {
        use crate::any::AnyProvider;
        use crate::mock::MockProvider;
        use crate::provider::ChatExtras;

        let secondary = MockProvider::with_responses(vec!["secondary response".into()]);
        let mut embed = MockProvider::default();
        embed.supports_embeddings = true;
        let coe = make_coe_router(AnyProvider::Mock(secondary), AnyProvider::Mock(embed));

        // Low entropy → should_shadow returns false with shadow_sample_rate=0.0
        let (text, _name, decision) = run_coe(
            &coe,
            "primary".into(),
            "primary response".into(),
            ChatExtras { entropy: Some(0.1) },
            &[],
        )
        .await
        .unwrap();

        assert_eq!(text, "primary response");
        assert!(matches!(decision, CoeDecision::KeepPrimary));
        assert_eq!(coe.metrics.kept_primary.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn run_coe_fallback_on_secondary_failure() {
        use crate::any::AnyProvider;
        use crate::mock::MockProvider;
        use crate::provider::ChatExtras;

        let mut secondary = MockProvider::default();
        secondary.fail_chat = true;
        let mut embed = MockProvider::default();
        embed.supports_embeddings = true;
        let coe = make_coe_router(AnyProvider::Mock(secondary), AnyProvider::Mock(embed));

        // High entropy triggers shadow; secondary fails → fallback to primary.
        let (text, _name, decision) = run_coe(
            &coe,
            "primary".into(),
            "primary response".into(),
            ChatExtras { entropy: Some(1.0) },
            &[],
        )
        .await
        .unwrap();

        assert_eq!(text, "primary response");
        assert!(matches!(decision, CoeDecision::KeepPrimary));
        assert_eq!(coe.metrics.embed_failures.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn run_coe_escalates_intra_when_entropy_high() {
        use crate::any::AnyProvider;
        use crate::mock::MockProvider;
        use crate::provider::ChatExtras;

        let secondary = MockProvider::with_responses(vec!["secondary response".into()]);
        let mut embed = MockProvider::default();
        embed.supports_embeddings = true;
        let coe = make_coe_router(AnyProvider::Mock(secondary), AnyProvider::Mock(embed));

        let (text, _name, decision) = run_coe(
            &coe,
            "primary".into(),
            "primary response text long enough to matter".into(),
            ChatExtras { entropy: Some(1.0) }, // above intra_threshold=0.8
            &[],
        )
        .await
        .unwrap();

        assert_eq!(text, "secondary response");
        assert!(matches!(decision, CoeDecision::EscalateIntra));
        assert_eq!(coe.metrics.intra_escalations.load(Ordering::Relaxed), 1);
    }
}
