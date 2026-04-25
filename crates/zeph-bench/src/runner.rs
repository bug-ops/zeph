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
use std::sync::Arc;
use std::time::Instant;

use zeph_core::agent::Agent;
use zeph_core::instructions::InstructionBlock;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

use crate::channel::BenchmarkChannel;
use crate::error::BenchError;
use crate::loaders::tau2_bench::{ActionTrace, TauBenchEvaluator};
use crate::results::{BenchRun, RunStatus, ScenarioResult};
use crate::scenario::{DatasetLoader, Evaluator, Scenario};

/// Controls how the runner processes the agent's raw text response.
///
/// Used by [`BenchRunner::run_one_with_executor`] to select the appropriate
/// system prompt and post-processing behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseMode {
    /// Inject a "shortest possible answer" system prompt and strip markdown from the response.
    ///
    /// Used by all knowledge-retrieval datasets (GAIA, LOCOMO, FRAMES, `LongMemEval`).
    TerseAnswer,
    /// Inject a tool-use system prompt; return the raw agent response without post-processing.
    ///
    /// Used by tau2-bench where the evaluation is based on the action trace, not text output.
    ToolUse,
}

/// Controls whether `SemanticMemory` is wired into the agent during a benchmark run.
///
/// # Examples
///
/// ```
/// use zeph_bench::runner::MemoryMode;
///
/// assert_eq!(MemoryMode::default(), MemoryMode::Off);
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MemoryMode {
    /// No `SemanticMemory` — current default behaviour.
    #[default]
    Off,
    /// Wire a `SQLite`-backed `SemanticMemory` into the agent via `Agent::with_memory`.
    On,
}

/// Parameters required to construct a per-scenario `SQLite`-backed `SemanticMemory`.
///
/// Populated by [`BenchRunner::with_memory_params`] and consumed inside
/// [`BenchRunner::run_one`] when `opts.memory_mode == MemoryMode::On`.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
/// use zeph_bench::runner::BenchMemoryParams;
///
/// let params = BenchMemoryParams {
///     data_dir: PathBuf::from("/tmp/bench"),
///     embedding_model: "nomic-embed-text".into(),
///     run_id: "bench-abc".into(),
///     dataset: "locomo".into(),
/// };
/// assert!(params.data_dir.to_string_lossy().contains("bench"));
/// ```
#[derive(Debug, Clone)]
pub struct BenchMemoryParams {
    /// Directory where per-scenario `SQLite` files live (deleted between scenarios).
    ///
    /// The derived path always contains the `bench-` segment (NFR-001).
    pub data_dir: PathBuf,
    /// Embedding model name passed to `SemanticMemory`.
    pub embedding_model: String,
    /// Run ID used to namespace bench artifacts; matches the outer `BenchRun.run_id`.
    pub run_id: String,
    /// Dataset name used to namespace bench artifacts.
    pub dataset: String,
}

/// Options that control which scenarios are executed and whether to resume a prior run.
///
/// Build via [`RunOptions::default`] and override the fields you need.
///
/// # Examples
///
/// ```
/// use zeph_bench::runner::{RunOptions, MemoryMode};
///
/// // Run all scenarios.
/// let opts = RunOptions::default();
/// assert!(opts.scenario_filter.is_none());
/// assert!(opts.completed_ids.is_empty());
/// assert_eq!(opts.memory_mode, MemoryMode::Off);
/// ```
#[derive(Debug, Default)]
pub struct RunOptions {
    /// When `Some(id)`, only the scenario with this ID is executed.
    pub scenario_filter: Option<String>,
    /// Set of scenario IDs already completed in a prior run (used for `--resume`).
    pub completed_ids: HashSet<String>,
    /// Whether to wire a `SemanticMemory` backend into the agent for this run.
    pub memory_mode: MemoryMode,
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
/// scenario (baseline mode: no tools, no MCP). Memory is optionally wired via
/// [`BenchRunner::with_memory_params`] and [`RunOptions::memory_mode`].
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
    /// Parameters for constructing per-scenario `SQLite`-backed `SemanticMemory`.
    ///
    /// Set via [`BenchRunner::with_memory_params`]; required when
    /// `RunOptions::memory_mode == MemoryMode::On`.
    memory_params: Option<BenchMemoryParams>,
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
        Self {
            provider,
            memory_params: None,
        }
    }

    /// Attach `SemanticMemory` parameters for memory-on benchmark runs.
    ///
    /// When set, a per-scenario `SQLite`-backed `SemanticMemory` is constructed inside
    /// [`run_one`][BenchRunner::run_one] whenever `opts.memory_mode == MemoryMode::On`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::PathBuf;
    /// use zeph_bench::runner::{BenchRunner, BenchMemoryParams};
    /// use zeph_llm::{any::AnyProvider, mock::MockProvider};
    ///
    /// let provider = AnyProvider::Mock(MockProvider::with_responses(vec![]));
    /// let params = BenchMemoryParams {
    ///     data_dir: PathBuf::from("/tmp/bench-data"),
    ///     embedding_model: "nomic-embed-text".into(),
    ///     run_id: "bench-abc".into(),
    ///     dataset: "locomo".into(),
    /// };
    /// let runner = BenchRunner::new(provider, false).with_memory_params(params);
    /// ```
    #[must_use]
    pub fn with_memory_params(mut self, params: BenchMemoryParams) -> Self {
        self.memory_params = Some(params);
        self
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
            let response_text = Box::pin(self.run_one(scenario, opts.memory_mode)).await?;
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

    /// Run all scenarios from `path` through a per-scenario env executor and return a [`BenchRun`].
    ///
    /// This is the execution path for tool-driven datasets (tau2-bench). For each scenario:
    /// 1. Calls `env_factory(scenario)` to build a fresh `(ToolExecutor, ActionTrace)`.
    /// 2. Builds a fresh `TauBenchEvaluator` from the scenario metadata and the trace.
    /// 3. Runs the agent with the env executor and the tool-use system prompt.
    /// 4. Scores the response via the evaluator (reads the populated trace).
    ///
    /// # Errors
    ///
    /// Returns [`BenchError`] if the dataset cannot be loaded, the env factory fails, or
    /// `TauBenchEvaluator::from_scenario` fails (malformed metadata).
    pub async fn run_dataset_with_env_factory<L, F, X>(
        &self,
        loader: &L,
        env_factory: F,
        path: &Path,
        opts: RunOptions,
    ) -> Result<BenchRun, BenchError>
    where
        L: DatasetLoader,
        F: Fn(&Scenario) -> Result<(X, ActionTrace), BenchError>,
        X: ToolExecutor + Send + Sync + 'static,
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
            if opts.completed_ids.contains(&scenario.id) {
                continue;
            }
            if let Some(ref filter) = opts.scenario_filter
                && &scenario.id != filter
            {
                continue;
            }

            let (executor, trace) = env_factory(scenario)?;
            let evaluator = TauBenchEvaluator::from_scenario(scenario, trace)?;

            let t0 = Instant::now();
            let response_text = Box::pin(self.run_one_with_executor(
                scenario,
                executor,
                opts.memory_mode,
                ResponseMode::ToolUse,
            ))
            .await?;
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

    /// Run a single scenario through a fresh agent and return the last response text.
    ///
    /// A concise-answer system prompt is injected via [`InstructionBlock`] so the model
    /// responds with only the final answer (a number, word, or short phrase) rather than
    /// full sentences. The raw response is then post-processed to extract the first
    /// non-empty line and strip markdown formatting, which further reduces noise for
    /// evaluators that perform exact or near-exact matching.
    ///
    /// When `memory_mode == MemoryMode::On`, a per-scenario `SQLite`-backed
    /// `SemanticMemory` is constructed and wired into the agent. The database file is
    /// deleted after the scenario completes (best-effort, NFR-001).
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::InvalidFormat`] when the scenario has no user turn or when
    /// `SemanticMemory` initialisation fails.
    async fn run_one(
        &self,
        scenario: &Scenario,
        memory_mode: MemoryMode,
    ) -> Result<String, BenchError> {
        Box::pin(self.run_one_with_executor(
            scenario,
            NoopExecutor,
            memory_mode,
            ResponseMode::TerseAnswer,
        ))
        .await
    }

    /// Core execution: run one scenario with the given executor and response mode.
    ///
    /// Called by both [`BenchRunner::run_dataset`] (with `NoopExecutor` + `TerseAnswer`) and
    /// [`BenchRunner::run_dataset_with_env_factory`] (with the domain env + `ToolUse`).
    async fn run_one_with_executor<X: ToolExecutor + Send + Sync + 'static>(
        &self,
        scenario: &Scenario,
        executor: X,
        memory_mode: MemoryMode,
        mode: ResponseMode,
    ) -> Result<String, BenchError> {
        let prompt = scenario.primary_prompt()?.to_owned();
        let channel = BenchmarkChannel::new(vec![prompt]);
        // TODO(multi-turn-history): when loaders emit multiple user turns, push each in
        // order and seed assistant turns into the channel as captured-history.
        let registry = SkillRegistry::empty();

        let system_content = match mode {
            ResponseMode::TerseAnswer => concat!(
                "You are an evaluation assistant. ",
                "Answer every question with the shortest possible response. ",
                "Give only the final answer — no explanation, no full sentences, ",
                "no punctuation unless it is part of the answer. ",
                "If the answer is a single word or number, respond with only that word or number."
            ),
            ResponseMode::ToolUse => concat!(
                "You are a customer-service agent. ",
                "Use the available tools to help the user. ",
                "Always call a tool when one applies; do not ask the user to perform actions you can perform yourself. ",
                "When you have completed the user's request, respond with a brief confirmation."
            ),
        };

        let blocks = vec![InstructionBlock {
            source: PathBuf::from("<bench-system-prompt>"),
            content: system_content.to_owned(),
        }];

        let base_agent = Agent::new(self.provider.clone(), channel, registry, None, 1, executor)
            .with_instruction_blocks(blocks);

        // Optionally wire SemanticMemory when the caller requests memory-on mode.
        let (mut agent, scenario_db) = if memory_mode == MemoryMode::On
            && let Some(ref params) = self.memory_params
        {
            // One SQLite file per scenario gives strict isolation (NFR-001 choice (a)).
            // This is more files than a per-run DB, but eliminates any cross-scenario
            // memory bleed and avoids needing BenchIsolation::reset() between scenarios.
            let scenario_db = params
                .data_dir
                .join(format!("bench-{}-{}.db", params.run_id, scenario.id));
            debug_assert!(
                scenario_db.to_string_lossy().contains("bench-"),
                "NFR-001: bench SQLite path must be namespaced with 'bench-'"
            );

            tracing::debug!(
                scenario_id = %scenario.id,
                path = %scenario_db.display(),
                "bench: memory init start"
            );
            let memory = Arc::new(
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    SemanticMemory::with_sqlite_backend(
                        scenario_db.to_string_lossy().as_ref(),
                        self.provider.clone(),
                        &params.embedding_model,
                        0.7,
                        0.3,
                    ),
                )
                .await
                .map_err(|_| {
                    BenchError::InvalidFormat(format!(
                        "SemanticMemory init timed out for scenario '{}'",
                        scenario.id
                    ))
                })?
                .map_err(|e| BenchError::InvalidFormat(format!("SemanticMemory init: {e}")))?,
            );
            tracing::debug!(scenario_id = %scenario.id, "bench: memory init done");

            // Seed the sessions table so persist_message does not fail with FK violation.
            let conv_id = memory
                .sqlite()
                .create_conversation()
                .await
                .map_err(|e| BenchError::InvalidFormat(format!("create_conversation: {e}")))?;

            // summarization_threshold = 100_000 deliberately suppresses LLM-driven
            // compaction during bench runs. Compaction calls another LLM round-trip
            // with non-deterministic timing/output, which would violate FR-003
            // (deterministic runs). recall_limit = 20 is generous enough to surface
            // long-context memory effects without silently capping LongMemEval scores
            // below their theoretical maximum. history_limit = 200 covers the longest
            // LongMemEval session without truncation.
            let wired_agent = base_agent.with_memory(memory, conv_id, 200, 20, 100_000);
            (wired_agent, Some(scenario_db))
        } else {
            (base_agent, None)
        };

        // Ignore agent errors — a failed LLM call still yields an empty response that
        // the evaluator scores as 0.0 rather than aborting the entire run.
        let _ = agent.run().await;
        let responses = agent.into_channel().into_responses();

        // Best-effort cleanup: delete per-scenario SQLite file after the run.
        // Failure is intentionally ignored — NFR-001 is hygiene, not correctness.
        if let Some(ref db_path) = scenario_db {
            let _ = std::fs::remove_file(db_path);
        }

        let raw = responses
            .into_iter()
            .last()
            .map(|r| r.text)
            .unwrap_or_default();

        Ok(match mode {
            ResponseMode::TerseAnswer => post_process_response(&raw),
            // Verified: dropping send_tool_output does NOT affect the agent loop's tool-result
            // feedback to the LLM. Tool outputs flow via Agent's internal MessagePart::ToolResult,
            // not via the channel. See crates/zeph-core/src/agent/tool_execution/native.rs.
            ResponseMode::ToolUse => raw,
        })
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
        assert_eq!(opts.memory_mode, MemoryMode::Off);
    }

    #[test]
    fn memory_mode_default_is_off() {
        assert_eq!(MemoryMode::default(), MemoryMode::Off);
    }

    #[test]
    fn with_memory_params_sets_isolation() {
        use zeph_llm::{any::AnyProvider, mock::MockProvider};
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec![]));
        let params = BenchMemoryParams {
            data_dir: std::path::PathBuf::from("/tmp/bench-data"),
            embedding_model: "nomic-embed-text".into(),
            run_id: "bench-abc".into(),
            dataset: "locomo".into(),
        };
        let runner = BenchRunner::new(provider, false).with_memory_params(params.clone());
        assert!(runner.memory_params.is_some());
        let stored = runner.memory_params.unwrap();
        assert_eq!(stored.run_id, "bench-abc");
        assert_eq!(stored.dataset, "locomo");
    }

    #[test]
    fn nfr_001_sqlite_path_namespaced() {
        let params = BenchMemoryParams {
            data_dir: std::path::PathBuf::from("/tmp/bench-data"),
            embedding_model: "nomic-embed-text".into(),
            run_id: "run-xyz".into(),
            dataset: "locomo".into(),
        };
        let scenario_id = "s1_0";
        let scenario_db = params
            .data_dir
            .join(format!("bench-{}-{}.db", params.run_id, scenario_id));
        assert!(
            scenario_db.to_string_lossy().contains("bench-"),
            "NFR-001: SQLite path must contain bench- prefix"
        );
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
