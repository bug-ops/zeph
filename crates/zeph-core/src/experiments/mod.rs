// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod benchmark;
pub mod error;
pub mod evaluator;
pub mod types;

pub use benchmark::{BenchmarkCase, BenchmarkSet};
pub use error::EvalError;
pub use evaluator::{CaseScore, EvalReport, Evaluator, JudgeOutput};
pub use types::{ExperimentResult, ExperimentSource, ParameterKind, Variation, VariationValue};
