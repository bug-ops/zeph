// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod channel;
pub mod cli;
pub mod dataset;
pub mod deterministic;
pub mod error;
pub mod loaders;
pub mod results;
pub mod scenario;

pub use channel::BenchmarkChannel;
pub use cli::BenchCommand;
pub use dataset::{DatasetFormat, DatasetMeta, DatasetRegistry};
pub use deterministic::apply_deterministic_overrides;
pub use error::BenchError;
pub use results::{Aggregate, BenchRun, ResultWriter, RunStatus, ScenarioResult};
pub use scenario::{
    DatasetLoader, EvalResult, Evaluator, Scenario, exact_match, gaia_normalized_exact_match,
    token_f1,
};
