// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Collaborative Entropy (`CoE`) router module.
//!
//! Detects uncertain primary responses via two orthogonal signals:
//! - **Intra-entropy** — mean negative log-probability from a single model's logprobs.
//! - **Inter-divergence** — normalised `(1-cosine)/2` between primary and secondary embeddings.
//!
//! When either signal crosses its threshold, `CoE` escalates to the secondary provider.
//!
//! # Scope
//!
//! `CoE` wraps the router's `chat()` and `chat_with_tools()`. Streaming is excluded by design:
//! delivered chunks cannot be replaced by a shadow answer. A tool-call primary answer is kept
//! without a shadow call; only a text answer can escalate. The shadow stage (secondary call and
//! inter-divergence) is bounded by [`CoeConfig::shadow_budget`], so a slow secondary never
//! costs the primary answer.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use zeph_common::math::cosine_similarity;

use crate::error::LlmError;
use crate::provider::{ChatExtras, ChatResponse, ResponseUsage};
use crate::provider_dyn::LlmProviderDyn;

/// Upper bound of the shadow stage, regardless of the configured LLM timeout.
const MAX_SHADOW_BUDGET: Duration = Duration::from_secs(30);

/// Shadow-stage budget for a given LLM timeout: `min(30 s, llm_timeout / 4)`.
///
/// A primary answer that completes within three quarters of `llm_timeout` is therefore never
/// lost to the `CoE` stage.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use zeph_llm::router::coe::shadow_budget_for;
///
/// assert_eq!(shadow_budget_for(Duration::from_secs(120)), Duration::from_secs(30));
/// assert_eq!(shadow_budget_for(Duration::from_secs(20)), Duration::from_secs(5));
/// ```
#[must_use]
pub fn shadow_budget_for(llm_timeout: Duration) -> Duration {
    MAX_SHADOW_BUDGET.min(llm_timeout / 4)
}

/// Configuration for the `CoE` subsystem (mirrors `[llm.coe]` in TOML).
#[derive(Debug, Clone)]
pub struct CoeConfig {
    /// Mean negative log-prob threshold; responses above this trigger intra escalation.
    pub intra_threshold: f64,
    /// Divergence threshold in `[0.0, 1.0]`; responses above this trigger inter escalation.
    pub inter_threshold: f64,
    /// Baseline rate at which secondary is called even when intra is low.
    pub shadow_sample_rate: f64,
    /// Time allowed for the shadow stage; on expiry the primary is kept.
    ///
    /// Runtime-derived from `timeouts.llm_seconds` (see [`shadow_budget_for`]), not a TOML key.
    /// A zero budget disables the shadow stage.
    pub shadow_budget: Duration,
}

impl Default for CoeConfig {
    fn default() -> Self {
        Self {
            intra_threshold: 0.8,
            inter_threshold: 0.20,
            shadow_sample_rate: 0.1,
            shadow_budget: MAX_SHADOW_BUDGET,
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
#[derive(Debug)]
pub struct CoeRouter {
    pub config: CoeConfig,
    /// The provider used as the secondary/escalation target.
    pub secondary: Arc<dyn LlmProviderDyn>,
    /// Provider used for computing inter-divergence embeddings.
    pub embed: Arc<dyn LlmProviderDyn>,
    pub metrics: Arc<CoeMetrics>,
}

/// Minimum response length (bytes) for inter-divergence to be meaningful.
const MIN_INTER_LEN: usize = 50;

/// Compute inter-divergence in `[0.0, 1.0]` between two response texts.
///
/// Returns `None` when either text is shorter than the minimum guard or embedding fails.
/// `0.0` = identical, `0.5` = orthogonal, `1.0` = opposite.
#[tracing::instrument(name = "llm.router.coe.inter_divergence", skip_all)]
pub async fn inter_divergence(
    primary: &str,
    secondary: &str,
    embed: &dyn LlmProviderDyn,
) -> Option<f32> {
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
#[non_exhaustive]
#[derive(Debug)]
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

/// A response shape that [`run_coe`] can check and, after an escalation, replace.
pub(crate) trait CoeResponse {
    /// Text the entropy and divergence signals apply to; `None` for tool-call answers, which
    /// are kept without a shadow call.
    fn coe_text(&self) -> Option<&str>;

    /// Carry per-request metadata of the discarded `primary` over to this replacement.
    fn inherit_request_metadata(&mut self, primary: &mut Self);
}

impl CoeResponse for String {
    fn coe_text(&self) -> Option<&str> {
        Some(self)
    }

    fn inherit_request_metadata(&mut self, _primary: &mut Self) {}
}

impl CoeResponse for ChatResponse {
    fn coe_text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } => Some(text),
            Self::ToolUse { .. } => None,
        }
    }

    /// The primary's request was the one sent, so its thinking-replay report and its
    /// compaction summary describe the request and must survive the replacement.
    fn inherit_request_metadata(&mut self, primary: &mut Self) {
        self.set_replay(primary.replay().merge(self.replay()));
        if let Some(summary) = primary.take_compaction() {
            self.attach_compaction(summary);
        }
    }
}

/// Result of one [`run_coe`] pass.
#[derive(Debug)]
pub(crate) struct CoeOutcome<R> {
    /// Final response (the primary's, or the secondary's after an escalation).
    pub response: R,
    /// Primary provider name, for Thompson updates.
    pub primary_name: String,
    /// Decision taken for this turn.
    pub decision: CoeDecision,
    /// Usage of the shadow call to the secondary, when one ran (any decision).
    ///
    /// The router folds it into the served usage as `auxiliary`, so it is billed but never
    /// counted toward context size.
    pub shadow_usage: Option<ResponseUsage>,
}

impl<R> CoeOutcome<R> {
    fn kept(response: R, primary_name: String, shadow_usage: Option<ResponseUsage>) -> Self {
        Self {
            response,
            primary_name,
            decision: CoeDecision::KeepPrimary,
            shadow_usage,
        }
    }
}

/// Run the full `CoE` pipeline for a single turn.
///
/// Takes the already-obtained primary response to avoid a redundant LLM call, and a lazy
/// `secondary` future that is polled only when a shadow call is warranted. The shadow stage
/// runs under [`CoeConfig::shadow_budget`]; a secondary error or a timeout keeps the primary
/// and counts toward `embed_failures` (COE-02). A tool-call primary, or a zero budget, returns
/// the primary immediately.
#[tracing::instrument(name = "llm.router.coe.run_coe", skip_all)]
pub(crate) async fn run_coe<R, F>(
    coe: &CoeRouter,
    primary_name: String,
    mut primary: R,
    primary_extras: ChatExtras,
    secondary: F,
) -> CoeOutcome<R>
where
    R: CoeResponse,
    F: Future<Output = Result<R, LlmError>>,
{
    if primary.coe_text().is_none() {
        return CoeOutcome::kept(primary, primary_name, None);
    }
    let entropy = primary_extras.entropy;

    if coe.config.shadow_budget.is_zero() || !should_shadow(entropy, &coe.config) {
        coe.metrics.kept_primary.fetch_add(1, Ordering::Relaxed);
        return CoeOutcome::kept(primary, primary_name, None);
    }

    let shadow = tokio::time::timeout(coe.config.shadow_budget, async {
        let secondary = secondary.await?;
        let usage = coe.secondary.last_response_usage();
        let divergence = match (primary.coe_text(), secondary.coe_text()) {
            (Some(p), Some(s)) => inter_divergence(p, s, &*coe.embed).await,
            _ => None,
        };
        Ok::<_, LlmError>((secondary, usage, divergence))
    })
    .await;
    let (mut secondary, shadow_usage, divergence) = match shadow {
        Ok(Ok(done)) => done,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "coe: secondary call failed, keeping primary");
            coe.metrics.embed_failures.fetch_add(1, Ordering::Relaxed);
            return CoeOutcome::kept(primary, primary_name, None);
        }
        Err(_) => {
            tracing::warn!(
                budget_ms = u64::try_from(coe.config.shadow_budget.as_millis()).unwrap_or(u64::MAX),
                "coe: shadow stage timed out, keeping primary"
            );
            coe.metrics.embed_failures.fetch_add(1, Ordering::Relaxed);
            return CoeOutcome::kept(primary, primary_name, None);
        }
    };

    let decision = decide(entropy, divergence, &coe.config);
    let response = match decision {
        CoeDecision::EscalateIntra | CoeDecision::EscalateInter => {
            let counter = if matches!(decision, CoeDecision::EscalateIntra) {
                &coe.metrics.intra_escalations
            } else {
                &coe.metrics.inter_escalations
            };
            counter.fetch_add(1, Ordering::Relaxed);
            secondary.inherit_request_metadata(&mut primary);
            secondary
        }
        CoeDecision::KeepPrimary => {
            coe.metrics.kept_primary.fetch_add(1, Ordering::Relaxed);
            primary
        }
    };
    CoeOutcome {
        response,
        primary_name,
        decision,
        shadow_usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

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
        assert_matches!(decide(Some(1.0), None, &config), CoeDecision::EscalateIntra);
    }

    #[test]
    fn decide_escalates_inter() {
        let config = CoeConfig::default();
        assert_matches!(decide(None, Some(0.5), &config), CoeDecision::EscalateInter);
    }

    #[test]
    fn decide_keeps_primary_when_below_thresholds() {
        let config = CoeConfig::default();
        assert_matches!(
            decide(Some(0.1), Some(0.05), &config),
            CoeDecision::KeepPrimary
        );
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
                shadow_budget: Duration::from_secs(30),
            },
            secondary: Arc::new(secondary) as Arc<dyn LlmProviderDyn>,
            embed: Arc::new(embed) as Arc<dyn LlmProviderDyn>,
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
        let CoeOutcome {
            response: text,
            decision,
            ..
        } = run_coe(
            &coe,
            "primary".into(),
            "primary response".into(),
            ChatExtras { entropy: Some(0.1) },
            coe.secondary.chat(&[]),
        )
        .await;

        assert_eq!(text, "primary response");
        assert_matches!(decision, CoeDecision::KeepPrimary);
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
        let CoeOutcome {
            response: text,
            decision,
            ..
        } = run_coe(
            &coe,
            "primary".into(),
            "primary response".into(),
            ChatExtras { entropy: Some(1.0) },
            coe.secondary.chat(&[]),
        )
        .await;

        assert_eq!(text, "primary response");
        assert_matches!(decision, CoeDecision::KeepPrimary);
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

        let CoeOutcome {
            response: text,
            decision,
            ..
        } = run_coe(
            &coe,
            "primary".into(),
            "primary response text long enough to matter".into(),
            ChatExtras { entropy: Some(1.0) }, // above intra_threshold=0.8
            coe.secondary.chat(&[]),
        )
        .await;

        assert_eq!(text, "secondary response");
        assert_matches!(decision, CoeDecision::EscalateIntra);
        assert_eq!(coe.metrics.intra_escalations.load(Ordering::Relaxed), 1);
    }
}
