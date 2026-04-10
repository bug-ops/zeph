// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for the experiments module.

/// Errors that can occur during experiment evaluation, benchmark loading, or persistence.
///
/// Most variants carry structured context (file paths, token counts, parameter names)
/// so that callers can surface actionable diagnostics to the user.
///
/// # Examples
///
/// ```rust
/// use zeph_experiments::EvalError;
///
/// let err = EvalError::BudgetExceeded { used: 1_500, budget: 1_000 };
/// assert!(err.to_string().contains("1500"));
///
/// let err = EvalError::InvalidRadius { radius: -1.0 };
/// assert!(err.to_string().contains("finite and positive"));
/// ```
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// The benchmark TOML file could not be opened or read.
    #[error("failed to load benchmark file {0}: {1}")]
    BenchmarkLoad(String, #[source] std::io::Error),

    /// The benchmark TOML file could not be parsed.
    #[error("failed to parse benchmark file {0}: {1}")]
    BenchmarkParse(String, String),

    /// [`BenchmarkSet::validate`] was called on an empty `cases` vec.
    ///
    /// [`BenchmarkSet::validate`]: crate::BenchmarkSet::validate
    #[error("benchmark set is empty")]
    EmptyBenchmarkSet,

    /// The cumulative token budget for judge calls was exhausted.
    ///
    /// When this error is returned from [`Evaluator::evaluate`], the report will
    /// have `is_partial = true` and only include cases scored before the budget was hit.
    ///
    /// [`Evaluator::evaluate`]: crate::Evaluator::evaluate
    #[error("evaluation budget exceeded: used {used} of {budget} tokens")]
    BudgetExceeded { used: u64, budget: u64 },

    /// An LLM call failed (network, auth, timeout, or API error).
    #[error("LLM error during evaluation: {0}")]
    Llm(#[from] zeph_llm::LlmError),

    /// The judge model returned a non-finite or structurally invalid score.
    #[error("judge output parse failed for case {case_index}: {detail}")]
    JudgeParse {
        /// Zero-based index of the benchmark case that produced the invalid output.
        case_index: usize,
        /// Description of the parse failure (e.g., `"non-finite score: NaN"`).
        detail: String,
    },

    /// The internal tokio semaphore used for concurrency control was closed.
    ///
    /// This is an internal invariant violation and should never occur in normal usage.
    #[error("semaphore acquire failed: {0}")]
    Semaphore(String),

    /// The benchmark file exceeds the 10 MiB size limit.
    #[error("benchmark file exceeds size limit ({size} bytes > {limit} bytes): {path}")]
    BenchmarkTooLarge {
        /// Canonicalized path of the file.
        path: String,
        /// Actual file size in bytes.
        size: u64,
        /// Maximum allowed size in bytes (currently 10 MiB).
        limit: u64,
    },

    /// The benchmark file's canonical path escaped the expected parent directory.
    ///
    /// This indicates a symlink traversal attack and is rejected before any file I/O.
    #[error("benchmark file path escapes allowed directory: {0}")]
    PathTraversal(String),

    /// A parameter value was outside its declared `[min, max]` range.
    #[error("parameter out of range: {kind} value {value} not in [{min}, {max}]")]
    OutOfRange {
        /// Parameter name (e.g., `"temperature"`).
        kind: String,
        /// The value that was rejected.
        value: f64,
        /// Minimum allowed value (inclusive).
        min: f64,
        /// Maximum allowed value (inclusive).
        max: f64,
    },

    /// All variations in the generator's search space have been visited.
    #[error("search space exhausted: all variations in {strategy} have been visited")]
    SearchSpaceExhausted {
        /// Name of the strategy that exhausted (e.g., `"grid"`).
        strategy: &'static str,
    },

    /// The [`Neighborhood`] radius was not finite and positive.
    ///
    /// [`Neighborhood`]: crate::Neighborhood
    #[error("invalid neighborhood radius {radius}: must be finite and positive")]
    InvalidRadius {
        /// The invalid radius value that was rejected.
        radius: f64,
    },

    /// An experiment result could not be persisted to SQLite.
    #[error("experiment storage error: {0}")]
    Storage(String),
}
