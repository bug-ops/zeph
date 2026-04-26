// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/experiment` slash-command handler for the agent loop.

use std::fmt::Write as _;
use std::sync::Arc;

use super::{Agent, error::AgentError};
use crate::channel::Channel;
use zeph_experiments::{
    BenchmarkSet, Evaluator, ExperimentEngine, ExperimentSource, GridStep, SearchSpace,
};

impl<C: Channel> Agent<C> {
    /// Build an [`ExperimentEngine`] from validated config, returning `Err(message)` on failure.
    fn build_experiment_engine(
        &mut self,
        config: crate::config::ExperimentConfig,
    ) -> Result<ExperimentEngine, String> {
        let benchmark_path = if let Some(p) = &config.benchmark_file {
            p.clone()
        } else {
            return Err("experiments.benchmark_file is not set in config.".to_owned());
        };

        let benchmark = BenchmarkSet::from_file(&benchmark_path)
            .map_err(|e| format!("Failed to load benchmark: {e}"))?;

        let provider_arc = Arc::new(self.provider.clone());
        // Use a dedicated eval provider when `eval_model` is configured, so the judge is
        // independent from the agent under test. Fall back to the primary provider otherwise.
        let judge_arc = self
            .experiments
            .eval_provider
            .as_ref()
            .map_or_else(|| Arc::clone(&provider_arc), |p| Arc::new(p.clone()));
        let evaluator = Evaluator::new(judge_arc, benchmark, config.eval_budget_tokens)
            .map_err(|e| format!("Failed to create evaluator: {e}"))?;

        let generator = Box::new(GridStep::new(SearchSpace::default()));
        // Use the pre-built baseline snapshot that reflects actual runtime config values.
        // Set via Agent::with_experiment(); defaults to ConfigSnapshot::default()
        // when not explicitly provided.
        let baseline = self.experiments.baseline.clone();
        let memory = self.memory_state.persistence.memory.clone();

        Ok(
            ExperimentEngine::new(evaluator, generator, provider_arc, baseline, config, memory)
                .with_source(ExperimentSource::Manual),
        )
    }

    /// Dispatch `/experiment` returning output as a string instead of writing to the channel.
    ///
    /// All `Arc` clones are extracted before `.await` so `&mut self` is not held across await
    /// boundaries, making the returned future `Send`.
    pub(super) async fn handle_experiment_command_as_string(
        &mut self,
        input: &str,
    ) -> Result<String, AgentError> {
        let args = input.strip_prefix("/experiment").unwrap_or("").trim();
        match args {
            "" | "status" => self.experiment_status().await,
            "stop" => Ok(self.experiment_stop()),
            "report" => self.experiment_report().await,
            "best" => self.experiment_best().await,
            _ if args == "start" || args.starts_with("start ") => {
                let max_override = args
                    .strip_prefix("start")
                    .and_then(|s| s.trim().parse::<u32>().ok());
                Ok(self.experiment_start(max_override))
            }
            _ => Ok(
                "Unknown /experiment subcommand. Available: /experiment start [N], \
                 /experiment stop, /experiment status, /experiment report, /experiment best"
                    .to_owned(),
            ),
        }
    }

    async fn experiment_status(&mut self) -> Result<String, AgentError> {
        let running = self
            .experiments
            .cancel
            .as_ref()
            .is_some_and(|t| !t.is_cancelled());
        let mut msg = if running {
            String::from("Experiment: **running**. Use `/experiment stop` to cancel.")
        } else {
            String::from("Experiment: **idle**. Use `/experiment start [N]` to begin.")
        };
        let memory = self.memory_state.persistence.memory.clone();
        if let Some(memory) = memory {
            let rows = memory.sqlite().list_experiment_results(None, 1).await?;
            if let Some(latest) = rows.first()
                && let Some(summary) = memory
                    .sqlite()
                    .experiment_session_summary(&latest.session_id)
                    .await?
            {
                let sid_len = summary.session_id.len().min(11);
                let _ = write!(
                    msg,
                    "\nLast session: `{}` | {} experiments | {} accepted | best delta: {:.3}",
                    &summary.session_id[..sid_len],
                    summary.total,
                    summary.accepted_count,
                    summary.best_delta,
                );
            }
        }
        Ok(msg)
    }

    fn experiment_stop(&mut self) -> String {
        match &self.experiments.cancel {
            Some(token) if !token.is_cancelled() => {
                token.cancel();
                "Experiment session cancelled. Results so far are saved.".to_owned()
            }
            _ => "No experiment is currently running.".to_owned(),
        }
    }

    async fn experiment_report(&mut self) -> Result<String, AgentError> {
        let memory = self.memory_state.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory is not enabled — cannot query experiment results.".to_owned());
        };
        let rows = memory.sqlite().list_experiment_results(None, 50).await?;
        if rows.is_empty() {
            return Ok("No experiment results found.".to_owned());
        }
        let mut out = String::from("**Experiment Results** (last 50, newest first)\n\n```\n");
        let _ = writeln!(
            out,
            "{:<8} {:<12} {:<20} {:<8} {:<8} {:<8} {:<8}",
            "ID", "Session", "Parameter", "Delta", "Baseline", "Candidate", "Accepted"
        );
        for r in &rows {
            let sid_len = r.session_id.len().min(11);
            let _ = writeln!(
                out,
                "{:<8} {:<12} {:<20} {:<8.3} {:<8.3} {:<8.3} {:<8}",
                r.id,
                &r.session_id[..sid_len],
                &r.parameter,
                r.delta,
                r.baseline_score,
                r.candidate_score,
                if r.accepted { "yes" } else { "no" },
            );
        }
        out.push_str("```");
        Ok(out)
    }

    async fn experiment_best(&mut self) -> Result<String, AgentError> {
        let memory = self.memory_state.persistence.memory.clone();
        let Some(memory) = memory else {
            return Ok("Memory is not enabled — cannot query experiment results.".to_owned());
        };
        let row = memory.sqlite().best_experiment_result(None).await?;
        let msg = match row {
            None => "No accepted experiment results found yet.".to_owned(),
            Some(r) => {
                let sid_len = r.session_id.len().min(11);
                format!(
                    "**Best experiment result**\n\
                     - Session: `{}`\n\
                     - Parameter: `{}`\n\
                     - Delta: `{:.3}` ({:.3} → {:.3})\n\
                     - Source: `{}`\n\
                     - At: {}",
                    &r.session_id[..sid_len],
                    r.parameter,
                    r.delta,
                    r.baseline_score,
                    r.candidate_score,
                    r.source,
                    r.created_at,
                )
            }
        };
        Ok(msg)
    }

    fn experiment_start(&mut self, max_override: Option<u32>) -> String {
        if self
            .experiments
            .cancel
            .as_ref()
            .is_some_and(|t| !t.is_cancelled())
        {
            return "Experiment already running. Use /experiment stop to cancel.".to_owned();
        }
        let mut config = self.experiments.config.clone();
        if !config.enabled {
            return "Experiments are disabled. Set `experiments.enabled = true` in config and restart."
                .to_owned();
        }
        if let Some(n) = max_override {
            config.max_experiments = n;
        }
        if let Err(e) = config.validate() {
            return format!("Experiment config is invalid: {e}");
        }
        let max_n = config.max_experiments;
        let engine = match self.build_experiment_engine(config) {
            Ok(e) => e,
            Err(msg) => return msg,
        };
        let cancel = engine.cancel_token();
        self.experiments.cancel = Some(cancel);
        let notify_tx = self.experiments.notify_tx.clone();
        // intentionally untracked: long-running multi-minute LLM session with its own
        // CancellationToken and single-instance invariant enforced by `experiments.cancel`.
        // BackgroundSupervisor::spawn() returns bool (no JoinHandle), uses Drop-on-overflow
        // semantics, and has only small-fast task classes (Telemetry/Enrichment); routing an
        // experiment session through it would silently drop it when the Telemetry pool is full,
        // producing a misleading "starting" message with no experiment actually running.
        drop(tokio::spawn(async move {
            let mut engine = engine;
            let msg = match engine.run().await {
                Ok(report) => {
                    let accepted = report.results.iter().filter(|r| r.accepted).count();
                    let wall_secs =
                        f64::from(u32::try_from(report.wall_time_ms).unwrap_or(u32::MAX)) / 1000.0;
                    format!(
                        "Experiment session `{}` complete: {}/{} accepted, \
                         baseline {:.3} → {:.3} (improvement {:.3}), {wall_secs:.1}s{}",
                        &report.session_id[..report.session_id.len().min(8)],
                        accepted,
                        report.results.len(),
                        report.baseline_score,
                        report.final_score,
                        report.total_improvement,
                        if report.cancelled { " [cancelled]" } else { "" },
                    )
                }
                Err(e) => format!("Experiment session failed: {e}"),
            };
            let _ = notify_tx.send(msg).await;
        }));
        format!(
            "Experiment session starting (max {max_n} experiments). \
             Use /experiment stop to cancel. Results will be shown when complete."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;

    fn make_agent() -> Agent<MockChannel> {
        Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
    }

    #[tokio::test]
    async fn unknown_subcommand_returns_help() {
        let mut agent = make_agent();
        let result = agent
            .handle_experiment_command_as_string("/experiment foobar")
            .await
            .unwrap();
        assert!(
            result.contains("Unknown /experiment"),
            "expected help text, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn start_when_disabled_returns_error() {
        let mut agent = make_agent();
        // Default config has enabled = false.
        let result = agent
            .handle_experiment_command_as_string("/experiment start")
            .await
            .unwrap();
        assert!(
            result.contains("disabled"),
            "expected disabled message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn stop_when_not_running_returns_not_running() {
        let mut agent = make_agent();
        let result = agent
            .handle_experiment_command_as_string("/experiment stop")
            .await
            .unwrap();
        assert!(
            result.contains("No experiment is currently running"),
            "expected not-running message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn status_when_idle_returns_idle() {
        let mut agent = make_agent();
        let result = agent
            .handle_experiment_command_as_string("/experiment status")
            .await
            .unwrap();
        assert!(
            result.contains("idle"),
            "expected idle status, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn empty_subcommand_returns_status() {
        let mut agent = make_agent();
        let result = agent
            .handle_experiment_command_as_string("/experiment")
            .await
            .unwrap();
        // Empty subcommand maps to status (idle when no experiment running).
        assert!(
            result.contains("idle"),
            "expected idle status for empty subcommand, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn start_while_running_returns_already_running() {
        use tokio_util::sync::CancellationToken;

        let mut agent = make_agent();
        // Simulate a running experiment by inserting a live cancel token.
        agent.experiments.cancel = Some(CancellationToken::new());

        let result = agent
            .handle_experiment_command_as_string("/experiment start")
            .await
            .unwrap();
        assert!(
            result.contains("already running"),
            "expected already-running guard, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn report_without_memory_returns_error() {
        let mut agent = make_agent();
        // No memory wired → should return the "not enabled" message.
        let result = agent
            .handle_experiment_command_as_string("/experiment report")
            .await
            .unwrap();
        assert!(
            result.contains("Memory is not enabled"),
            "expected no-memory message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn best_without_memory_returns_error() {
        let mut agent = make_agent();
        let result = agent
            .handle_experiment_command_as_string("/experiment best")
            .await
            .unwrap();
        assert!(
            result.contains("Memory is not enabled"),
            "expected no-memory message, got: {result:?}"
        );
    }
}
