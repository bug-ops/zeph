// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use crate::error::BenchError;

/// A single benchmark scenario loaded from a dataset file.
///
/// Each scenario represents one question/task that will be presented to the agent.
/// The `id` field is used to correlate agent responses with ground-truth answers and
/// to skip already-completed scenarios during a `--resume` run.
///
/// # Examples
///
/// ```
/// use zeph_bench::Scenario;
///
/// let scenario = Scenario {
///     id: "gaia_t42".into(),
///     prompt: "What is the boiling point of water in Celsius?".into(),
///     expected: "100".into(),
///     metadata: serde_json::json!({"level": 1}),
/// };
/// assert_eq!(scenario.id, "gaia_t42");
/// ```
#[derive(Debug, Clone)]
pub struct Scenario {
    /// Unique identifier within the dataset (e.g. `"frames_0"`, `"s1_2"`).
    pub id: String,
    /// The question or task text fed verbatim to the agent.
    pub prompt: String,
    /// The gold-standard answer used for scoring.
    pub expected: String,
    /// Dataset-specific extras such as difficulty level or `reasoning_types`.
    ///
    /// Set to [`serde_json::Value::Null`] when the dataset has no extra metadata.
    pub metadata: serde_json::Value,
}

/// Result of evaluating one agent response against the expected answer.
///
/// Produced by [`Evaluator::evaluate`]. The `score` is always in `0.0..=1.0`:
/// - `1.0` — perfect match (exact or token-level depending on the evaluator).
/// - `0.0` — no match.
/// - Intermediate values — partial token overlap (LOCOMO token-F1 evaluator).
///
/// # Examples
///
/// ```
/// use zeph_bench::EvalResult;
///
/// let result = EvalResult {
///     scenario_id: "s1".into(),
///     score: 0.75,
///     passed: true,
///     details: "token_f1=0.7500".into(),
/// };
/// assert!(result.passed);
/// ```
#[derive(Debug, Clone)]
pub struct EvalResult {
    /// ID of the scenario that produced this result.
    pub scenario_id: String,
    /// Numeric score in `0.0..=1.0`.
    pub score: f64,
    /// `true` when `score >= threshold` (threshold is evaluator-specific).
    pub passed: bool,
    /// Human-readable details such as `"token_f1=0.7500"` or `"exact_match=true"`.
    pub details: String,
}

/// Loads scenarios from a dataset file on disk.
///
/// Implement this trait to add support for a new dataset format. The harness
/// calls [`DatasetLoader::load`] once per run to materialise the full scenario
/// list before iterating.
///
/// Built-in implementations:
/// - [`crate::loaders::LocomoLoader`] — JSON array of sessions
/// - [`crate::loaders::FramesLoader`] — JSONL, one record per line
/// - [`crate::loaders::GaiaLoader`] — JSONL with optional level filter
pub trait DatasetLoader {
    /// Short identifier matching the dataset name in [`crate::DatasetRegistry`].
    fn name(&self) -> &'static str;

    /// Load all matching scenarios from `path`.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Io`] when the file cannot be opened or read, and
    /// [`BenchError::InvalidFormat`] when the file content cannot be parsed.
    fn load(&self, path: &Path) -> Result<Vec<Scenario>, BenchError>;
}

/// Scores one agent response against a [`Scenario`].
///
/// Each dataset loader ships a paired evaluator:
/// - [`crate::loaders::LocomoEvaluator`] — token F1 with threshold 0.5
/// - [`crate::loaders::FramesEvaluator`] — exact match (case-insensitive, punctuation stripped)
/// - [`crate::loaders::GaiaEvaluator`] — GAIA-normalized exact match (articles stripped)
pub trait Evaluator {
    /// Compute and return an [`EvalResult`] for the given `agent_response`.
    fn evaluate(&self, scenario: &Scenario, agent_response: &str) -> EvalResult;
}

/// Token F1 score: overlap of whitespace-split tokens between prediction and reference.
///
/// Splits both strings on whitespace, computes precision and recall over the
/// token-type intersection, then returns the harmonic mean (F1).
/// Returns `0.0` when either string is empty.
///
/// This metric is tolerant of minor wording differences and is used by the
/// LOCOMO evaluator.
///
/// # Examples
///
/// ```
/// use zeph_bench::token_f1;
///
/// // Perfect match.
/// assert!((token_f1("hello world", "hello world") - 1.0).abs() < f64::EPSILON);
///
/// // No overlap.
/// assert!(token_f1("foo bar", "baz qux") < f64::EPSILON);
///
/// // Partial overlap gives a value between 0 and 1.
/// let f1 = token_f1("the cat sat", "the cat ran");
/// assert!(f1 > 0.0 && f1 < 1.0);
///
/// // Empty strings return 0.
/// assert!(token_f1("", "hello") < f64::EPSILON);
/// ```
#[must_use]
pub fn token_f1(prediction: &str, reference: &str) -> f64 {
    let pred_tokens: std::collections::HashSet<&str> = prediction.split_whitespace().collect();
    let ref_tokens: std::collections::HashSet<&str> = reference.split_whitespace().collect();

    if pred_tokens.is_empty() || ref_tokens.is_empty() {
        return 0.0;
    }

    #[allow(clippy::cast_precision_loss)]
    let common = pred_tokens.intersection(&ref_tokens).count() as f64;
    #[allow(clippy::cast_precision_loss)]
    let precision = common / pred_tokens.len() as f64;
    #[allow(clippy::cast_precision_loss)]
    let recall = common / ref_tokens.len() as f64;

    if precision + recall == 0.0 {
        return 0.0;
    }

    2.0 * precision * recall / (precision + recall)
}

/// Exact match after lowercasing and stripping punctuation/whitespace.
///
/// Both strings are normalized by:
/// 1. Keeping only alphanumeric characters and whitespace.
/// 2. Converting to lowercase.
/// 3. Collapsing runs of whitespace to a single space.
///
/// Used by the FRAMES evaluator.
///
/// # Examples
///
/// ```
/// use zeph_bench::exact_match;
///
/// assert!(exact_match("Hello, World!", "hello world"));
/// assert!(exact_match("answer: YES.", "answer yes"));
/// assert!(!exact_match("foo", "bar"));
/// ```
#[must_use]
pub fn exact_match(prediction: &str, reference: &str) -> bool {
    normalize_basic(prediction) == normalize_basic(reference)
}

/// GAIA-normalized exact match: lowercase, strip articles, strip punctuation, collapse
/// whitespace, then compare.
///
/// Normalization steps (in order):
/// 1. Keep only alphanumeric characters and whitespace.
/// 2. Convert to lowercase.
/// 3. Remove the articles `a`, `an`, and `the`.
/// 4. Collapse whitespace and compare.
///
/// This matches the official GAIA leaderboard scoring script.
///
/// # Examples
///
/// ```
/// use zeph_bench::gaia_normalized_exact_match;
///
/// // Articles are stripped from both sides.
/// assert!(gaia_normalized_exact_match("The Tokyo", "Tokyo"));
/// assert!(gaia_normalized_exact_match("a cat sat on an apple", "cat sat on apple"));
///
/// // Different answers do not match.
/// assert!(!gaia_normalized_exact_match("1944", "1945"));
/// ```
#[must_use]
pub fn gaia_normalized_exact_match(prediction: &str, reference: &str) -> bool {
    normalize_gaia(prediction) == normalize_gaia(reference)
}

fn normalize_basic(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_gaia(s: &str) -> String {
    const ARTICLES: &[&str] = &["a", "an", "the"];

    let stripped = s
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase();

    stripped
        .split_whitespace()
        .filter(|tok| !ARTICLES.contains(tok))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_f1_identical() {
        assert!((token_f1("hello world", "hello world") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn token_f1_no_overlap() {
        assert!(token_f1("foo bar", "baz qux") < f64::EPSILON);
    }

    #[test]
    fn token_f1_partial_overlap() {
        let f1 = token_f1("hello world foo", "hello world bar");
        assert!(f1 > 0.0 && f1 < 1.0);
    }

    #[test]
    fn token_f1_empty_prediction() {
        assert!(token_f1("", "hello") < f64::EPSILON);
    }

    #[test]
    fn token_f1_empty_reference() {
        assert!(token_f1("hello", "") < f64::EPSILON);
    }

    #[test]
    fn exact_match_identical() {
        assert!(exact_match("Hello, World!", "hello world"));
    }

    #[test]
    fn exact_match_differs() {
        assert!(!exact_match("foo", "bar"));
    }

    #[test]
    fn exact_match_strips_punctuation() {
        assert!(exact_match("answer: yes.", "answer yes"));
    }

    #[test]
    fn gaia_normalized_strips_articles() {
        assert!(gaia_normalized_exact_match(
            "The quick brown fox",
            "quick brown fox"
        ));
    }

    #[test]
    fn gaia_normalized_strips_a_an() {
        assert!(gaia_normalized_exact_match(
            "a cat sat on an apple",
            "cat sat on apple"
        ));
    }

    #[test]
    fn gaia_normalized_differs() {
        assert!(!gaia_normalized_exact_match("cat", "dog"));
    }
}
