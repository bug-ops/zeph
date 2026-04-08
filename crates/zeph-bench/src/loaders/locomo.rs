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
/// Schema (lmlab/locomo on HuggingFace):
/// ```json
/// [{"session_id": "...", "qa": [{"question": "...", "answer": "..."}]}]
/// ```
///
/// Each QA pair becomes one [`Scenario`] with id `"{session_id}_{qa_index}"`.
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
                scenarios.push(Scenario {
                    id: format!("{}_{}", session.session_id, idx),
                    prompt: qa.question.clone(),
                    expected: qa.answer.clone(),
                    metadata: serde_json::Value::Null,
                });
            }
        }
        Ok(scenarios)
    }
}

/// Evaluates LOCOMO responses using token F1 with a threshold of 0.5.
#[derive(Debug)]
pub struct LocomoEvaluator;

impl Evaluator for LocomoEvaluator {
    fn evaluate(&self, scenario: &Scenario, agent_response: &str) -> EvalResult {
        let score = token_f1(agent_response, &scenario.expected);
        EvalResult {
            scenario_id: scenario.id.clone(),
            score,
            passed: score >= PASS_THRESHOLD,
            details: format!("token_f1={score:.4}"),
        }
    }
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
        assert_eq!(scenarios[0].prompt, "What is Rust?");
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
