// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use crate::error::BenchError;

/// Role of a turn in a multi-turn scenario conversation.
///
/// # Examples
///
/// ```
/// use zeph_bench::scenario::Role;
///
/// assert!(matches!(Role::User, Role::User));
/// assert!(matches!(Role::Assistant, Role::Assistant));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// A message from the human user.
    User,
    /// A message from the AI assistant.
    Assistant,
}

/// One turn in a multi-turn scenario conversation.
///
/// # Examples
///
/// ```
/// use zeph_bench::scenario::{Role, Turn};
///
/// let turn = Turn { role: Role::User, content: "What is the capital of France?".into() };
/// assert!(matches!(turn.role, Role::User));
/// ```
#[derive(Debug, Clone)]
pub struct Turn {
    /// Who authored this turn.
    pub role: Role,
    /// Text content of the turn.
    pub content: String,
}

/// A single benchmark scenario loaded from a dataset file.
///
/// Each scenario represents one question/task that will be presented to the agent.
/// The `id` field is used to correlate agent responses with ground-truth answers and
/// to skip already-completed scenarios during a `--resume` run.
///
/// Construct via [`Scenario::single`] for single-turn scenarios (all built-in loaders),
/// or push [`Turn`]s directly into [`Scenario::turns`] for multi-turn scenarios.
///
/// # Examples
///
/// ```
/// use zeph_bench::Scenario;
///
/// let scenario = Scenario::single(
///     "gaia_t42",
///     "What is the boiling point of water in Celsius?",
///     "100",
///     serde_json::json!({"level": 1}),
/// );
/// assert_eq!(scenario.id, "gaia_t42");
/// assert_eq!(scenario.primary_prompt().unwrap(), "What is the boiling point of water in Celsius?");
/// ```
#[derive(Debug, Clone)]
pub struct Scenario {
    /// Unique identifier within the dataset (e.g. `"frames_0"`, `"s1_2"`).
    pub id: String,
    /// Ordered turns in this scenario. Non-empty by contract of [`Scenario::single`].
    ///
    /// Direct construction is allowed for multi-turn scenarios; callers must ensure
    /// at least one [`Role::User`] turn is present before calling [`Scenario::primary_prompt`].
    pub turns: Vec<Turn>,
    /// The gold-standard answer used for scoring.
    pub expected: String,
    /// Dataset-specific extras such as difficulty level or `reasoning_types`.
    ///
    /// Set to [`serde_json::Value::Null`] when the dataset has no extra metadata.
    pub metadata: serde_json::Value,
}

impl Scenario {
    /// Convenience constructor for single-turn scenarios.
    ///
    /// Wraps `prompt` in a one-element [`Vec<Turn>`] with [`Role::User`]. All built-in
    /// dataset loaders use this constructor.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_bench::Scenario;
    ///
    /// let s = Scenario::single("id1", "What year?", "2026", serde_json::Value::Null);
    /// assert_eq!(s.primary_prompt().unwrap(), "What year?");
    /// ```
    #[must_use]
    pub fn single(
        id: impl Into<String>,
        prompt: impl Into<String>,
        expected: impl Into<String>,
        metadata: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            turns: vec![Turn {
                role: Role::User,
                content: prompt.into(),
            }],
            expected: expected.into(),
            metadata,
        }
    }

    /// Returns the content of the first [`Role::User`] turn.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::InvalidFormat`] when `turns` is empty or contains no
    /// [`Role::User`] entry. Loaders must construct via [`Scenario::single`] or push
    /// at least one user turn.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_bench::Scenario;
    ///
    /// let s = Scenario::single("id1", "hello", "world", serde_json::Value::Null);
    /// assert_eq!(s.primary_prompt().unwrap(), "hello");
    /// ```
    pub fn primary_prompt(&self) -> Result<&str, BenchError> {
        self.turns
            .iter()
            .find(|t| matches!(t.role, Role::User))
            .map(|t| t.content.as_str())
            .ok_or_else(|| {
                BenchError::InvalidFormat(format!("scenario '{}' has no user turn", self.id))
            })
    }
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

    // Map Unicode subscript/superscript digits to their ASCII equivalents before
    // stripping — this ensures "H₂O" and "H2O" normalize identically.
    let ascii_mapped: String = s.chars().map(ascii_fold_digit).collect();

    let stripped = ascii_mapped
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

/// Map Unicode subscript and superscript digit characters to their ASCII equivalents.
///
/// Returns the character unchanged if it is not a subscript/superscript digit.
fn ascii_fold_digit(c: char) -> char {
    match c {
        '\u{2080}' | '\u{2070}' => '0',
        '\u{2081}' | '\u{00B9}' => '1',
        '\u{2082}' | '\u{00B2}' => '2',
        '\u{2083}' | '\u{00B3}' => '3',
        '\u{2084}' | '\u{2074}' => '4',
        '\u{2085}' | '\u{2075}' => '5',
        '\u{2086}' | '\u{2076}' => '6',
        '\u{2087}' | '\u{2077}' => '7',
        '\u{2088}' | '\u{2078}' => '8',
        '\u{2089}' | '\u{2079}' => '9',
        other => other,
    }
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

    #[test]
    fn gaia_normalized_subscript_digits_match_ascii() {
        // Model may respond with Unicode subscript "H₂O" — must match ASCII "H2O".
        assert!(gaia_normalized_exact_match("H\u{2082}O", "H2O"));
    }

    #[test]
    fn single_constructs_one_user_turn() {
        let s = Scenario::single("id1", "hello", "world", serde_json::Value::Null);
        assert_eq!(s.turns.len(), 1);
        assert!(matches!(s.turns[0].role, Role::User));
        assert_eq!(s.turns[0].content, "hello");
        assert_eq!(s.expected, "world");
    }

    #[test]
    fn primary_prompt_returns_first_user_turn_content() {
        let s = Scenario::single("id1", "What year?", "2026", serde_json::Value::Null);
        assert_eq!(s.primary_prompt().unwrap(), "What year?");
    }

    #[test]
    fn primary_prompt_skips_leading_assistant_turns() {
        let s = Scenario {
            id: "id2".into(),
            turns: vec![
                Turn {
                    role: Role::Assistant,
                    content: "I am ready.".into(),
                },
                Turn {
                    role: Role::User,
                    content: "What is Rust?".into(),
                },
            ],
            expected: "A systems language".into(),
            metadata: serde_json::Value::Null,
        };
        assert_eq!(s.primary_prompt().unwrap(), "What is Rust?");
    }

    #[test]
    fn primary_prompt_errors_when_no_user_turn() {
        let s = Scenario {
            id: "id3".into(),
            turns: vec![Turn {
                role: Role::Assistant,
                content: "assistant only".into(),
            }],
            expected: String::new(),
            metadata: serde_json::Value::Null,
        };
        assert!(s.primary_prompt().is_err());

        let empty = Scenario {
            id: "id4".into(),
            turns: vec![],
            expected: String::new(),
            metadata: serde_json::Value::Null,
        };
        assert!(empty.primary_prompt().is_err());
    }
}
