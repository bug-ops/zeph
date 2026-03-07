// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Autonomous experiments engine — Phase 2: Benchmark Dataset & LLM-as-Judge.
//!
//! This module provides the types and evaluator needed to run structured benchmark
//! evaluations against LLM models. See [`Evaluator`] for the entry point.

pub mod benchmark;
pub mod error;
pub mod evaluator;

pub use benchmark::{BenchmarkCase, BenchmarkSet};
pub use error::EvalError;
pub use evaluator::{CaseScore, EvalReport, Evaluator, JudgeOutput};
