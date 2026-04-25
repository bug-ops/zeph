// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{io::BufReader, path::Path};

use serde::Deserialize;

use crate::{
    error::BenchError,
    scenario::{DatasetLoader, EvalResult, Evaluator, Scenario, exact_match},
};

#[derive(Debug, Deserialize)]
struct TauBenchTask {
    task_id: String,
    instruction: String,
    expected_actions: Vec<String>,
    ground_truth: String,
    domain: String,
}

/// Loads tau-bench scenarios from a JSON array file.
///
/// **Source**: [`sierra-research/tau-bench`](https://github.com/sierra-research/tau-bench)
/// on GitHub.
///
/// **Schema**: JSON array of task objects:
/// ```json
/// [{"task_id":"retail_001","instruction":"...","expected_actions":["search_product"],"ground_truth":"...","domain":"retail"}]
/// ```
///
/// Each task becomes one [`Scenario`]:
/// - `id` — `task_id`
/// - `prompt` — `instruction`
/// - `expected` — `ground_truth`
/// - `metadata` — `{"domain": ..., "expected_actions": [...]}`
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use zeph_bench::loaders::TauBenchLoader;
/// use zeph_bench::scenario::DatasetLoader;
///
/// let scenarios = TauBenchLoader.load(Path::new("/data/tau_bench.json")).unwrap();
/// println!("loaded {} scenarios", scenarios.len());
/// ```
#[derive(Debug)]
pub struct TauBenchLoader;

impl DatasetLoader for TauBenchLoader {
    fn name(&self) -> &'static str {
        "tau-bench"
    }

    /// # Errors
    ///
    /// Returns [`BenchError::Io`] when the file cannot be read and
    /// [`BenchError::InvalidFormat`] when JSON parsing fails.
    fn load(&self, path: &Path) -> Result<Vec<Scenario>, BenchError> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let tasks: Vec<TauBenchTask> = serde_json::from_reader(reader)
            .map_err(|e| BenchError::InvalidFormat(e.to_string()))?;

        let scenarios = tasks
            .into_iter()
            .map(|t| {
                Scenario::single(
                    t.task_id,
                    t.instruction,
                    t.ground_truth,
                    serde_json::json!({
                        "domain": t.domain,
                        "expected_actions": t.expected_actions,
                    }),
                )
            })
            .collect();
        Ok(scenarios)
    }
}

/// Evaluates tau-bench responses using binary exact match.
///
/// - `passed` = `exact_match(agent_response, ground_truth)`
/// - `score` = `1.0` if passed, `0.0` otherwise
/// - `details` = `"task_completion=true/false"`
///
/// # Examples
///
/// ```
/// use zeph_bench::{Scenario, loaders::TauBenchEvaluator};
/// use zeph_bench::scenario::Evaluator;
///
/// let scenario = Scenario::single("retail_001", "Find a product", "found item XYZ", serde_json::Value::Null);
///
/// let result = TauBenchEvaluator.evaluate(&scenario, "found item XYZ");
/// assert!(result.passed);
/// assert!((result.score - 1.0).abs() < f64::EPSILON);
/// ```
#[derive(Debug)]
pub struct TauBenchEvaluator;

impl Evaluator for TauBenchEvaluator {
    fn evaluate(&self, scenario: &Scenario, agent_response: &str) -> EvalResult {
        let passed = exact_match(agent_response, &scenario.expected);
        EvalResult {
            scenario_id: scenario.id.clone(),
            score: if passed { 1.0 } else { 0.0 },
            passed,
            details: format!("task_completion={}", if passed { "true" } else { "false" }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"[
        {
            "task_id": "retail_001",
            "instruction": "Find product X",
            "expected_actions": ["search", "select"],
            "ground_truth": "Product X found",
            "domain": "retail"
        },
        {
            "task_id": "airline_002",
            "instruction": "Book flight to Paris",
            "expected_actions": ["search_flight", "book"],
            "ground_truth": "Flight booked",
            "domain": "airline"
        }
    ]"#;

    fn load_from_str(json: &str) -> Vec<Scenario> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tau_bench.json");
        std::fs::write(&path, json).unwrap();
        TauBenchLoader.load(&path).unwrap()
    }

    #[test]
    fn load_parses_scenario_count() {
        assert_eq!(load_from_str(FIXTURE).len(), 2);
    }

    #[test]
    fn load_builds_correct_ids() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios[0].id, "retail_001");
        assert_eq!(scenarios[1].id, "airline_002");
    }

    #[test]
    fn load_maps_prompt_and_expected() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios[0].primary_prompt().unwrap(), "Find product X");
        assert_eq!(scenarios[0].expected, "Product X found");
    }

    #[test]
    fn load_stores_domain_in_metadata() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios[0].metadata["domain"], "retail");
        assert_eq!(scenarios[1].metadata["domain"], "airline");
    }

    #[test]
    fn load_stores_expected_actions_in_metadata() {
        let scenarios = load_from_str(FIXTURE);
        assert!(scenarios[0].metadata["expected_actions"].is_array());
    }

    #[test]
    fn evaluator_exact_match_passes() {
        let scenarios = load_from_str(FIXTURE);
        let result = TauBenchEvaluator.evaluate(&scenarios[0], "Product X found");
        assert!(result.passed);
        assert!((result.score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn evaluator_wrong_answer_fails() {
        let scenarios = load_from_str(FIXTURE);
        let result = TauBenchEvaluator.evaluate(&scenarios[0], "Product not found");
        assert!(!result.passed);
        assert!(result.score < f64::EPSILON);
    }

    #[test]
    fn evaluator_details_format() {
        let scenarios = load_from_str(FIXTURE);
        let pass_result = TauBenchEvaluator.evaluate(&scenarios[0], "Product X found");
        assert_eq!(pass_result.details, "task_completion=true");

        let fail_result = TauBenchEvaluator.evaluate(&scenarios[0], "wrong answer");
        assert_eq!(fail_result.details, "task_completion=false");
    }

    #[test]
    fn load_invalid_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(TauBenchLoader.load(&path).is_err());
    }
}
