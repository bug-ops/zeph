// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Experiment engine for adaptive agent behavior testing and hyperparameter tuning.
//!
//! `zeph-experiments` provides the infrastructure for running autonomous A/B experiments
//! over Zeph's tunable parameters (temperature, top-p, retrieval depth, etc.) using an
//! LLM-as-judge evaluation loop.
//!
//! # Architecture
//!
//! The crate is organized around three main concerns:
//!
//! 1. **Benchmark datasets** — [`BenchmarkSet`] / [`BenchmarkCase`]: TOML-loaded prompt/reference
//!    pairs that define *what* to measure.
//! 2. **Evaluation** — [`Evaluator`]: runs cases against a subject model and scores responses
//!    with a judge model, producing an [`EvalReport`].
//! 3. **Search strategies** — [`VariationGenerator`] implementations ([`GridStep`], [`Random`],
//!    [`Neighborhood`]) that decide *which* parameter to try next.
//!
//! [`ExperimentEngine`] ties all three together: it evaluates a baseline, iterates over
//! variations produced by the generator, accepts improvements (greedy hill-climbing), and
//! optionally persists results to SQLite.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use zeph_experiments::{
//!     BenchmarkCase, BenchmarkSet, ConfigSnapshot, EvalError, Evaluator, ExperimentEngine,
//!     GridStep, SearchSpace,
//! };
//! # use zeph_llm::any::AnyProvider;
//! # use zeph_llm::mock::MockProvider;
//! # use zeph_config::ExperimentConfig;
//!
//! # async fn example() -> Result<(), EvalError> {
//! let benchmark = BenchmarkSet {
//!     cases: vec![BenchmarkCase {
//!         prompt: "What is the capital of France?".into(),
//!         context: None,
//!         reference: Some("Paris".into()),
//!         tags: None,
//!     }],
//! };
//!
//! // Use a mock provider for the judge in tests; real providers in production.
//! let judge = Arc::new(AnyProvider::Mock(MockProvider::with_responses(vec![
//!     r#"{"score": 9.0, "reason": "correct"}"#.into(),
//! ])));
//! let subject = Arc::new(AnyProvider::Mock(MockProvider::with_responses(vec![
//!     "Paris".into(),
//! ])));
//!
//! let evaluator = Evaluator::new(Arc::clone(&judge), benchmark, 100_000)?;
//! let generator = Box::new(GridStep::new(SearchSpace::default()));
//! let baseline = ConfigSnapshot::default();
//! let config = ExperimentConfig::default();
//!
//! let mut engine = ExperimentEngine::new(evaluator, generator, subject, baseline, config, None);
//! let report = engine.run().await?;
//! println!("baseline={:.2} final={:.2}", report.baseline_score, report.final_score);
//! # Ok(())
//! # }
//! ```
pub mod benchmark;
pub mod engine;
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
pub use engine::{ExperimentEngine, ExperimentSessionReport};
pub use error::EvalError;
pub use evaluator::{CaseScore, EvalReport, Evaluator, JudgeOutput};
pub use generator::VariationGenerator;
pub use grid::GridStep;
pub use neighborhood::Neighborhood;
pub use random::Random;
pub use search_space::{ParameterRange, SearchSpace};
pub use snapshot::{ConfigSnapshot, GenerationOverrides};
pub use types::{ExperimentResult, ExperimentSource, ParameterKind, Variation, VariationValue};
