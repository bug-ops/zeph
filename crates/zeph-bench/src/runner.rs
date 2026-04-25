// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Benchmark runner: drives `Agent<BenchmarkChannel>` over a dataset and collects results.
//!
//! [`BenchRunner`] is the execution engine for `zeph bench run`. It is intentionally
//! minimal — baseline mode only (no tools, no memory, no MCP). Each scenario is run in
//! isolation through a fresh [`BenchmarkChannel`] and the agent's raw text response is
//! scored by the supplied [`Evaluator`].
//!
//! # Usage
//!
//! ```no_run
//! use std::path::Path;
//! use zeph_bench::runner::{BenchRunner, RunOptions};
//! use zeph_bench::loaders::{GaiaLoader, GaiaEvaluator};
//! use zeph_llm::{any::AnyProvider, mock::MockProvider};
//!
//! # async fn example() -> Result<(), zeph_bench::BenchError> {
//! let provider = AnyProvider::Mock(MockProvider::with_responses(vec!["1945".into()]));
//! let runner = BenchRunner::new(provider, false);
//! let opts = RunOptions::default();
//! let run = runner.run_dataset(&GaiaLoader::all_levels(), &GaiaEvaluator, Path::new("/data/gaia.jsonl"), opts).await?;
//! println!("mean score: {:.4}", run.aggregate.mean_score);
//! # Ok(())
//! # }
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use zeph_core::agent::Agent;
use zeph_core::instructions::InstructionBlock;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

use crate::channel::BenchmarkChannel;
use crate::error::BenchError;
use crate::results::{BenchRun, RunStatus, ScenarioResult};
use crate::scenario::{DatasetLoader, Evaluator};

/// Options that control which scenarios are executed and whether to resume a prior run.
///
/// Build via [`RunOptions::default`] and override the fields you need.
///
/// # Examples
///
/// ```
/// use zeph_bench::runner::RunOptions;
///
/// // Run all scenarios.
/// let opts = RunOptions::default();
/// assert!(opts.scenario_filter.is_none());
/// assert!(opts.completed_ids.is_empty());
/// ```
#[derive(Debug, Default)]
pub struct RunOptions {
    /// When `Some(id)`, only the scenario with this ID is executed.
    pub scenario_filter: Option<String>,
    /// Set of scenario IDs already completed in a prior run (used for `--resume`).
    pub completed_ids: HashSet<String>,
}

/// Minimal no-op tool executor for baseline benchmark runs.
///
/// Returns an empty tool list and `Ok(None)` on every execute call, ensuring that
/// the agent loop cannot invoke any tools during a benchmark run.
struct NoopExecutor;

impl ToolExecutor for NoopExecutor {
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }
}

/// Drives [`Agent<BenchmarkChannel>`] over a dataset and collects scored results.
///
/// Each call to [`run_dataset`][BenchRunner::run_dataset] creates a fresh agent per
/// scenario (baseline mode: no tools, no memory, no MCP). Scenarios can be filtered
/// and prior runs can be resumed via [`RunOptions`].
///
/// # Examples
///
/// ```no_run
/// use zeph_bench::runner::BenchRunner;
/// use zeph_llm::{any::AnyProvider, mock::MockProvider};
///
/// let provider = AnyProvider::Mock(MockProvider::with_responses(vec!["Paris".into()]));
/// let runner = BenchRunner::new(provider, false);
/// ```
pub struct BenchRunner {
    provider: AnyProvider,
}

impl BenchRunner {
    /// Create a new runner with the given provider.
    ///
    /// The `no_deterministic` argument is unused at runtime but kept in the public API
    /// so the bench command can pass it through for future use (e.g., logging or config).
    /// Apply deterministic overrides to `provider` before calling this if needed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_bench::runner::BenchRunner;
    /// use zeph_llm::{any::AnyProvider, mock::MockProvider};
    ///
    /// let provider = AnyProvider::Mock(MockProvider::with_responses(vec![]));
    /// let runner = BenchRunner::new(provider, false);
    /// ```
    #[must_use]
    pub fn new(provider: AnyProvider, _no_deterministic: bool) -> Self {
        Self { provider }
    }

    /// Run all matching scenarios from `path` through the agent and return a [`BenchRun`].
    ///
    /// For each scenario:
    /// 1. Builds a fresh `Agent<BenchmarkChannel>` with no tools or memory.
    /// 2. Feeds the scenario prompt and collects the agent's response.
    /// 3. Scores the response with `evaluator`.
    /// 4. Appends a [`ScenarioResult`] and recomputes aggregate statistics.
    ///
    /// The returned [`BenchRun`] has `status = Running` until the caller sets it to
    /// `Completed` or `Interrupted`.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError`] if the dataset cannot be loaded or a scenario run fails.
    pub async fn run_dataset<L, E>(
        &self,
        loader: &L,
        evaluator: &E,
        path: &Path,
        opts: RunOptions,
    ) -> Result<BenchRun, BenchError>
    where
        L: DatasetLoader,
        E: Evaluator,
    {
        let scenarios = loader.load(path)?;
        let model_id = self.provider.model_identifier().to_owned();

        let mut run = BenchRun {
            dataset: loader.name().to_owned(),
            model: model_id,
            run_id: uuid(),
            started_at: now_rfc3339(),
            finished_at: String::new(),
            status: RunStatus::Running,
            results: vec![],
            aggregate: crate::results::Aggregate::default(),
        };

        for scenario in &scenarios {
            // Skip if resume is active and scenario already completed.
            if opts.completed_ids.contains(&scenario.id) {
                continue;
            }
            // Skip if a single-scenario filter is active.
            if let Some(ref filter) = opts.scenario_filter
                && &scenario.id != filter
            {
                continue;
            }

            let t0 = Instant::now();
            let response_text = Box::pin(self.run_one(scenario.prompt.clone())).await?;
            let elapsed_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);

            let eval = evaluator.evaluate(scenario, &response_text);
            let excerpt = response_text.chars().take(200).collect::<String>();

            run.results.push(ScenarioResult {
                scenario_id: scenario.id.clone(),
                score: eval.score,
                response_excerpt: excerpt,
                error: None,
                elapsed_ms,
            });
            run.recompute_aggregate();
        }

        Ok(run)
    }

    /// Run a single prompt through a fresh agent and return the last response text.
    ///
    /// A concise-answer system prompt is injected via [`InstructionBlock`] so the model
    /// responds with only the final answer (a number, word, or short phrase) rather than
    /// full sentences. The raw response is then post-processed to extract the first
    /// non-empty line and strip markdown formatting, which further reduces noise for
    /// evaluators that perform exact or near-exact matching.
    async fn run_one(&self, prompt: String) -> Result<String, BenchError> {
        let channel = BenchmarkChannel::new(vec![prompt]);
        let registry = SkillRegistry::empty();

        // Force the model to emit only the shortest possible answer. This is the primary
        // driver of score improvement — without this, models produce full sentences that
        // fail both token-F1 and exact-match evaluators.
        let blocks = vec![InstructionBlock {
            source: PathBuf::from("<bench-system-prompt>"),
            content: concat!(
                "You are an evaluation assistant. ",
                "Answer every question with the shortest possible response. ",
                "Give only the final answer — no explanation, no full sentences, ",
                "no punctuation unless it is part of the answer. ",
                "If the answer is a single word or number, respond with only that word or number."
            )
            .to_owned(),
        }];

        let mut agent = Agent::new(
            self.provider.clone(),
            channel,
            registry,
            None,
            1,
            NoopExecutor,
        )
        .with_instruction_blocks(blocks);

        // Ignore agent errors — a failed LLM call still yields an empty response that
        // the evaluator scores as 0.0 rather than aborting the entire run.
        let _ = agent.run().await;
        let responses = agent.into_channel().into_responses();
        let raw = responses
            .into_iter()
            .last()
            .map(|r| r.text)
            .unwrap_or_default();
        Ok(post_process_response(&raw))
    }
}

/// Post-process the raw agent response to extract a clean, terse answer.
///
/// Applies these transformations in order:
/// 1. Take only the first non-empty line — strips explanations appended after the answer.
/// 2. Strip markdown formatting (bold `**`, italic `*` and `_`, inline code `` ` ``).
/// 3. Trim surrounding whitespace.
///
/// This is a best-effort cleanup. Evaluators still normalize the result, so minor
/// leftover punctuation is handled downstream.
fn post_process_response(raw: &str) -> String {
    // Take the first non-empty line to discard any trailing explanation.
    let first_line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");

    // Strip common markdown formatting characters.
    first_line
        .trim_matches(|c: char| matches!(c, '*' | '_' | '`' | ' ' | '\t'))
        .replace("**", "")
        .replace('`', "")
        .trim()
        .to_owned()
}

/// Generate a short pseudo-UUID-like run ID without the `uuid` crate.
///
/// Uses `std::time::SystemTime` for uniqueness. Not cryptographically random but
/// sufficient for benchmark run identification.
fn uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("bench-{secs:x}-{ns:x}")
}

/// RFC 3339-like timestamp using `std` only (no chrono).
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Minimal ISO 8601 UTC representation — good enough for result metadata.
    let (y, mo, d, h, mi, s) = secs_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Decompose Unix seconds into (year, month, day, hour, minute, second) UTC.
fn secs_to_ymdhms(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    const SECS_PER_MIN: u64 = 60;
    const DAYS_PER_400Y: u64 = 146_097;

    let s = secs % SECS_PER_MIN;
    let total_mins = secs / SECS_PER_MIN;
    let mi = total_mins % 60;
    let total_hours = total_mins / 60;
    let h = total_hours % 24;
    let mut days = total_hours / 24;

    // Proleptic Gregorian calendar computation.
    // Shift epoch from 1970-01-01 to 0000-03-01 for easier leap-year math.
    days += 719_468;
    let era = days / DAYS_PER_400Y;
    let doe = days % DAYS_PER_400Y;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_options_default_is_empty() {
        let opts = RunOptions::default();
        assert!(opts.scenario_filter.is_none());
        assert!(opts.completed_ids.is_empty());
    }

    #[test]
    fn now_rfc3339_has_correct_format() {
        let ts = now_rfc3339();
        // e.g. "2026-04-25T10:30:00Z"
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
    }

    #[test]
    fn uuid_generates_non_empty_string() {
        let id = uuid();
        assert!(id.starts_with("bench-"));
        assert!(id.len() > 10);
    }

    #[test]
    fn post_process_takes_first_line() {
        let raw = "1945\n\nWorld War II ended in 1945.";
        assert_eq!(post_process_response(raw), "1945");
    }

    #[test]
    fn post_process_strips_markdown_bold() {
        assert_eq!(post_process_response("**1945**"), "1945");
    }

    #[test]
    fn post_process_strips_backticks() {
        assert_eq!(post_process_response("`Au`"), "Au");
    }

    #[test]
    fn post_process_trims_whitespace() {
        assert_eq!(post_process_response("  Paris  "), "Paris");
    }

    #[test]
    fn post_process_empty_input_returns_empty() {
        assert_eq!(post_process_response(""), "");
    }

    #[test]
    fn post_process_skips_empty_leading_lines() {
        let raw = "\n\n  \nParis";
        assert_eq!(post_process_response(raw), "Paris");
    }
}
