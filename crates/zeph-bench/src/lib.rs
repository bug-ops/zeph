// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Benchmark harness for evaluating Zeph agent performance on standardized datasets.
//!
//! `zeph-bench` implements the CLI subcommand `zeph bench` and provides the building blocks
//! for running reproducible evaluations against LOCOMO, FRAMES, GAIA, and other datasets.
//!
//! # Architecture
//!
//! The harness is built around three composable traits:
//!
//! - [`DatasetLoader`] — reads a dataset file and returns a [`Vec<Scenario>`].
//! - [`Evaluator`] — scores one agent response against a [`Scenario`].
//! - [`zeph_core::channel::Channel`] — implemented by [`BenchmarkChannel`] to drive the agent
//!   loop headlessly (no terminal, no network).
//!
//! Results are accumulated into a [`BenchRun`] and persisted by [`ResultWriter`], which writes
//! both `results.json` (machine-readable) and `summary.md` (human-readable) to the output
//! directory. Runs can be interrupted and resumed via the `--resume` flag.
//!
//! # Quick Start
//!
//! ```no_run
//! use std::path::Path;
//! use zeph_bench::{DatasetRegistry, loaders::{LocomoLoader, LocomoEvaluator}};
//! use zeph_bench::scenario::{DatasetLoader, Evaluator};
//!
//! // 1. Discover available datasets.
//! let registry = DatasetRegistry::new();
//! let meta = registry.get("locomo").expect("locomo is built-in");
//! println!("dataset url: {}", meta.url);
//!
//! // 2. Load scenarios from a locally cached file.
//! let scenarios = LocomoLoader.load(Path::new("/data/locomo.json")).unwrap();
//!
//! // 3. Evaluate a response.
//! let result = LocomoEvaluator.evaluate(&scenarios[0], "some agent response");
//! println!("score={:.4} passed={}", result.score, result.passed);
//! ```
//!
//! # Deterministic Runs
//!
//! By default the harness forces `temperature=0.0` on the configured provider so that runs are
//! reproducible. Pass `--no-deterministic` on the CLI or call [`apply_deterministic_overrides`]
//! with `no_deterministic = true` to disable this behaviour.
//!
//! # Modules
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`baseline`] | Baseline comparison types and delta computation |
//! | [`channel`] | Headless [`BenchmarkChannel`] that drives the agent without I/O |
//! | [`cli`] | Clap subcommand definition ([`BenchCommand`]) |
//! | [`dataset`] | Dataset registry and metadata types |
//! | [`deterministic`] | Temperature-zero override helpers |
//! | [`error`] | [`BenchError`] error type |
//! | [`isolation`] | Per-scenario storage isolation ([`BenchIsolation`]) |
//! | [`loaders`] | Concrete loaders for LOCOMO, FRAMES, GAIA, LongMemEval, and tau-bench |
//! | [`results`] | Result types and [`ResultWriter`] |
//! | [`runner`] | [`BenchRunner`] that drives the agent loop over a dataset |
//! | [`scenario`] | Core traits ([`DatasetLoader`], [`Evaluator`]) and scoring helpers |

pub mod baseline;
pub mod channel;
pub mod cli;
pub mod dataset;
pub mod deterministic;
pub mod error;
pub mod isolation;
pub mod loaders;
pub mod results;
pub mod runner;
pub mod scenario;

pub use baseline::{BaselineComparison, ScenarioDelta};
pub use channel::BenchmarkChannel;
pub use cli::BenchCommand;
pub use dataset::{DatasetFormat, DatasetMeta, DatasetRegistry};
pub use deterministic::apply_deterministic_overrides;
pub use error::BenchError;
pub use isolation::BenchIsolation;
pub use results::{Aggregate, BenchRun, ResultWriter, RunStatus, ScenarioResult};
pub use runner::{BenchMemoryParams, BenchRunner, MemoryMode, ResponseMode, RunOptions};
pub use scenario::{
    DatasetLoader, EvalResult, Evaluator, Role, Scenario, Turn, exact_match,
    gaia_normalized_exact_match, token_f1,
};
