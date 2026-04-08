// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use crate::error::BenchError;

/// A single benchmark scenario loaded from any dataset.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub id: String,
    /// The question or task fed to the agent.
    pub prompt: String,
    /// The gold answer for evaluation.
    pub expected: String,
    /// Dataset-specific extras (level, `reasoning_types`, etc.).
    pub metadata: serde_json::Value,
}

/// Result of evaluating one agent response against the expected answer.
#[derive(Debug, Clone)]
pub struct EvalResult {
    pub scenario_id: String,
    /// Score in the range 0.0–1.0.
    pub score: f64,
    /// True when `score >= threshold`.
    pub passed: bool,
    pub details: String,
}

/// Loads scenarios from a dataset file.
pub trait DatasetLoader {
    fn name(&self) -> &'static str;

    /// # Errors
    ///
    /// Returns [`BenchError`] when the file cannot be read or parsed.
    fn load(&self, path: &Path) -> Result<Vec<Scenario>, BenchError>;
}

/// Scores an agent response against a scenario.
pub trait Evaluator {
    fn evaluate(&self, scenario: &Scenario, agent_response: &str) -> EvalResult;
}

/// Token F1 score: overlap of whitespace-split tokens between prediction and reference.
///
/// Returns a value in `0.0..=1.0`. Returns `0.0` when either string is empty.
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
#[must_use]
pub fn exact_match(prediction: &str, reference: &str) -> bool {
    normalize_basic(prediction) == normalize_basic(reference)
}

/// GAIA-normalized exact match: lowercase, strip articles, strip punctuation, collapse
/// whitespace, then compare.
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
