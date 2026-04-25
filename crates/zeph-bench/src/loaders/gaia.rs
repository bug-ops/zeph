// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    io::{BufRead as _, BufReader},
    path::Path,
};

use serde::Deserialize;

use crate::{
    error::BenchError,
    scenario::{DatasetLoader, EvalResult, Evaluator, Scenario, gaia_normalized_exact_match},
};

#[derive(Debug, Deserialize)]
struct GaiaRecord {
    task_id: String,
    #[serde(rename = "Question")]
    question: String,
    #[serde(rename = "Level")]
    level: u8,
    #[serde(rename = "Final answer")]
    final_answer: String,
    #[serde(rename = "Annotator Metadata")]
    annotator_metadata: Option<serde_json::Value>,
}

/// Loads GAIA benchmark scenarios from a JSONL file with an optional level filter.
///
/// **Source**: [`gaia-benchmark/GAIA`](https://huggingface.co/datasets/gaia-benchmark/GAIA)
/// on `HuggingFace`.
///
/// **Schema**: one JSON object per line:
/// ```json
/// {
///   "task_id": "...",
///   "Question": "...",
///   "Level": 1,
///   "Final answer": "...",
///   "Annotator Metadata": { ... }
/// }
/// ```
///
/// Scenarios are mapped as:
/// - `id` — `task_id`.
/// - `prompt` — `Question`.
/// - `expected` — `Final answer`.
/// - `metadata` — `{"level": N, "annotator_metadata": {...}}`.
///
/// When [`level`][GaiaLoader::level] is `Some(n)`, only lines whose `Level` field
/// equals `n` are returned.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use zeph_bench::loaders::GaiaLoader;
/// use zeph_bench::scenario::DatasetLoader;
///
/// // Load all levels.
/// let all = GaiaLoader::all_levels().load(Path::new("/data/gaia.jsonl")).unwrap();
///
/// // Load only level-1 tasks.
/// let easy = GaiaLoader::with_level(1).load(Path::new("/data/gaia.jsonl")).unwrap();
/// assert!(easy.len() <= all.len());
/// ```
#[derive(Debug)]
pub struct GaiaLoader {
    /// Optional level filter. When `Some(n)`, only scenarios where `Level == n` are loaded.
    pub level: Option<u8>,
}

impl GaiaLoader {
    /// Create a loader that loads scenarios from all difficulty levels.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_bench::loaders::GaiaLoader;
    ///
    /// let loader = GaiaLoader::all_levels();
    /// assert!(loader.level.is_none());
    /// ```
    #[must_use]
    pub fn all_levels() -> Self {
        Self { level: None }
    }

    /// Create a loader that only loads scenarios whose `Level` field equals `level`.
    ///
    /// GAIA levels run from 1 (easy) to 3 (hard).
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_bench::loaders::GaiaLoader;
    ///
    /// let loader = GaiaLoader::with_level(2);
    /// assert_eq!(loader.level, Some(2));
    /// ```
    #[must_use]
    pub fn with_level(level: u8) -> Self {
        Self { level: Some(level) }
    }
}

impl DatasetLoader for GaiaLoader {
    fn name(&self) -> &'static str {
        "gaia"
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
            let record: GaiaRecord = serde_json::from_str(trimmed)
                .map_err(|e| BenchError::InvalidFormat(format!("line {line_number}: {e}")))?;

            if let Some(filter_level) = self.level
                && record.level != filter_level
            {
                continue;
            }

            let metadata = serde_json::json!({
                "level": record.level,
                "annotator_metadata": record.annotator_metadata,
            });

            scenarios.push(Scenario::single(
                record.task_id,
                record.question,
                record.final_answer,
                metadata,
            ));
        }
        Ok(scenarios)
    }
}

/// Evaluates GAIA responses using GAIA-normalized exact match.
///
/// Normalization (applied to both prediction and reference):
/// 1. Keep only alphanumeric characters and whitespace.
/// 2. Convert to lowercase.
/// 3. Remove the articles `a`, `an`, and `the`.
/// 4. Collapse whitespace.
///
/// This matches the official GAIA leaderboard evaluation script.
/// Score is `1.0` on match, `0.0` otherwise.
///
/// # Examples
///
/// ```
/// use zeph_bench::{Scenario, loaders::GaiaEvaluator};
/// use zeph_bench::scenario::Evaluator;
///
/// let scenario = Scenario::single("t1", "Capital of Japan?", "Tokyo", serde_json::json!({"level": 1}));
///
/// // Article "The" is stripped before comparison.
/// assert!(GaiaEvaluator.evaluate(&scenario, "The Tokyo").passed);
/// assert!(!GaiaEvaluator.evaluate(&scenario, "Osaka").passed);
/// ```
#[derive(Debug)]
pub struct GaiaEvaluator;

impl Evaluator for GaiaEvaluator {
    fn evaluate(&self, scenario: &Scenario, agent_response: &str) -> EvalResult {
        let passed = gaia_normalized_exact_match(agent_response, &scenario.expected);
        EvalResult {
            scenario_id: scenario.id.clone(),
            score: if passed { 1.0 } else { 0.0 },
            passed,
            details: format!(
                "gaia_normalized_exact_match={}",
                if passed { "true" } else { "false" }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"task_id": "t1", "Question": "What year did WWII end?", "Level": 1, "Final answer": "1945", "Annotator Metadata": {"difficulty": "easy"}}
{"task_id": "t2", "Question": "Who wrote Hamlet?", "Level": 2, "Final answer": "Shakespeare", "Annotator Metadata": null}
{"task_id": "t3", "Question": "Capital of Japan?", "Level": 1, "Final answer": "Tokyo", "Annotator Metadata": null}
"#;

    fn load_from_str(jsonl: &str, level: Option<u8>) -> Vec<Scenario> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gaia.jsonl");
        std::fs::write(&path, jsonl).unwrap();
        GaiaLoader { level }.load(&path).unwrap()
    }

    #[test]
    fn load_all_levels_parses_scenario_count() {
        let scenarios = load_from_str(FIXTURE, None);
        assert_eq!(scenarios.len(), 3);
    }

    #[test]
    fn load_filters_by_level() {
        let scenarios = load_from_str(FIXTURE, Some(1));
        assert_eq!(scenarios.len(), 2);
        for s in &scenarios {
            assert_eq!(s.metadata["level"], 1);
        }
    }

    #[test]
    fn load_maps_task_id_to_scenario_id() {
        let scenarios = load_from_str(FIXTURE, None);
        assert_eq!(scenarios[0].id, "t1");
        assert_eq!(scenarios[1].id, "t2");
    }

    #[test]
    fn load_maps_prompt_and_expected() {
        let scenarios = load_from_str(FIXTURE, None);
        assert_eq!(
            scenarios[0].primary_prompt().unwrap(),
            "What year did WWII end?"
        );
        assert_eq!(scenarios[0].expected, "1945");
    }

    #[test]
    fn load_stores_level_in_metadata() {
        let scenarios = load_from_str(FIXTURE, None);
        assert_eq!(scenarios[1].metadata["level"], 2);
    }

    #[test]
    fn evaluator_normalized_match_passes() {
        let scenarios = load_from_str(FIXTURE, None);
        // "The 1945" should match "1945" after stripping article and comparing
        let result = GaiaEvaluator.evaluate(&scenarios[0], "1945");
        assert!(result.passed);
    }

    #[test]
    fn evaluator_wrong_answer_fails() {
        let scenarios = load_from_str(FIXTURE, None);
        let result = GaiaEvaluator.evaluate(&scenarios[0], "1944");
        assert!(!result.passed);
        assert!(result.score < f64::EPSILON);
    }

    #[test]
    fn evaluator_strips_article_the() {
        let scenarios = load_from_str(FIXTURE, None);
        // scenario[2]: expected = "Tokyo"
        let result = GaiaEvaluator.evaluate(&scenarios[2], "The Tokyo");
        assert!(result.passed);
    }

    #[test]
    fn load_invalid_jsonl_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        assert!(GaiaLoader::all_levels().load(&path).is_err());
    }

    #[test]
    fn all_levels_constructor() {
        let loader = GaiaLoader::all_levels();
        assert!(loader.level.is_none());
    }

    #[test]
    fn with_level_constructor() {
        let loader = GaiaLoader::with_level(2);
        assert_eq!(loader.level, Some(2));
    }
}
