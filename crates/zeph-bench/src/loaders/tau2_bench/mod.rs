// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! tau2-bench full-environment support.
//!
//! Implements loaders, in-memory environment executors, and an action-trace
//! evaluator for the [`sierra-research/tau2-bench`](https://github.com/sierra-research/tau2-bench)
//! benchmark.
//!
//! # Architecture
//!
//! ```text
//! Tau2BenchLoader (retail/airline)
//!   └─ loads tasks.json → Vec<Scenario> (metadata carries EvaluationCriteria)
//!
//! Per scenario (in BenchRunner::run_dataset_with_env_factory):
//!   RetailEnv / AirlineEnv  ←──── db.json seed
//!      │ implements ToolExecutor
//!      │ records every tool call to ActionTrace (Arc<Mutex<Vec<RecordedToolCall>>>)
//!      └─▶ agent sees tool results, calls more tools
//!
//!   TauBenchEvaluator
//!      │ holds clone of the same ActionTrace
//!      └─▶ after run: scores gold_actions vs recorded_calls
//! ```
//!
//! # Supported domains
//!
//! | Dataset name | Domain | Loader constructor |
//! |---|---|---|
//! | `tau2-bench-retail` | Retail customer service | [`loader::Tau2BenchLoader::retail`] |
//! | `tau2-bench-airline` | Airline flight reservation | [`loader::Tau2BenchLoader::airline`] |

pub mod data;
pub mod envs;
pub mod eval;
pub mod loader;

pub use data::Domain;
pub use envs::ActionTrace;
pub use envs::airline::AirlineEnv;
pub use envs::retail::RetailEnv;
pub use eval::TauBenchEvaluator;
pub use loader::{Tau2BenchLoader, db_json_path};
