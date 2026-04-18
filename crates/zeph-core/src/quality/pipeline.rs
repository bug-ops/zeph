// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SelfCheckPipeline`: orchestrates the Proposer → Checker MARCH pipeline.

use std::sync::Arc;
use std::time::{Duration, Instant};

use zeph_llm::any::AnyProvider;

use super::checker::run_checker;
use super::config::{QualityConfig, TriggerPolicy};
use super::proposer::run_proposer;
use super::types::{AssertionVerdict, SelfCheckReport, SkipReason, StageOutcome, VerdictStatus};

/// Collected retrieved-memory context for a turn.
///
/// All fields hold borrowed slices from message parts so no allocation is needed
/// beyond the joining step.
#[derive(Debug, Default)]
pub struct RetrievedContext<'a> {
    /// Semantic recall fragments.
    pub recall: Vec<&'a str>,
    /// Graph/known-facts fragments.
    pub graph_facts: Vec<&'a str>,
    /// Cross-session memory fragments.
    pub cross_session: Vec<&'a str>,
    /// Compaction/conversation summaries.
    pub summaries: Vec<&'a str>,
}

impl RetrievedContext<'_> {
    /// Returns `true` when no retrieved context was found for this turn.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.recall.is_empty()
            && self.graph_facts.is_empty()
            && self.cross_session.is_empty()
            && self.summaries.is_empty()
    }

    /// Concatenate all fragments with the given separator.
    #[must_use]
    pub fn joined(&self, sep: &str) -> String {
        let parts: Vec<&str> = self
            .recall
            .iter()
            .chain(&self.graph_facts)
            .chain(&self.cross_session)
            .chain(&self.summaries)
            .copied()
            .collect();
        parts.join(sep)
    }
}

/// MARCH self-check pipeline.
///
/// Built once per active provider via [`SelfCheckPipeline::build`] and stored on the agent.
/// Rebuilt whenever the active provider changes.
pub struct SelfCheckPipeline {
    pub(crate) cfg: QualityConfig,
    proposer: AnyProvider,
    /// Separate instance with prompt-cache emission suppressed (H3).
    checker: AnyProvider,
}

impl SelfCheckPipeline {
    /// Reference to the pipeline config (used by the turn-level hook).
    pub(crate) fn cfg_ref(&self) -> &QualityConfig {
        &self.cfg
    }
}

impl SelfCheckPipeline {
    /// Build the pipeline from the given config and main provider.
    ///
    /// In MVP, `proposer_provider` / `checker_provider` config fields are advisory no-ops;
    /// the main provider is used for both roles.
    ///
    /// # Errors
    ///
    /// Returns an error string if config validation fails.
    pub fn build(config: &QualityConfig, main_provider: &AnyProvider) -> Result<Arc<Self>, String> {
        config.validate().map_err(|e| e.to_string())?;
        let proposer = main_provider.clone();
        let checker = if config.cache_disabled_for_checker {
            main_provider.with_prompt_cache_disabled()
        } else {
            main_provider.clone()
        };
        Ok(Arc::new(Self {
            cfg: config.clone(),
            proposer,
            checker,
        }))
    }

    /// Run the full self-check pipeline for one turn.
    ///
    /// Returns a [`SelfCheckReport`] whether or not assertions were found or flagged.
    pub async fn run(
        &self,
        response: &str,
        retrieved_context: RetrievedContext<'_>,
        user_query: &str,
        turn_id: u64,
    ) -> SelfCheckReport {
        let started = Instant::now();
        let per_call = Duration::from_millis(self.cfg.per_call_timeout_ms);

        // Apply trigger policy
        if self.cfg.trigger == TriggerPolicy::HasRetrieval && retrieved_context.is_empty() {
            return SelfCheckReport {
                turn_id,
                assertions: vec![],
                verdicts: vec![],
                flagged_ids: vec![],
                latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                proposer_tokens: 0,
                checker_tokens: 0,
                proposer_outcome: StageOutcome::Skipped(SkipReason::NoRetrievedContext),
                checker_outcome: StageOutcome::Skipped(SkipReason::NoRetrievedContext),
                parse_retries: 0,
            };
        }

        // Proposer stage
        let (assertions, p_tokens, p_outcome, p_retries) =
            run_proposer(&self.proposer, response, self.cfg.max_assertions, per_call).await;

        if assertions.is_empty() {
            return SelfCheckReport {
                turn_id,
                assertions,
                verdicts: vec![],
                flagged_ids: vec![],
                latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                proposer_tokens: p_tokens,
                checker_tokens: 0,
                proposer_outcome: p_outcome,
                checker_outcome: StageOutcome::Skipped(SkipReason::NoAssistantText),
                parse_retries: p_retries,
            };
        }

        let evidence = retrieved_context.joined("\n\n");

        // Checker stage
        let (verdicts, c_tokens, c_outcome, c_retries) =
            run_checker(&self.checker, &assertions, &evidence, user_query, per_call).await;

        let flagged_ids = self.compute_flagged(&verdicts);

        SelfCheckReport {
            turn_id,
            assertions,
            verdicts,
            flagged_ids,
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            proposer_tokens: p_tokens,
            checker_tokens: c_tokens,
            proposer_outcome: p_outcome,
            checker_outcome: c_outcome,
            parse_retries: p_retries + c_retries,
        }
    }

    /// Compute the set of assertion IDs that should be flagged.
    ///
    /// Flagged = `Contradicted` OR (`status != Irrelevant` AND `evidence < min_evidence`).
    /// `Irrelevant` verdicts are never flagged regardless of evidence score.
    fn compute_flagged(&self, verdicts: &[AssertionVerdict]) -> Vec<u32> {
        verdicts
            .iter()
            .filter(|v| {
                v.status == VerdictStatus::Contradicted
                    || (v.status != VerdictStatus::Irrelevant && v.evidence < self.cfg.min_evidence)
            })
            .map(|v| v.id)
            .collect()
    }
}
