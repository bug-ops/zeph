// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LLM-backed predicate evaluator for the per-subtask verification gate.
//!
//! [`VerifyPredicate`] and [`PredicateOutcome`] (the data types stored on `TaskNode`) live
//! in [`crate::graph`]. This module contains only [`PredicateEvaluator`], which requires
//! an LLM provider.

use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use zeph_common::OutputSanitizer;
use zeph_llm::provider::{LlmProvider, Message, Role};

use super::error::OrchestrationError;
use super::graph::{PredicateOutcome, VerifyPredicate};

/// LLM-backed predicate evaluator.
///
/// Evaluates a [`VerifyPredicate`] against task output by calling the configured
/// LLM provider with a judge prompt. Fail-open: evaluation errors produce a
/// permissive `passed = true` outcome with `confidence = 0.0` and log a warning
/// rather than aborting the scheduler.
///
/// Task output is sanitized via [`OutputSanitizer`] before being embedded in the
/// judge prompt, mirroring the same defence used by `PlanVerifier`.
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use zeph_orchestration::{PredicateEvaluator, VerifyPredicate};
/// use zeph_common::IdentitySanitizer;
///
/// # async fn example<P: zeph_llm::provider::LlmProvider>(provider: P) {
/// let evaluator = PredicateEvaluator::new(provider, Arc::new(IdentitySanitizer), 30);
/// let outcome = evaluator
///     .evaluate(
///         &VerifyPredicate::Natural("output must include a summary".to_string()),
///         "Here is the summary: ...",
///         None,
///     )
///     .await;
/// assert!(outcome.confidence >= 0.0);
/// # }
/// ```
pub struct PredicateEvaluator<P: LlmProvider> {
    provider: P,
    sanitizer: Arc<dyn OutputSanitizer>,
    timeout: Duration,
}

impl<P: LlmProvider> PredicateEvaluator<P> {
    /// Create a new evaluator backed by `provider`.
    ///
    /// `sanitizer` is applied to task output before it is embedded in the judge prompt.
    /// `timeout_secs` bounds the LLM call; on timeout the evaluator returns a fail-open
    /// outcome (`passed = true`, `confidence = 0.0`) and logs a warning.
    pub fn new(provider: P, sanitizer: Arc<dyn OutputSanitizer>, timeout_secs: u64) -> Self {
        Self {
            provider,
            sanitizer,
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    /// Evaluate `predicate` against `output`.
    ///
    /// `prior_failure_reason` is injected into the prompt on re-runs so the model
    /// knows why the previous attempt failed. Pass `None` on the first evaluation.
    ///
    /// On LLM or parse error, returns a permissive outcome (`passed = true,
    /// confidence = 0.0`) and logs a warning — fail-open per the orchestration
    /// error policy.
    ///
    /// # Errors
    ///
    /// This method is infallible: all errors are absorbed and result in a fail-open
    /// `PredicateOutcome`. The `OrchestrationError` variant is only returned by
    /// `VerifyPredicate::as_natural` for unsupported predicate types.
    #[tracing::instrument(
        name = "orchestration.verify_predicate.evaluate",
        skip(self, predicate, output, prior_failure_reason)
    )]
    pub async fn evaluate(
        &self,
        predicate: &VerifyPredicate,
        output: &str,
        prior_failure_reason: Option<&str>,
    ) -> PredicateOutcome {
        let criterion = match predicate.as_natural() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "unsupported predicate variant, skipping evaluation (fail-open)");
                return PredicateOutcome {
                    passed: true,
                    confidence: 0.0,
                    reason: format!("predicate not evaluated: {e}"),
                };
            }
        };

        let prior_note = prior_failure_reason
            .map(|r| {
                let truncated: String = r.chars().take(256).collect();
                format!(
                    "\n\n<prior_failure_reason>{truncated}</prior_failure_reason>\n\
                     Note: a previous evaluation failed with this reason. Take it into account."
                )
            })
            .unwrap_or_default();

        let system = format!(
            "You are a strict output verifier. Evaluate whether the task output satisfies \
             the given criterion. Respond with a JSON object: \
             {{\"passed\": true/false, \"confidence\": 0.0-1.0, \"reason\": \"...\"}}\n\
             Criterion: {criterion}{prior_note}"
        );

        // Sanitize task output before embedding it in the judge prompt (prompt-injection defence).
        let safe_output = self.sanitizer.sanitize_task_output(output);
        let user = format!("Task output:\n\n{safe_output}");

        let messages = vec![
            Message::from_legacy(Role::System, system),
            Message::from_legacy(Role::User, user),
        ];

        match tokio::time::timeout(
            self.timeout,
            self.provider.chat_typed::<EvalResponse>(&messages),
        )
        .await
        {
            Ok(Ok(resp)) => {
                let outcome = PredicateOutcome {
                    passed: resp.passed,
                    confidence: resp.confidence.clamp(0.0, 1.0),
                    reason: resp.reason,
                };
                if outcome.passed && outcome.confidence < 0.5 {
                    tracing::warn!(
                        confidence = outcome.confidence,
                        reason = %outcome.reason,
                        "weak predicate pass (confidence < 0.5)"
                    );
                }
                outcome
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    "predicate evaluation LLM call failed, returning fail-open outcome"
                );
                PredicateOutcome {
                    passed: true,
                    confidence: 0.0,
                    reason: format!("evaluation failed: {e}"),
                }
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = self.timeout.as_secs(),
                    "predicate evaluation timed out, returning fail-open outcome"
                );
                PredicateOutcome {
                    passed: true,
                    confidence: 0.0,
                    reason: "evaluation timed out".to_string(),
                }
            }
        }
    }
}

/// Internal response shape for predicate evaluation.
#[derive(Debug, Deserialize, JsonSchema)]
struct EvalResponse {
    passed: bool,
    confidence: f32,
    reason: String,
}

// Suppress unused-import warning when feature is disabled.
#[allow(unused_imports)]
use OrchestrationError as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_predicate_as_natural() {
        let pred = VerifyPredicate::Natural("must contain JSON".to_string());
        assert_eq!(pred.as_natural().unwrap(), "must contain JSON");
    }

    #[test]
    fn expression_predicate_returns_error() {
        let pred = VerifyPredicate::Expression("len(output) > 0".to_string());
        assert!(pred.as_natural().is_err());
    }

    #[test]
    fn predicate_outcome_serde_roundtrip() {
        let o = PredicateOutcome {
            passed: true,
            confidence: 0.85,
            reason: "looks good".to_string(),
        };
        let json = serde_json::to_string(&o).expect("serialize");
        let restored: PredicateOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.passed, o.passed);
        assert!((restored.confidence - o.confidence).abs() < f32::EPSILON);
        assert_eq!(restored.reason, o.reason);
    }

    #[test]
    fn verify_predicate_serde_roundtrip_natural() {
        let pred = VerifyPredicate::Natural("criterion".to_string());
        let json = serde_json::to_string(&pred).expect("serialize");
        let restored: VerifyPredicate = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(pred, restored);
    }

    #[test]
    fn task_node_missing_predicate_fields_deserialize_as_none() {
        let json = r#"{
            "id": 0,
            "title": "t",
            "description": "d",
            "agent_hint": null,
            "status": "pending",
            "depends_on": [],
            "result": null,
            "assigned_agent": null,
            "retry_count": 0,
            "failure_strategy": null,
            "max_retries": null
        }"#;
        let val: serde_json::Value = serde_json::from_str(json).expect("parse");
        assert!(val.get("verify_predicate").is_none());
        assert!(val.get("predicate_outcome").is_none());
    }
}
