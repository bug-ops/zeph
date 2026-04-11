// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    io::{BufRead as _, BufReader},
    path::Path,
};

use serde::Deserialize;

use crate::{
    error::BenchError,
    scenario::{DatasetLoader, EvalResult, Evaluator, Scenario, exact_match, token_f1},
};

#[derive(Debug, Deserialize)]
struct LongMemEvalItem {
    question_id: String,
    question: String,
    answer: String,
    session_id: String,
    sessions: serde_json::Value,
}

/// Loads `LongMemEval` benchmark scenarios from a JSONL file.
///
/// **Source**: [`xiaowu0162/longmemeval`](https://huggingface.co/datasets/xiaowu0162/longmemeval)
/// on `HuggingFace`.
///
/// **Schema**: one JSON object per line:
/// ```json
/// {"question_id":"q_001","question":"...","answer":"...","session_id":"sess_1","sessions":[...]}
/// ```
///
/// Each non-empty line becomes one [`Scenario`]:
/// - `id` — `question_id`
/// - `prompt` — `question`
/// - `expected` — `answer`
/// - `metadata` — `{"session_id": ..., "sessions": [...]}`
///
/// Empty lines are skipped. Parse errors include the 1-based line number.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use zeph_bench::loaders::LongMemEvalLoader;
/// use zeph_bench::scenario::DatasetLoader;
///
/// let scenarios = LongMemEvalLoader.load(Path::new("/data/longmemeval.jsonl")).unwrap();
/// println!("loaded {} scenarios", scenarios.len());
/// ```
#[derive(Debug)]
pub struct LongMemEvalLoader;

impl DatasetLoader for LongMemEvalLoader {
    fn name(&self) -> &'static str {
        "longmemeval"
    }

    /// # Errors
    ///
    /// Returns [`BenchError::Io`] when the file cannot be read and
    /// [`BenchError::InvalidFormat`] when a JSONL line cannot be parsed
    /// (message includes the 1-based line number).
    fn load(&self, path: &Path) -> Result<Vec<Scenario>, BenchError> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);

        let mut scenarios = Vec::new();
        for (idx, line) in reader.lines().enumerate() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let item: LongMemEvalItem = serde_json::from_str(trimmed)
                .map_err(|e| BenchError::InvalidFormat(format!("line {}: {e}", idx + 1)))?;

            scenarios.push(Scenario {
                id: item.question_id,
                prompt: item.question,
                expected: item.answer,
                metadata: serde_json::json!({
                    "session_id": item.session_id,
                    "sessions": item.sessions,
                }),
            });
        }
        Ok(scenarios)
    }
}

/// Evaluates `LongMemEval` responses using exact match as primary metric.
///
/// - `passed` = `exact_match(agent_response, expected)`
/// - `score` = `1.0` if exact match, otherwise `token_f1` value (partial credit)
/// - `details` = `"exact_match=true/false token_f1=0.XXXX"`
///
/// # Examples
///
/// ```
/// use zeph_bench::{Scenario, loaders::LongMemEvalEvaluator};
/// use zeph_bench::scenario::Evaluator;
///
/// let scenario = Scenario {
///     id: "q1".into(),
///     prompt: "What is Rust?".into(),
///     expected: "A systems language".into(),
///     metadata: serde_json::Value::Null,
/// };
///
/// let result = LongMemEvalEvaluator.evaluate(&scenario, "A systems language");
/// assert!(result.passed);
/// assert!((result.score - 1.0).abs() < f64::EPSILON);
/// ```
#[derive(Debug)]
pub struct LongMemEvalEvaluator;

impl Evaluator for LongMemEvalEvaluator {
    fn evaluate(&self, scenario: &Scenario, agent_response: &str) -> EvalResult {
        let matched = exact_match(agent_response, &scenario.expected);
        let f1 = token_f1(agent_response, &scenario.expected);
        let score = if matched { 1.0 } else { f1 };
        EvalResult {
            scenario_id: scenario.id.clone(),
            score,
            passed: matched,
            details: format!("exact_match={matched} token_f1={f1:.4}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"question_id":"q1","question":"What is Rust?","answer":"A systems language","session_id":"s1","sessions":[]}
{"question_id":"q2","question":"Is it fast?","answer":"Yes","session_id":"s1","sessions":[]}
{"question_id":"q3","question":"Creator?","answer":"Graydon Hoare","session_id":"s2","sessions":[]}"#;

    fn load_from_str(jsonl: &str) -> Vec<Scenario> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("longmemeval.jsonl");
        std::fs::write(&path, jsonl).unwrap();
        LongMemEvalLoader.load(&path).unwrap()
    }

    #[test]
    fn load_parses_scenario_count() {
        assert_eq!(load_from_str(FIXTURE).len(), 3);
    }

    #[test]
    fn load_builds_correct_ids() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios[0].id, "q1");
        assert_eq!(scenarios[1].id, "q2");
        assert_eq!(scenarios[2].id, "q3");
    }

    #[test]
    fn load_maps_prompt_and_expected() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios[0].prompt, "What is Rust?");
        assert_eq!(scenarios[0].expected, "A systems language");
    }

    #[test]
    fn load_stores_session_id_in_metadata() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios[0].metadata["session_id"], "s1");
    }

    #[test]
    fn load_stores_sessions_in_metadata() {
        let scenarios = load_from_str(FIXTURE);
        assert!(scenarios[0].metadata["sessions"].is_array());
    }

    #[test]
    fn evaluator_exact_match_passes() {
        let scenarios = load_from_str(FIXTURE);
        let result = LongMemEvalEvaluator.evaluate(&scenarios[0], "A systems language");
        assert!(result.passed);
        assert!((result.score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn evaluator_wrong_answer_fails() {
        let scenarios = load_from_str(FIXTURE);
        let result = LongMemEvalEvaluator.evaluate(&scenarios[0], "A web framework");
        assert!(!result.passed);
    }

    #[test]
    fn evaluator_partial_overlap_gives_token_f1_score() {
        let scenarios = load_from_str(FIXTURE);
        // "A systems framework" overlaps with "A systems language" on "systems"
        let result = LongMemEvalEvaluator.evaluate(&scenarios[0], "A systems framework");
        assert!(!result.passed);
        let expected_f1 = token_f1("A systems framework", "A systems language");
        assert!((result.score - expected_f1).abs() < f64::EPSILON);
    }

    #[test]
    fn evaluator_details_contain_token_f1() {
        let scenarios = load_from_str(FIXTURE);
        let result = LongMemEvalEvaluator.evaluate(&scenarios[0], "some answer");
        assert!(result.details.contains("token_f1="));
    }

    #[test]
    fn load_invalid_jsonl_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        assert!(LongMemEvalLoader.load(&path).is_err());
    }
}
