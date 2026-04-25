// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    io::{BufRead as _, BufReader},
    path::Path,
};

use serde::Deserialize;

use crate::{
    error::BenchError,
    scenario::{DatasetLoader, EvalResult, Evaluator, Scenario, exact_match},
};

#[derive(Debug, Deserialize)]
struct FramesRecord {
    #[serde(rename = "Prompt")]
    prompt: String,
    #[serde(rename = "Answer")]
    answer: String,
    reasoning_types: Option<serde_json::Value>,
}

/// Loads FRAMES benchmark scenarios from a JSONL file.
///
/// **Source**: [`google/frames-benchmark`](https://huggingface.co/datasets/google/frames-benchmark)
/// on `HuggingFace`.
///
/// **Schema**: one JSON object per line:
/// ```json
/// {"Prompt": "...", "Answer": "...", "reasoning_types": [...], "wiki_links": [...]}
/// ```
///
/// Each non-empty line becomes one [`Scenario`]:
/// - `id` — `"frames_{line_number}"` (zero-based, counting from the first line of the file).
/// - `prompt` — value of `"Prompt"`.
/// - `expected` — value of `"Answer"`.
/// - `metadata` — value of `"reasoning_types"` (array of strings, or `null`).
///
/// Empty lines are skipped. Unknown fields (e.g. `"wiki_links"`) are ignored.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use zeph_bench::loaders::FramesLoader;
/// use zeph_bench::scenario::DatasetLoader;
///
/// let scenarios = FramesLoader.load(Path::new("/data/frames.jsonl")).unwrap();
/// println!("loaded {} scenarios", scenarios.len());
/// ```
#[derive(Debug)]
pub struct FramesLoader;

impl DatasetLoader for FramesLoader {
    fn name(&self) -> &'static str {
        "frames"
    }

    /// # Errors
    ///
    /// Returns [`BenchError::Io`] when the file cannot be read and
    /// [`BenchError::InvalidFormat`] when a JSONL line cannot be parsed.
    fn load(&self, path: &Path) -> Result<Vec<Scenario>, BenchError> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);

        let mut scenarios = Vec::new();
        for (line_number, line) in reader.lines().enumerate() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let record: FramesRecord = serde_json::from_str(trimmed)
                .map_err(|e| BenchError::InvalidFormat(format!("line {line_number}: {e}")))?;

            let metadata = record.reasoning_types.unwrap_or(serde_json::Value::Null);

            scenarios.push(Scenario::single(
                format!("frames_{line_number}"),
                record.prompt,
                record.answer,
                metadata,
            ));
        }
        Ok(scenarios)
    }
}

/// Evaluates FRAMES responses using case-insensitive exact match.
///
/// Normalization (applied to both prediction and reference before comparison):
/// 1. Keep only alphanumeric characters and whitespace.
/// 2. Convert to lowercase.
/// 3. Collapse runs of whitespace.
///
/// Score is `1.0` when the normalized strings match, `0.0` otherwise.
///
/// # Examples
///
/// ```
/// use zeph_bench::{Scenario, loaders::FramesEvaluator};
/// use zeph_bench::scenario::Evaluator;
///
/// let scenario = Scenario::single("frames_0", "Capital of France?", "Paris", serde_json::Value::Null);
///
/// // Case-insensitive and punctuation-stripped.
/// assert!(FramesEvaluator.evaluate(&scenario, "paris").passed);
/// assert!(FramesEvaluator.evaluate(&scenario, "Paris!").passed);
/// assert!(!FramesEvaluator.evaluate(&scenario, "London").passed);
/// ```
#[derive(Debug)]
pub struct FramesEvaluator;

impl Evaluator for FramesEvaluator {
    fn evaluate(&self, scenario: &Scenario, agent_response: &str) -> EvalResult {
        let passed = exact_match(agent_response, &scenario.expected);
        EvalResult {
            scenario_id: scenario.id.clone(),
            score: if passed { 1.0 } else { 0.0 },
            passed,
            details: format!("exact_match={}", if passed { "true" } else { "false" }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"Prompt": "What is 2+2?", "Answer": "4", "reasoning_types": ["math"], "wiki_links": []}
{"Prompt": "Capital of France?", "Answer": "Paris", "reasoning_types": ["geography"]}
"#;

    fn load_from_str(jsonl: &str) -> Vec<Scenario> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frames.jsonl");
        std::fs::write(&path, jsonl).unwrap();
        FramesLoader.load(&path).unwrap()
    }

    #[test]
    fn load_parses_scenario_count() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios.len(), 2);
    }

    #[test]
    fn load_builds_correct_ids() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios[0].id, "frames_0");
        assert_eq!(scenarios[1].id, "frames_1");
    }

    #[test]
    fn load_maps_prompt_and_expected() {
        let scenarios = load_from_str(FIXTURE);
        assert_eq!(scenarios[0].primary_prompt().unwrap(), "What is 2+2?");
        assert_eq!(scenarios[0].expected, "4");
    }

    #[test]
    fn load_stores_reasoning_types_in_metadata() {
        let scenarios = load_from_str(FIXTURE);
        assert!(scenarios[0].metadata.is_array());
    }

    #[test]
    fn evaluator_exact_match_passes() {
        let scenarios = load_from_str(FIXTURE);
        let result = FramesEvaluator.evaluate(&scenarios[0], "4");
        assert!(result.passed);
        assert!((result.score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn evaluator_wrong_answer_fails() {
        let scenarios = load_from_str(FIXTURE);
        let result = FramesEvaluator.evaluate(&scenarios[0], "5");
        assert!(!result.passed);
        assert!(result.score < f64::EPSILON);
    }

    #[test]
    fn evaluator_case_insensitive_match() {
        let scenarios = load_from_str(FIXTURE);
        let result = FramesEvaluator.evaluate(&scenarios[1], "paris");
        assert!(result.passed);
    }

    #[test]
    fn load_invalid_jsonl_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        assert!(FramesLoader.load(&path).is_err());
    }
}
