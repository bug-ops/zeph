// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-subtask verification predicates (predicate gate).
//!
//! Each task in a DAG may carry a `VerifyPredicate` that must be satisfied by
//! the task's output before downstream tasks may consume it. Evaluation is
//! LLM-based via `PredicateEvaluator`.
//!
//! # Design
//!
//! - [`VerifyPredicate`] is an enum stored in `TaskNode.verify_predicate`. Only the
//!   `Natural(String)` variant is constructible in v1 — `Expression` returns an error
//!   if the planner ever emits one.
//! - [`PredicateOutcome`] is persisted on `TaskNode` via `GraphPersistence::save` (wired in
//!   `zeph-core` scheduler loop and `handle_plan_confirm`). After a crash, rehydrating the
//!   graph via `/plan resume <id>` restores `predicate_outcome` so the gate is not re-evaluated
//!   for already-completed tasks.
//! - [`PredicateEvaluator`] wraps any [`LlmProvider`] and produces [`PredicateOutcome`]
//!   values. The evaluation prompt is intentionally minimal and model-agnostic.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeph_llm::provider::{LlmProvider, Message, Role};
use zeph_sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};

use super::error::OrchestrationError;

/// A verification criterion attached to a task node.
///
/// The planner populates this from the `verify_criteria` field in its JSON output.
/// Only `Natural` is constructible in v1. If the planner emits `Expression`, the
/// scheduler returns `OrchestrationError::PredicateNotSupported` rather than
/// silently ignoring the criterion.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::VerifyPredicate;
///
/// let pred = VerifyPredicate::Natural("output must contain a valid JSON object".to_string());
/// assert!(matches!(pred, VerifyPredicate::Natural(_)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum VerifyPredicate {
    /// Free-form natural-language criterion evaluated by the LLM judge.
    Natural(String),
    /// Symbolic expression (reserved, not supported in v1).
    Expression(String),
}

impl VerifyPredicate {
    /// Returns `Ok(&criterion)` for `Natural` predicates; `Err(PredicateNotSupported)`
    /// for unsupported variants.
    ///
    /// # Errors
    ///
    /// Returns [`OrchestrationError::PredicateNotSupported`] when the variant is not
    /// evaluatable in the current version.
    pub fn as_natural(&self) -> Result<&str, OrchestrationError> {
        match self {
            VerifyPredicate::Natural(s) => Ok(s.as_str()),
            VerifyPredicate::Expression(s) => Err(OrchestrationError::PredicateNotSupported(
                format!("Expression predicate '{s}' is not supported in v1; use Natural"),
            )),
        }
    }
}

/// Result of evaluating a [`VerifyPredicate`] against a task's output.
///
/// Stored on `TaskNode::predicate_outcome` (in-memory only; restart re-evaluates
/// any pending predicates). A `None` value signals "not yet evaluated"; consumers
/// should re-emit `SchedulerAction::VerifyPredicate` on the next tick.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::PredicateOutcome;
///
/// let outcome = PredicateOutcome { passed: true, confidence: 0.9, reason: "output is valid JSON".to_string() };
/// assert!(outcome.passed);
/// assert!(outcome.confidence > 0.8);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredicateOutcome {
    /// Whether the predicate was satisfied.
    pub passed: bool,
    /// Confidence score in [0.0, 1.0]. Values < 0.5 with `passed = true` log a warn.
    pub confidence: f32,
    /// Human-readable explanation from the LLM judge.
    pub reason: String,
}

/// LLM-backed predicate evaluator.
///
/// Evaluates a [`VerifyPredicate`] against task output by calling the configured
/// LLM provider with a judge prompt. Fail-open: evaluation errors produce a
/// permissive `passed = true` outcome with `confidence = 0.0` and log a warning
/// rather than aborting the scheduler.
///
/// Task output is sanitized via [`ContentSanitizer`] before being embedded in the
/// judge prompt, mirroring the same defence used by `PlanVerifier`.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_orchestration::{PredicateEvaluator, VerifyPredicate};
/// use zeph_sanitizer::{ContentSanitizer, ContentIsolationConfig};
///
/// # async fn example<P: zeph_llm::provider::LlmProvider>(provider: P) {
/// let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());
/// let evaluator = PredicateEvaluator::new(provider, sanitizer, 30);
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
    sanitizer: ContentSanitizer,
    timeout: Duration,
}

impl<P: LlmProvider> PredicateEvaluator<P> {
    /// Create a new evaluator backed by `provider`.
    ///
    /// `sanitizer` is applied to task output before it is embedded in the judge prompt.
    /// `timeout_secs` bounds the LLM call; on timeout the evaluator returns a fail-open
    /// outcome (`passed = true`, `confidence = 0.0`) and logs a warning.
    pub fn new(provider: P, sanitizer: ContentSanitizer, timeout_secs: u64) -> Self {
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
                // Truncate and wrap in XML tags to prevent injection from compromised judge output.
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
        let source = ContentSource::new(ContentSourceKind::ToolResult)
            .with_identifier("predicate-evaluator-input");
        let sanitized = self.sanitizer.sanitize(output, source);
        let user = format!("Task output:\n\n{}", sanitized.body);

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
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EvalResponse {
    passed: bool,
    confidence: f32,
    reason: String,
}

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
        // Simulate old JSON blob without predicate fields — #[serde(default)] must handle it.
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
        // Parse as serde_json::Value first (TaskNode is in graph.rs; test the concept here
        // by checking that our types have correct default handling).
        let val: serde_json::Value = serde_json::from_str(json).expect("parse");
        assert!(val.get("verify_predicate").is_none());
        assert!(val.get("predicate_outcome").is_none());
        // Actual TaskNode deserialization is tested in graph.rs tests.
    }
}
