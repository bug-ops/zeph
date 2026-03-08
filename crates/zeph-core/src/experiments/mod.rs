// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod benchmark;
pub mod error;
pub mod evaluator;
pub mod generator;
pub mod grid;
pub mod neighborhood;
pub mod random;
pub mod search_space;
pub mod snapshot;
pub mod types;

pub use benchmark::{BenchmarkCase, BenchmarkSet};
pub use error::EvalError;
pub use evaluator::{CaseScore, EvalReport, Evaluator, JudgeOutput};
pub use generator::VariationGenerator;
pub use grid::GridStep;
pub use neighborhood::Neighborhood;
pub use random::Random;
pub use search_space::{ParameterRange, SearchSpace};
pub use snapshot::{ConfigSnapshot, GenerationOverrides};
pub use types::{ExperimentResult, ExperimentSource, ParameterKind, Variation, VariationValue};
