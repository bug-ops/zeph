// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use serde::Deserialize;

use crate::{
    error::BenchError,
    scenario::{DatasetLoader, EvalResult, Evaluator, Scenario, token_f1},
};

const PASS_THRESHOLD: f64 = 0.5;

#[derive(Debug, Deserialize)]
struct LocomoSession {
    session_id: String,
    qa: Vec<LocomoQa>,
}

#[derive(Debug, Deserialize)]
struct LocomoQa {
    question: String,
    answer: String,
}

/// Loads LOCOMO benchmark scenarios from a JSON file.
///
/// **Source**: [`lmlab/locomo`](https://huggingface.co/datasets/lmlab/locomo) on `HuggingFace`.
///
/// **Schema**: the file is a JSON array of session objects:
/// ```json
/// [
///   {
///     "session_id": "abc",
///     "qa": [
///       {"question": "...", "answer": "..."}
///     ]
///   }
/// ]
/// ```
///
/// Each QA pair within a session becomes one [`Scenario`] with id
/// `"{session_id}_{qa_index}"` (zero-based). `metadata` is set to
/// [`serde_json::Value::Null`] because LOCOMO QA pairs carry no extra fields.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use zeph_bench::loaders::LocomoLoader;
/// use zeph_bench::scenario::DatasetLoader;
///
/// let scenarios = LocomoLoader.load(Path::new("/data/locomo.json")).unwrap();
/// println!("loaded {} scenarios", scenarios.len());
/// ```
#[derive(Debug)]
pub struct LocomoLoader;

impl DatasetLoader for LocomoLoader {
    fn name(&self) -> &'static str {
        "locomo"
    }

    /// # Errors
    ///
    /// Returns [`BenchError::Io`] when the file cannot be read and
    /// [`BenchError::InvalidFormat`] when JSON parsing fails.
    fn load(&self, path: &Path) -> Result<Vec<Scenario>, BenchError> {
        let content = std::fs::read_to_string(path)?;
        let sessions: Vec<LocomoSession> =
            serde_json::from_str(&content).map_err(|e| BenchError::InvalidFormat(e.to_string()))?;

        let mut scenarios = Vec::new();
        for session in sessions {
            for (idx, qa) in session.qa.iter().enumerate() {
                scenarios.push(Scenario::single(
                    format!("{}_{}", session.session_id, idx),
                    qa.question.clone(),
                    qa.answer.clone(),
                    serde_json::Value::Null,
                ));
            }
        }
        Ok(scenarios)
    }
}

/// Evaluates LOCOMO responses using token F1 with a pass threshold of 0.5.
///
/// A response passes when its token F1 score against the gold answer is ≥ 0.5.
/// The raw score (in `0.0..=1.0`) is always written to the result regardless of
/// the pass/fail decision.
///
/// # Examples
///
/// ```
/// use zeph_bench::{Scenario, loaders::LocomoEvaluator};
/// use zeph_bench::scenario::Evaluator;
///
/// let scenario = Scenario::single(
///     "s1_0",
///     "What is the capital of France?",
///     "Paris",
///     serde_json::Value::Null,
/// );
///
/// let result = LocomoEvaluator.evaluate(&scenario, "Paris");
/// assert!((result.score - 1.0).abs() < f64::EPSILON);
/// assert!(result.passed);
///
/// let bad = LocomoEvaluator.evaluate(&scenario, "completely unrelated answer xyz");
/// assert!(!bad.passed);
/// ```
#[derive(Debug)]
pub struct LocomoEvaluator;

impl Evaluator for LocomoEvaluator {
    fn evaluate(&self, scenario: &Scenario, agent_response: &str) -> EvalResult {
        // Normalize both sides before scoring: lowercase and keep only alphanumeric/whitespace.
        // This ensures "4." and "Leonardo da Vinci." match their expected forms.
        let normalized_response = normalize_for_f1(agent_response);
        let normalized_expected = normalize_for_f1(&scenario.expected);
        let score = token_f1(&normalized_response, &normalized_expected);
        EvalResult {
            scenario_id: scenario.id.clone(),
            score,
            passed: score >= PASS_THRESHOLD,
            details: format!("token_f1={score:.4}"),
        }
    }
}

/// Normalize a string for token-F1 scoring: lowercase and strip non-alphanumeric characters.
///
/// This mirrors the normalization used in the original `SQuAD` evaluation script and
/// ensures that punctuation differences (e.g., "Paris." vs "Paris") do not penalize
/// otherwise correct answers.
fn normalize_for_f1(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"[
        {
            "session_id": "s1",
            "qa": [
                {"question": "What is Rust?", "answer": "A systems programming language"},
                {"question": "Is it fast?", "answer": "Yes"}
            ]
        }
    ]"#;

    fn load_from_str(json: &str) -> Vec<Scenario> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locomo.json");
        std::fs::write(&path, json).unwrap();
        LocomoLoader.load(&path).unwrap()
    }

    #[test]
    fn load_parses_scenario_count() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios.len(), 2);
    }

    #[test]
    fn load_builds_correct_ids() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios[0].id, "s1_0");
        assert_eq!(scenarios[1].id, "s1_1");
    }

    #[test]
    fn load_maps_prompt_and_expected() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios[0].primary_prompt().unwrap(), "What is Rust?");
        assert_eq!(scenarios[0].expected, "A systems programming language");
    }

    #[test]
    fn evaluator_perfect_match_passes() {
        let scenarios = load_from_str(FIXTURE);
        let result = LocomoEvaluator.evaluate(&scenarios[0], "A systems programming language");
        assert!((result.score - 1.0).abs() < f64::EPSILON);
        assert!(result.passed);
    }

    #[test]
    fn evaluator_no_match_fails() {
        let scenarios = load_from_str(FIXTURE);
        let result = LocomoEvaluator.evaluate(&scenarios[0], "completely different response xyz");
        assert!(!result.passed);
    }

    #[test]
    fn load_invalid_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(LocomoLoader.load(&path).is_err());
    }
}
