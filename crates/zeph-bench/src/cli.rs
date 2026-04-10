// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Clap subcommand definitions for `zeph bench`.
//!
//! The top-level entry point is [`BenchCommand`], which is nested under the root
//! `zeph` binary as `zeph bench <subcommand>`.

/// Top-level subcommands available under `zeph bench`.
///
/// Each variant maps to one logical operation:
/// - [`List`][BenchCommand::List] — inspect what datasets are available locally.
/// - [`Download`][BenchCommand::Download] — fetch a dataset from its canonical URL.
/// - [`Run`][BenchCommand::Run] — execute a full benchmark run and write results.
/// - [`Show`][BenchCommand::Show] — print a summary of a previously saved run.
///
/// # Examples
///
/// ```
/// use zeph_bench::BenchCommand;
///
/// // The enum is parsed by Clap; construct directly in tests.
/// let cmd = BenchCommand::List;
/// assert!(matches!(cmd, BenchCommand::List));
/// ```
#[derive(clap::Subcommand, Debug)]
pub enum BenchCommand {
    /// List available benchmark datasets and their cache status
    List,

    /// Download a dataset to the local cache
    Download {
        /// Dataset name (e.g. gaia, tau-bench)
        #[arg(long)]
        dataset: String,
    },

    /// Run a benchmark against the agent
    Run {
        /// Dataset name (e.g. `locomo`, `gaia`, `frames`)
        #[arg(long)]
        dataset: String,

        /// Directory where `results.json` and `summary.md` are written
        #[arg(long)]
        output: std::path::PathBuf,

        /// Run only the scenario with this ID (runs all scenarios if omitted)
        #[arg(long)]
        scenario: Option<String>,

        /// LLM provider name as declared in `[[llm.providers]]` (uses default if omitted)
        #[arg(long)]
        provider: Option<String>,

        /// Run with a baseline (non-agentic) configuration that disables tools and memory
        #[arg(long)]
        baseline: bool,

        /// Resume a previously interrupted run, skipping already-completed scenarios
        #[arg(long)]
        resume: bool,

        /// Disable deterministic mode — by default temperature is forced to 0.0 for
        /// reproducibility; pass this flag to use the provider's configured temperature
        #[arg(long)]
        no_deterministic: bool,
    },

    /// Print a human-readable summary of results from a previous benchmark run
    Show {
        /// Path to the `results.json` file produced by `bench run`
        #[arg(long)]
        results: std::path::PathBuf,
    },
}
