// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Experiment engine for adaptive agent behavior testing and hyperparameter tuning.

#[cfg(feature = "experiments")]
pub mod benchmark;
#[cfg(feature = "experiments")]
pub mod engine;
#[cfg(feature = "experiments")]
pub mod error;
#[cfg(feature = "experiments")]
pub mod evaluator;
#[cfg(feature = "experiments")]
pub mod generator;
#[cfg(feature = "experiments")]
pub mod grid;
#[cfg(feature = "experiments")]
pub mod neighborhood;
#[cfg(feature = "experiments")]
pub mod random;
#[cfg(feature = "experiments")]
pub mod search_space;
#[cfg(feature = "experiments")]
pub mod snapshot;
#[cfg(feature = "experiments")]
pub mod types;

#[cfg(feature = "experiments")]
pub use benchmark::{BenchmarkCase, BenchmarkSet};
#[cfg(feature = "experiments")]
pub use engine::{ExperimentEngine, ExperimentSessionReport};
#[cfg(feature = "experiments")]
pub use error::EvalError;
#[cfg(feature = "experiments")]
pub use evaluator::{CaseScore, EvalReport, Evaluator, JudgeOutput};
#[cfg(feature = "experiments")]
pub use generator::VariationGenerator;
#[cfg(feature = "experiments")]
pub use grid::GridStep;
#[cfg(feature = "experiments")]
pub use neighborhood::Neighborhood;
#[cfg(feature = "experiments")]
pub use random::Random;
#[cfg(feature = "experiments")]
pub use search_space::{ParameterRange, SearchSpace};
#[cfg(feature = "experiments")]
pub use snapshot::{ConfigSnapshot, GenerationOverrides};
#[cfg(feature = "experiments")]
pub use types::{ExperimentResult, ExperimentSource, ParameterKind, Variation, VariationValue};
