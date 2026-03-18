// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

fn default_planner_max_tokens() -> u32 {
    4096
}

fn default_aggregator_max_tokens() -> u32 {
    4096
}

fn default_deferral_backoff_ms() -> u64 {
    100
}

fn default_experiment_max_experiments() -> u32 {
    20
}

fn default_experiment_max_wall_time_secs() -> u64 {
    3600
}

fn default_experiment_min_improvement() -> f64 {
    0.5
}

fn default_experiment_eval_budget_tokens() -> u64 {
    100_000
}

fn default_experiment_schedule_cron() -> String {
    "0 3 * * *".to_string()
}

fn default_experiment_max_experiments_per_run() -> u32 {
    20
}

fn default_experiment_schedule_max_wall_time_secs() -> u64 {
    1800
}

/// Configuration for the task orchestration subsystem (`[orchestration]` TOML section).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct OrchestrationConfig {
    /// Enable the orchestration subsystem.
    pub enabled: bool,
    /// Maximum number of tasks in a single graph.
    pub max_tasks: u32,
    /// Maximum number of tasks that can run in parallel.
    pub max_parallel: u32,
    /// Default failure strategy for all tasks unless overridden per-task.
    pub default_failure_strategy: String,
    /// Default number of retries for the `retry` failure strategy.
    pub default_max_retries: u32,
    /// Timeout in seconds for a single task. `0` means no timeout.
    pub task_timeout_secs: u64,
    /// Model override for planning LLM calls. When `None`, uses the agent's primary model.
    ///
    /// Reserved for future caller-side provider selection (post-MVP). `LlmPlanner` itself
    /// does not use this field — the caller is responsible for constructing the appropriate
    /// provider based on this value before passing it to `LlmPlanner::new`.
    #[serde(default)]
    pub planner_model: Option<String>,
    /// Maximum tokens budget hint for planner responses. Reserved for future use when
    /// per-call token limits are added to the `LlmProvider::chat` API.
    #[serde(default = "default_planner_max_tokens")]
    pub planner_max_tokens: u32,
    /// Total character budget for cross-task dependency context injection.
    pub dependency_context_budget: usize,
    /// Whether to show a confirmation prompt before executing a plan.
    pub confirm_before_execute: bool,
    /// Maximum tokens budget for aggregation LLM calls. Default: 4096.
    #[serde(default = "default_aggregator_max_tokens")]
    pub aggregator_max_tokens: u32,
    /// Base backoff for `ConcurrencyLimit` retries; grows exponentially (×2 each attempt) up to 5 s.
    #[serde(default = "default_deferral_backoff_ms")]
    pub deferral_backoff_ms: u64,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_tasks: 20,
            max_parallel: 4,
            default_failure_strategy: "abort".to_string(),
            default_max_retries: 3,
            task_timeout_secs: 300,
            planner_model: None,
            planner_max_tokens: default_planner_max_tokens(),
            dependency_context_budget: 16384,
            confirm_before_execute: true,
            aggregator_max_tokens: default_aggregator_max_tokens(),
            deferral_backoff_ms: default_deferral_backoff_ms(),
        }
    }
}

/// Configuration for the autonomous self-experimentation engine (`[experiments]` TOML section).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ExperimentConfig {
    pub enabled: bool,
    pub eval_model: Option<String>,
    pub benchmark_file: Option<std::path::PathBuf>,
    #[serde(default = "default_experiment_max_experiments")]
    pub max_experiments: u32,
    #[serde(default = "default_experiment_max_wall_time_secs")]
    pub max_wall_time_secs: u64,
    #[serde(default = "default_experiment_min_improvement")]
    pub min_improvement: f64,
    #[serde(default = "default_experiment_eval_budget_tokens")]
    pub eval_budget_tokens: u64,
    pub auto_apply: bool,
    #[serde(default)]
    pub schedule: ExperimentSchedule,
}

impl Default for ExperimentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            eval_model: None,
            benchmark_file: None,
            max_experiments: default_experiment_max_experiments(),
            max_wall_time_secs: default_experiment_max_wall_time_secs(),
            min_improvement: default_experiment_min_improvement(),
            eval_budget_tokens: default_experiment_eval_budget_tokens(),
            auto_apply: false,
            schedule: ExperimentSchedule::default(),
        }
    }
}

/// Cron scheduling configuration for automatic experiment runs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ExperimentSchedule {
    pub enabled: bool,
    #[serde(default = "default_experiment_schedule_cron")]
    pub cron: String,
    #[serde(default = "default_experiment_max_experiments_per_run")]
    pub max_experiments_per_run: u32,
    /// Wall-time cap for a single scheduled experiment session (seconds).
    ///
    /// Overrides `experiments.max_wall_time_secs` for scheduled runs. Defaults to 1800s so
    /// a background session cannot overlap the next cron trigger on typical schedules.
    #[serde(default = "default_experiment_schedule_max_wall_time_secs")]
    pub max_wall_time_secs: u64,
}

impl Default for ExperimentSchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            cron: default_experiment_schedule_cron(),
            max_experiments_per_run: default_experiment_max_experiments_per_run(),
            max_wall_time_secs: default_experiment_schedule_max_wall_time_secs(),
        }
    }
}

impl ExperimentConfig {
    /// Validate that numeric bounds are within sane operating limits.
    ///
    /// # Errors
    ///
    /// Returns a description string if any field is outside allowed range.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=1_000).contains(&self.max_experiments) {
            return Err(format!(
                "experiments.max_experiments must be in 1..=1000, got {}",
                self.max_experiments
            ));
        }
        if !(60..=86_400).contains(&self.max_wall_time_secs) {
            return Err(format!(
                "experiments.max_wall_time_secs must be in 60..=86400, got {}",
                self.max_wall_time_secs
            ));
        }
        if !(1_000..=10_000_000).contains(&self.eval_budget_tokens) {
            return Err(format!(
                "experiments.eval_budget_tokens must be in 1000..=10000000, got {}",
                self.eval_budget_tokens
            ));
        }
        if !(0.0..=100.0).contains(&self.min_improvement) {
            return Err(format!(
                "experiments.min_improvement must be in 0.0..=100.0, got {}",
                self.min_improvement
            ));
        }
        if !(1..=100).contains(&self.schedule.max_experiments_per_run) {
            return Err(format!(
                "experiments.schedule.max_experiments_per_run must be in 1..=100, got {}",
                self.schedule.max_experiments_per_run
            ));
        }
        if !(60..=86_400).contains(&self.schedule.max_wall_time_secs) {
            return Err(format!(
                "experiments.schedule.max_wall_time_secs must be in 60..=86400, got {}",
                self.schedule.max_wall_time_secs
            ));
        }
        Ok(())
    }
}
