// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/experiment` slash-command handler for the agent loop.

use std::fmt::Write as _;
use std::sync::Arc;

use super::{Agent, error::AgentError};
use crate::channel::Channel;
use crate::experiments::{
    BenchmarkSet, Evaluator, ExperimentEngine, ExperimentSource, GridStep, SearchSpace,
};

impl<C: Channel> Agent<C> {
    /// Dispatch `/experiment [subcommand]` slash command.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel send fails or a `SQLite` query fails.
    pub async fn handle_experiment_command(&mut self, input: &str) -> Result<(), AgentError> {
        let args = input.strip_prefix("/experiment").unwrap_or("").trim();

        match args {
            "" | "status" => return self.handle_experiment_status().await,
            "stop" => return self.handle_experiment_stop().await,
            "report" => return self.handle_experiment_report().await,
            "best" => return self.handle_experiment_best().await,
            _ => {}
        }

        if args == "start" || args.starts_with("start") {
            let max_override = args
                .strip_prefix("start")
                .and_then(|s| s.trim().parse::<u32>().ok());
            return self.handle_experiment_start(max_override).await;
        }

        self.channel
            .send(
                "Unknown /experiment subcommand. Available: /experiment start [N], \
                 /experiment stop, /experiment status, /experiment report, /experiment best",
            )
            .await?;
        Ok(())
    }

    async fn handle_experiment_start(
        &mut self,
        max_override: Option<u32>,
    ) -> Result<(), AgentError> {
        // Guard: reject if an experiment is already running.
        if self
            .experiments
            .cancel
            .as_ref()
            .is_some_and(|t| !t.is_cancelled())
        {
            self.channel
                .send("Experiment already running. Use /experiment stop to cancel.")
                .await?;
            return Ok(());
        }

        let mut config = self.experiments.config.clone();

        if !config.enabled {
            self.channel
                .send(
                    "Experiments are disabled. Set `experiments.enabled = true` in config \
                     and restart.",
                )
                .await?;
            return Ok(());
        }

        if let Some(n) = max_override {
            config.max_experiments = n;
        }

        if let Err(e) = config.validate() {
            self.channel
                .send(&format!("Experiment config is invalid: {e}"))
                .await?;
            return Ok(());
        }

        let max_n = config.max_experiments;
        let engine = match self.build_experiment_engine(config) {
            Ok(e) => e,
            Err(msg) => {
                self.channel.send(&msg).await?;
                return Ok(());
            }
        };

        self.run_experiment_engine(engine, max_n).await
    }

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
        // TODO(#eval-model): eval_model config field is not yet wired to evaluator construction.
        // Both the agent path and runner.rs use the agent's own provider as the judge.
        // Wire eval_model to create a separate judge provider in a follow-up PR.
        let evaluator = Evaluator::new(
            Arc::clone(&provider_arc),
            benchmark,
            config.eval_budget_tokens,
        )
        .map_err(|e| format!("Failed to create evaluator: {e}"))?;

        let generator = Box::new(GridStep::new(SearchSpace::default()));
        // Use the pre-built baseline snapshot that reflects actual runtime config values.
        // Set via Agent::with_experiment_baseline(); defaults to ConfigSnapshot::default()
        // when not explicitly provided.
        let baseline = self.experiments.baseline.clone();
        let memory = self.memory_state.memory.clone();

        Ok(
            ExperimentEngine::new(evaluator, generator, provider_arc, baseline, config, memory)
                .with_source(ExperimentSource::Manual),
        )
    }

    async fn run_experiment_engine(
        &mut self,
        mut engine: ExperimentEngine,
        max_n: u32,
    ) -> Result<(), AgentError> {
        let cancel = engine.cancel_token();
        self.experiments.cancel = Some(cancel);

        self.channel
            .send(&format!(
                "Experiment session starting (max {max_n} experiments). \
                 Use /experiment stop to cancel. Results will be shown when complete.",
            ))
            .await?;

        // Run the engine in a background task so the agent loop remains responsive.
        // Completion (ok or error) is forwarded via experiment_notify_tx; the agent loop
        // select! branch clears experiment_cancel and delivers the message to the channel.
        let notify_tx = self.experiments.notify_tx.clone();
        tokio::spawn(async move {
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
            // Ignore send errors: the agent may have shut down before the engine finished.
            let _ = notify_tx.send(msg).await;
        });

        Ok(())
    }

    async fn handle_experiment_stop(&mut self) -> Result<(), AgentError> {
        match &self.experiments.cancel {
            Some(token) if !token.is_cancelled() => {
                token.cancel();
                self.channel
                    .send("Experiment session cancelled. Results so far are saved.")
                    .await?;
            }
            _ => {
                self.channel
                    .send("No experiment is currently running.")
                    .await?;
            }
        }
        Ok(())
    }

    async fn handle_experiment_status(&mut self) -> Result<(), AgentError> {
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

        if let Some(memory) = &self.memory_state.memory {
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
                    "\nLast session: `{}` | {} experiments | {} accepted | \
                     best delta: {:.3}",
                    &summary.session_id[..sid_len],
                    summary.total,
                    summary.accepted_count,
                    summary.best_delta,
                );
            }
        }

        self.channel.send(&msg).await?;
        Ok(())
    }

    async fn handle_experiment_report(&mut self) -> Result<(), AgentError> {
        let Some(memory) = &self.memory_state.memory else {
            self.channel
                .send("Memory is not enabled — cannot query experiment results.")
                .await?;
            return Ok(());
        };

        let rows = memory.sqlite().list_experiment_results(None, 50).await?;

        if rows.is_empty() {
            self.channel.send("No experiment results found.").await?;
            return Ok(());
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
        self.channel.send(&out).await?;
        Ok(())
    }

    async fn handle_experiment_best(&mut self) -> Result<(), AgentError> {
        let Some(memory) = &self.memory_state.memory else {
            self.channel
                .send("Memory is not enabled — cannot query experiment results.")
                .await?;
            return Ok(());
        };

        let row = memory.sqlite().best_experiment_result(None).await?;

        match row {
            None => {
                self.channel
                    .send("No accepted experiment results found yet.")
                    .await?;
            }
            Some(r) => {
                let sid_len = r.session_id.len().min(11);
                let msg = format!(
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
                );
                self.channel.send(&msg).await?;
            }
        }
        Ok(())
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
        agent
            .handle_experiment_command("/experiment foobar")
            .await
            .unwrap();
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|s| s.contains("Unknown /experiment")),
            "expected help text, got: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn start_when_disabled_returns_error() {
        let mut agent = make_agent();
        // Default config has enabled = false.
        agent
            .handle_experiment_command("/experiment start")
            .await
            .unwrap();
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|s| s.contains("disabled")),
            "expected disabled message, got: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn stop_when_not_running_returns_not_running() {
        let mut agent = make_agent();
        agent
            .handle_experiment_command("/experiment stop")
            .await
            .unwrap();
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter()
                .any(|s| s.contains("No experiment is currently running")),
            "expected not-running message, got: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn status_when_idle_returns_idle() {
        let mut agent = make_agent();
        agent
            .handle_experiment_command("/experiment status")
            .await
            .unwrap();
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|s| s.contains("idle")),
            "expected idle status, got: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn empty_subcommand_returns_status() {
        let mut agent = make_agent();
        agent
            .handle_experiment_command("/experiment")
            .await
            .unwrap();
        let msgs = agent.channel.sent_messages();
        // Empty subcommand maps to status (idle when no experiment running).
        assert!(
            msgs.iter().any(|s| s.contains("idle")),
            "expected idle status for empty subcommand, got: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn start_while_running_returns_already_running() {
        use tokio_util::sync::CancellationToken;

        let mut agent = make_agent();
        // Simulate a running experiment by inserting a live cancel token.
        agent.experiments.cancel = Some(CancellationToken::new());

        agent
            .handle_experiment_command("/experiment start")
            .await
            .unwrap();
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|s| s.contains("already running")),
            "expected already-running guard, got: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn report_without_memory_returns_error() {
        let mut agent = make_agent();
        // No memory wired → should return the "not enabled" message.
        agent
            .handle_experiment_command("/experiment report")
            .await
            .unwrap();
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|s| s.contains("Memory is not enabled")),
            "expected no-memory message, got: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn best_without_memory_returns_error() {
        let mut agent = make_agent();
        agent
            .handle_experiment_command("/experiment best")
            .await
            .unwrap();
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|s| s.contains("Memory is not enabled")),
            "expected no-memory message, got: {msgs:?}"
        );
    }
}
