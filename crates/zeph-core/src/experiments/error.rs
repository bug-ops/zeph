// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for the experiments module.

/// Errors that can occur during benchmark evaluation.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("failed to load benchmark file {0}: {1}")]
    BenchmarkLoad(String, #[source] std::io::Error),

    #[error("failed to parse benchmark file {0}: {1}")]
    BenchmarkParse(String, String),

    #[error("benchmark set is empty")]
    EmptyBenchmarkSet,

    #[error("evaluation budget exceeded: used {used} of {budget} tokens")]
    BudgetExceeded { used: u64, budget: u64 },

    #[error("LLM error during evaluation: {0}")]
    Llm(#[from] zeph_llm::LlmError),

    #[error("judge output parse failed for case {case_index}: {detail}")]
    JudgeParse { case_index: usize, detail: String },

    #[error("semaphore acquire failed: {0}")]
    Semaphore(String),

    #[error("benchmark file exceeds size limit ({size} bytes > {limit} bytes): {path}")]
    BenchmarkTooLarge { path: String, size: u64, limit: u64 },

    #[error("benchmark file path escapes allowed directory: {0}")]
    PathTraversal(String),

    #[error("parameter out of range: {kind} value {value} not in [{min}, {max}]")]
    OutOfRange {
        kind: String,
        value: f64,
        min: f64,
        max: f64,
    },

    #[error("search space exhausted: all variations in {strategy} have been visited")]
    SearchSpaceExhausted { strategy: &'static str },

    #[error("invalid neighborhood radius {radius}: must be finite and positive")]
    InvalidRadius { radius: f64 },

    #[error("experiment storage error: {0}")]
    Storage(String),
}
