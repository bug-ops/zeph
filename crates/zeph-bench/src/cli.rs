// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Top-level bench subcommands.
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
        /// Dataset name
        #[arg(long)]
        dataset: String,

        /// Output path for results (JSON)
        #[arg(long)]
        output: std::path::PathBuf,

        /// Specific scenario ID to run (runs all if omitted)
        #[arg(long)]
        scenario: Option<String>,

        /// LLM provider name to use (uses default if omitted)
        #[arg(long)]
        provider: Option<String>,

        /// Run with a baseline (non-agentic) configuration
        #[arg(long)]
        baseline: bool,

        /// Resume a previously interrupted run
        #[arg(long)]
        resume: bool,

        /// Disable deterministic mode (temperature=0 override)
        #[arg(long)]
        no_deterministic: bool,
    },

    /// Show results from a previous benchmark run
    Show {
        /// Path to results file
        #[arg(long)]
        results: std::path::PathBuf,
    },
}
