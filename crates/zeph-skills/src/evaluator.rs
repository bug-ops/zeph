// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! External-feedback skill evaluator (#3319).
//!
//! [`SkillEvaluator`] calls a critic LLM to score a generated SKILL.md across three
//! dimensions — correctness, reusability, and specificity — and returns an accept/reject
//! verdict. This module is Feature B of the skills-compression-spectrum initiative.
//!
//! # Design notes
//!
//! The evaluator gate is a **quality heuristic**, not a safety boundary. A misbehaving
//! critic LLM should not block users from generating skills; they can always inspect and
//! delete a bad SKILL.md manually. For this reason the default policy is **fail-open**:
//! evaluator errors (timeout, JSON parse failure) produce [`SkillVerdict::AcceptOnEvalError`]
//! rather than a rejection. Operators who want strict quality enforcement can set
//! `fail_open_on_error = false` in config.
//!
//! Weights (`correctness`, `reusability`, `specificity`) must sum to 1.0 ± 1e-3; this is
//! enforced at config validation time. Defaults (0.50 / 0.25 / 0.25) and threshold (0.60)
//! are starting points based on intuition and will be tuned once real-world telemetry is
//! available.

use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, Role};

use crate::error::SkillError;

/// A request to evaluate a generated SKILL.md through the critic LLM.
///
/// All fields are borrowed from the caller — no allocation.
pub struct SkillEvaluationRequest<'a> {
    /// The human-readable skill name (as parsed from frontmatter).
    pub name: &'a str,
    /// The description field from the skill frontmatter.
    pub description: &'a str,
    /// The full SKILL.md content (frontmatter + body).
    pub body: &'a str,
    /// The original natural-language intent that triggered generation.
    pub original_intent: &'a str,
}

/// Three-dimensional score returned by the critic LLM.
///
/// Each dimension is a value in `[0.0, 1.0]`. Use [`composite`](Self::composite) to
/// combine them into a single scalar for threshold comparison.
#[derive(Debug, Clone)]
pub struct SkillQualityScore {
    /// How likely is the skill body to produce correct results? (0.0–1.0)
    pub correctness: f32,
    /// How well does the skill generalise beyond the exact original request? (0.0–1.0)
    pub reusability: f32,
    /// Is the skill tightly scoped (not too broad, not too narrow)? (0.0–1.0)
    pub specificity: f32,
    /// Free-text rationale from the critic.
    pub rationale: String,
}

impl SkillQualityScore {
    /// Compute the weighted composite score.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_skills::evaluator::{SkillQualityScore, EvaluationWeights};
    ///
    /// let score = SkillQualityScore {
    ///     correctness: 0.9,
    ///     reusability: 0.8,
    ///     specificity: 0.7,
    ///     rationale: String::new(),
    /// };
    /// let weights = EvaluationWeights::default();
    /// let composite = score.composite(&weights);
    /// assert!((composite - 0.825_f32).abs() < 1e-4, "expected ~0.825, got {composite}");
    /// ```
    #[must_use]
    pub fn composite(&self, w: &EvaluationWeights) -> f32 {
        w.correctness * self.correctness
            + w.reusability * self.reusability
            + w.specificity * self.specificity
    }
}

/// Weights used to combine the three quality dimensions into a single composite score.
///
/// Weights must sum to 1.0 ± 1e-3; this is enforced at config validation time.
#[derive(Debug, Clone, Copy)]
pub struct EvaluationWeights {
    /// Weight for correctness (default 0.50).
    pub correctness: f32,
    /// Weight for reusability (default 0.25).
    pub reusability: f32,
    /// Weight for specificity (default 0.25).
    pub specificity: f32,
}

impl Default for EvaluationWeights {
    fn default() -> Self {
        Self {
            correctness: 0.50,
            reusability: 0.25,
            specificity: 0.25,
        }
    }
}

/// The outcome of an evaluation run.
#[derive(Debug, Clone)]
pub enum SkillVerdict {
    /// Skill meets the quality threshold.
    Accept(SkillQualityScore),
    /// Skill is below threshold or was explicitly rejected.
    Reject {
        /// The score that caused rejection.
        score: SkillQualityScore,
        /// Human-readable reason.
        reason: String,
    },
    /// The evaluator encountered an error (timeout or parse failure) but
    /// `fail_open_on_error = true`, so the skill is accepted anyway.
    AcceptOnEvalError(String),
}

/// Evaluates generated SKILL.md files through a critic LLM.
///
/// Constructed from [`SkillEvaluationConfig`](zeph_config::SkillEvaluationConfig) by the
/// agent builder. A single instance is shared across all skill-generating paths
/// (`SkillGenerator`, `ProactiveExplorer`, `PromotionEngine`) via `Arc<SkillEvaluator>`.
///
/// # Examples
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use zeph_skills::evaluator::{SkillEvaluator, EvaluationWeights, SkillEvaluationRequest};
///
/// async fn demo(provider: zeph_llm::any::AnyProvider) {
///     let evaluator = SkillEvaluator::new(provider, EvaluationWeights::default(), 0.60, true, 15_000);
///     let req = SkillEvaluationRequest {
///         name: "fetch-weather",
///         description: "Fetch weather data from wttr.in",
///         body: "---\nname: fetch-weather\n...",
///         original_intent: "I need to get weather information",
///     };
///     let verdict = evaluator.evaluate(&req).await;
///     println!("{verdict:?}");
/// }
/// ```
pub struct SkillEvaluator {
    critic: AnyProvider,
    weights: EvaluationWeights,
    threshold: f32,
    /// When `true`, evaluator errors produce `AcceptOnEvalError` instead of `Reject`.
    fail_open: bool,
    /// Timeout in milliseconds for the LLM call.
    timeout_ms: u64,
}

impl SkillEvaluator {
    /// Create a new evaluator.
    ///
    /// `fail_open`: when `true` (recommended default), evaluator errors accept the skill.
    /// `timeout_ms`: maximum wait for the critic LLM response.
    #[must_use]
    pub fn new(
        critic: AnyProvider,
        weights: EvaluationWeights,
        threshold: f32,
        fail_open: bool,
        timeout_ms: u64,
    ) -> Self {
        Self {
            critic,
            weights,
            threshold,
            fail_open,
            timeout_ms,
        }
    }

    /// Evaluate a generated skill through the critic LLM.
    ///
    /// Returns a [`SkillVerdict`]. Never returns `Err` unless the skill violates a
    /// hard constraint (currently unused — all LLM errors are handled by the fail-open
    /// policy). The `Err` variant is reserved for future hard-error scenarios.
    ///
    /// # Errors
    ///
    /// Currently always returns `Ok(_)`. Error propagation is reserved for future use.
    #[tracing::instrument(name = "skills.eval.evaluate", skip_all, fields(skill_name = %req.name))]
    pub async fn evaluate(
        &self,
        req: &SkillEvaluationRequest<'_>,
    ) -> Result<SkillVerdict, SkillError> {
        let prompt = build_eval_prompt(req);
        let messages = vec![
            Message::from_legacy(Role::System, EVAL_SYSTEM_PROMPT),
            Message::from_legacy(Role::User, &prompt),
        ];

        let llm_result = tokio::time::timeout(Duration::from_millis(self.timeout_ms), async {
            let span = tracing::info_span!("skills.eval.llm_call");
            let _enter = span.enter();
            self.critic.chat(&messages).await
        })
        .await;

        let raw = match llm_result {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                let msg = format!("critic LLM error: {e}");
                tracing::warn!(error = %e, "skill evaluator LLM call failed");
                return Ok(self.handle_error(msg));
            }
            Err(_timeout) => {
                let msg = format!("critic LLM timed out after {}ms", self.timeout_ms);
                tracing::warn!(timeout_ms = self.timeout_ms, "skill evaluator timed out");
                return Ok(self.handle_error(msg));
            }
        };

        match parse_eval_response(&raw) {
            Ok(score) => {
                let composite = score.composite(&self.weights);
                if composite >= self.threshold {
                    Ok(SkillVerdict::Accept(score))
                } else {
                    let reason = format!(
                        "composite score {composite:.3} below threshold {:.3}: {}",
                        self.threshold, score.rationale
                    );
                    Ok(SkillVerdict::Reject { score, reason })
                }
            }
            Err(parse_err) => {
                let msg = format!("failed to parse evaluator JSON: {parse_err}");
                tracing::warn!(error = %parse_err, "skill evaluator JSON parse failed");
                Ok(self.handle_error(msg))
            }
        }
    }

    fn handle_error(&self, msg: String) -> SkillVerdict {
        if self.fail_open {
            tracing::info!(%msg, "skills.eval.fail_open_triggered: accepting skill despite evaluator error");
            SkillVerdict::AcceptOnEvalError(msg)
        } else {
            SkillVerdict::Reject {
                score: zero_score(msg.clone()),
                reason: format!("evaluator error, fail-closed: {msg}"),
            }
        }
    }
}

/// System prompt given to the critic LLM.
const EVAL_SYSTEM_PROMPT: &str = "\
You are a strict quality reviewer for SKILL.md files used by AI agents. \
Evaluate the skill on three dimensions and return ONLY a JSON object, no extra text.\n\n\
JSON schema:\n\
{\n  \"correctness\": <float 0.0-1.0>,\n  \"reusability\": <float 0.0-1.0>,\n  \
\"specificity\": <float 0.0-1.0>,\n  \"rationale\": \"<one sentence>\"\n}\n\n\
Dimension definitions:\n\
- correctness: Is the skill body likely to produce correct, safe, and useful results?\n\
- reusability: Does the skill generalise beyond the exact original request (not over-fitted)?\n\
- specificity: Is the skill tightly scoped — not too broad to be useless, not so narrow it helps only once?\n";

fn build_eval_prompt(req: &SkillEvaluationRequest<'_>) -> String {
    format!(
        "Original intent: {}\n\nSkill name: {}\n\nDescription: {}\n\nSKILL.md body:\n```\n{}\n```",
        req.original_intent, req.name, req.description, req.body
    )
}

fn zero_score(rationale: String) -> SkillQualityScore {
    SkillQualityScore {
        correctness: 0.0,
        reusability: 0.0,
        specificity: 0.0,
        rationale,
    }
}

fn parse_eval_response(raw: &str) -> Result<SkillQualityScore, serde_json::Error> {
    #[derive(serde::Deserialize)]
    struct EvalResponse {
        correctness: f32,
        reusability: f32,
        specificity: f32,
        rationale: String,
    }

    let trimmed = raw.trim();
    // Strip markdown code fences if present.
    let json_str = if let Some(inner) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.trim_start_matches('\n').rsplit_once("```"))
    {
        inner.0.trim()
    } else {
        trimmed
    };

    let resp: EvalResponse = serde_json::from_str(json_str)?;
    Ok(SkillQualityScore {
        correctness: resp.correctness.clamp(0.0, 1.0),
        reusability: resp.reusability.clamp(0.0, 1.0),
        specificity: resp.specificity.clamp(0.0, 1.0),
        rationale: resp.rationale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_score_default_weights() {
        let score = SkillQualityScore {
            correctness: 0.9,
            reusability: 0.8,
            specificity: 0.7,
            rationale: String::new(),
        };
        let w = EvaluationWeights::default();
        let c = score.composite(&w);
        // 0.9*0.5 + 0.8*0.25 + 0.7*0.25 = 0.45 + 0.20 + 0.175 = 0.825
        assert!((c - 0.825_f32).abs() < 1e-4, "expected ~0.825, got {c}");
    }

    #[test]
    fn composite_score_custom_weights() {
        let score = SkillQualityScore {
            correctness: 1.0,
            reusability: 0.0,
            specificity: 0.0,
            rationale: String::new(),
        };
        let w = EvaluationWeights {
            correctness: 1.0,
            reusability: 0.0,
            specificity: 0.0,
        };
        assert!((score.composite(&w) - 1.0_f32).abs() < 1e-6);
    }

    #[test]
    fn parse_eval_response_valid_json() {
        let raw =
            r#"{"correctness": 0.9, "reusability": 0.8, "specificity": 0.7, "rationale": "good"}"#;
        let score = parse_eval_response(raw).unwrap();
        assert!((score.correctness - 0.9).abs() < 1e-6);
        assert!((score.reusability - 0.8).abs() < 1e-6);
        assert!((score.specificity - 0.7).abs() < 1e-6);
        assert_eq!(score.rationale, "good");
    }

    #[test]
    fn parse_eval_response_strips_code_fence() {
        let raw = "```json\n{\"correctness\": 0.5, \"reusability\": 0.5, \"specificity\": 0.5, \"rationale\": \"ok\"}\n```";
        let score = parse_eval_response(raw).unwrap();
        assert!((score.correctness - 0.5).abs() < 1e-6);
    }

    #[test]
    fn parse_eval_response_clamps_out_of_range() {
        let raw =
            r#"{"correctness": 1.5, "reusability": -0.1, "specificity": 0.5, "rationale": "x"}"#;
        let score = parse_eval_response(raw).unwrap();
        assert!(
            (score.correctness - 1.0).abs() < 1e-6,
            "should clamp to 1.0"
        );
        assert!(
            (score.reusability - 0.0).abs() < 1e-6,
            "should clamp to 0.0"
        );
    }

    #[test]
    fn parse_eval_response_invalid_returns_err() {
        let raw = "not json at all";
        assert!(parse_eval_response(raw).is_err());
    }

    #[tokio::test]
    async fn evaluator_fail_open_on_llm_error() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let mock = MockProvider::failing();
        let ev = SkillEvaluator::new(
            AnyProvider::Mock(mock),
            EvaluationWeights::default(),
            0.60,
            true,
            5_000,
        );
        let req = SkillEvaluationRequest {
            name: "test-skill",
            description: "A test skill.",
            body: "---\nname: test-skill\n---\n\n## Usage\n\nTest.",
            original_intent: "test",
        };
        let verdict = ev.evaluate(&req).await.unwrap();
        assert!(
            matches!(verdict, SkillVerdict::AcceptOnEvalError(_)),
            "expected AcceptOnEvalError, got {verdict:?}"
        );
    }

    #[tokio::test]
    async fn evaluator_fail_closed_on_llm_error() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let mock = MockProvider::failing();
        let ev = SkillEvaluator::new(
            AnyProvider::Mock(mock),
            EvaluationWeights::default(),
            0.60,
            false, // fail-closed
            5_000,
        );
        let req = SkillEvaluationRequest {
            name: "test-skill",
            description: "A test skill.",
            body: "---\nname: test-skill\n---\n\n## Usage\n\nTest.",
            original_intent: "test",
        };
        let verdict = ev.evaluate(&req).await.unwrap();
        assert!(
            matches!(verdict, SkillVerdict::Reject { .. }),
            "expected Reject, got {verdict:?}"
        );
    }

    #[tokio::test]
    async fn evaluator_accept_high_score() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let response = r#"{"correctness": 0.9, "reusability": 0.8, "specificity": 0.9, "rationale": "excellent"}"#;
        let mock = MockProvider::with_responses(vec![response.to_string()]);
        let ev = SkillEvaluator::new(
            AnyProvider::Mock(mock),
            EvaluationWeights::default(),
            0.60,
            true,
            5_000,
        );
        let req = SkillEvaluationRequest {
            name: "fetch-weather",
            description: "Fetch weather data.",
            body: "---\nname: fetch-weather\n---\n## Usage\n\nFetch it.",
            original_intent: "get weather",
        };
        let verdict = ev.evaluate(&req).await.unwrap();
        assert!(
            matches!(verdict, SkillVerdict::Accept(_)),
            "expected Accept, got {verdict:?}"
        );
    }

    #[tokio::test]
    async fn evaluator_reject_low_score() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let response =
            r#"{"correctness": 0.2, "reusability": 0.1, "specificity": 0.1, "rationale": "poor"}"#;
        let mock = MockProvider::with_responses(vec![response.to_string()]);
        let ev = SkillEvaluator::new(
            AnyProvider::Mock(mock),
            EvaluationWeights::default(),
            0.60,
            true,
            5_000,
        );
        let req = SkillEvaluationRequest {
            name: "bad-skill",
            description: "Bad skill.",
            body: "---\nname: bad-skill\n---\n## Usage\n\nBad.",
            original_intent: "do something bad",
        };
        let verdict = ev.evaluate(&req).await.unwrap();
        assert!(
            matches!(verdict, SkillVerdict::Reject { .. }),
            "expected Reject, got {verdict:?}"
        );
    }
}
