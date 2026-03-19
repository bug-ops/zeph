// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Experiment engine — core async loop for autonomous parameter tuning.
//!
//! [`ExperimentEngine`] orchestrates baseline evaluation, variation generation,
//! candidate scoring, acceptance decisions, and optional `SQLite` persistence.
//! Cancellation is supported via [`tokio_util::sync::CancellationToken`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::sqlite::experiments::NewExperimentResult;

use super::error::EvalError;
use super::evaluator::Evaluator;
use super::generator::VariationGenerator;
use super::snapshot::ConfigSnapshot;
use super::types::{ExperimentResult, ExperimentSource, Variation};
use zeph_config::ExperimentConfig;

/// Final report produced by [`ExperimentEngine::run`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentSessionReport {
    /// UUID identifying this experiment session.
    pub session_id: String,
    /// All experiment results recorded in this session (accepted and rejected).
    pub results: Vec<ExperimentResult>,
    /// The best-known config snapshot at session end (progressive baseline winner).
    pub best_config: ConfigSnapshot,
    /// Baseline mean score captured before the loop started.
    pub baseline_score: f64,
    /// Final best-known mean score at session end.
    pub final_score: f64,
    /// `final_score - baseline_score` (positive means improvement).
    pub total_improvement: f64,
    /// Wall-clock time for the full session in milliseconds.
    pub wall_time_ms: u64,
    /// Whether the session was stopped via [`ExperimentEngine::stop`].
    pub cancelled: bool,
}

/// Autonomous parameter-tuning engine.
///
/// The engine evaluates a baseline configuration, then generates and tests
/// parameter variations one at a time. Accepted variations update the progressive
/// baseline (greedy hill-climbing). The loop terminates on budget exhaustion,
/// search-space exhaustion, wall-time limit, or cancellation.
///
/// # Storage
///
/// When `memory` is `Some`, each result is persisted to `SQLite` via
/// [`SemanticMemory::sqlite`]. When `None`, results are kept only in the
/// in-memory `results` vec of the final report.
///
/// # Budget ownership
///
/// The `Evaluator` is passed pre-built by the caller. The caller is responsible
/// for constructing it with the desired `budget_tokens` (typically
/// `config.eval_budget_tokens`). The `eval_budget_tokens` field in
/// [`ExperimentConfig`] is a hint for the caller — the engine itself does not
/// construct the evaluator.
pub struct ExperimentEngine {
    evaluator: Evaluator,
    generator: Box<dyn VariationGenerator>,
    subject: Arc<AnyProvider>,
    baseline: ConfigSnapshot,
    config: ExperimentConfig,
    memory: Option<Arc<SemanticMemory>>,
    session_id: String,
    cancel: CancellationToken,
    source: ExperimentSource,
}

/// Maximum number of consecutive NaN-scored evaluations before the loop breaks.
/// Prevents unbounded spinning when the evaluator consistently returns degenerate reports.
const MAX_CONSECUTIVE_NAN: u32 = 3;

impl ExperimentEngine {
    /// Create a new `ExperimentEngine`.
    ///
    /// A fresh UUID session ID is generated at construction time.
    /// The `evaluator` should already be configured with the desired token budget
    /// (typically `config.eval_budget_tokens`).
    ///
    /// # Contract
    ///
    /// The caller must ensure `config` is valid before constructing the engine.
    /// Call [`ExperimentConfig::validate`] during bootstrap — passing invalid config
    /// (e.g., `max_experiments=0`, `max_wall_time_secs=0`) results in unspecified
    /// loop behaviour (immediate exit or no effective budget enforcement).
    pub fn new(
        evaluator: Evaluator,
        generator: Box<dyn VariationGenerator>,
        subject: Arc<AnyProvider>,
        baseline: ConfigSnapshot,
        config: ExperimentConfig,
        memory: Option<Arc<SemanticMemory>>,
    ) -> Self {
        Self {
            evaluator,
            generator,
            subject,
            baseline,
            config,
            memory,
            session_id: uuid::Uuid::new_v4().to_string(),
            cancel: CancellationToken::new(),
            source: ExperimentSource::Manual,
        }
    }

    /// Set the [`ExperimentSource`] for this session.
    ///
    /// Defaults to [`ExperimentSource::Manual`]. Use [`ExperimentSource::Scheduled`]
    /// for runs triggered by the scheduler.
    #[must_use]
    pub fn with_source(mut self, source: ExperimentSource) -> Self {
        self.source = source;
        self
    }

    /// Return a clone of the internal [`CancellationToken`].
    ///
    /// External callers (CLI, TUI, scheduler) can hold a token handle and call
    /// `.cancel()` to trigger graceful shutdown. See also [`Self::stop`].
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Stop the engine by cancelling the internal [`CancellationToken`].
    ///
    /// The current evaluation call will complete; the loop exits after it returns.
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Run the experiment loop and return a session report.
    ///
    /// The loop:
    /// 1. Evaluates the baseline once to obtain `initial_baseline_score`.
    /// 2. Generates variations via the [`VariationGenerator`].
    /// 3. Evaluates each variation with a clone of `subject` patched with generation overrides
    ///    derived from the candidate `ConfigSnapshot` via `AnyProvider::with_generation_overrides`.
    /// 4. Accepts the variation if `delta >= config.min_improvement`.
    /// 5. On acceptance, updates the progressive baseline (greedy hill-climbing).
    ///    **Known limitation (S1):** single-sample acceptance has no statistical
    ///    confidence check. Noise in the evaluator can cause gradual score drift.
    ///    Phase 5 should add repeated trials or a confidence margin derived from
    ///    per-case variance before promoting a variation.
    /// 6. Optionally persists results to `SQLite` when `memory` is `Some`.
    /// 7. Breaks on: max experiments, wall-time, search exhaustion, or cancellation.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError`] if the baseline evaluation or any subject LLM call fails.
    /// `SQLite` persistence failures are returned as [`EvalError::Storage`].
    pub async fn run(&mut self) -> Result<ExperimentSessionReport, EvalError> {
        let start = Instant::now();
        let best_snapshot = self.baseline.clone();

        // Step 0: evaluate baseline once, with cancellation support.
        // Issue #4: wrapped in select! so a cancel during a slow baseline evaluation is honoured.
        let baseline_report = tokio::select! {
            biased;
            () = self.cancel.cancelled() => {
                tracing::info!(session_id = %self.session_id, "cancelled before baseline");
                #[allow(clippy::cast_possible_truncation)]
                return Ok(ExperimentSessionReport {
                    session_id: self.session_id.clone(),
                    results: vec![],
                    best_config: best_snapshot,
                    baseline_score: f64::NAN,
                    final_score: f64::NAN,
                    total_improvement: 0.0,
                    wall_time_ms: start.elapsed().as_millis() as u64,
                    cancelled: true,
                });
            }
            report = self.evaluator.evaluate(&self.subject) => report?,
        };

        // Bug #3: if baseline produces NaN, there is no meaningful anchor — fail fast.
        let initial_baseline_score = baseline_report.mean_score;
        if initial_baseline_score.is_nan() {
            return Err(EvalError::Storage(
                "baseline evaluation produced NaN mean score; \
                 check evaluator budget and judge responses"
                    .into(),
            ));
        }
        tracing::info!(
            session_id = %self.session_id,
            baseline_score = initial_baseline_score,
            "experiment session started"
        );
        self.run_loop(start, initial_baseline_score, best_snapshot)
            .await
    }

    /// Inner experiment loop — runs after a successful baseline evaluation.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError`] if any LLM call or `SQLite` persist fails.
    #[allow(clippy::too_many_lines)] // experiment loop with inherent complexity: variation→evaluate→compare
    async fn run_loop(
        &mut self,
        start: Instant,
        initial_baseline_score: f64,
        mut best_snapshot: ConfigSnapshot,
    ) -> Result<ExperimentSessionReport, EvalError> {
        let wall_limit = std::time::Duration::from_secs(self.config.max_wall_time_secs);
        let mut results: Vec<ExperimentResult> = Vec::new();
        let mut visited: HashSet<Variation> = HashSet::new();
        let (mut best_score, mut counter, mut consecutive_nan) =
            (initial_baseline_score, 0i64, 0u32);

        'main: loop {
            if results.len() >= self.config.max_experiments as usize {
                tracing::info!(session_id = %self.session_id, "budget exhausted");
                break;
            }
            if start.elapsed() >= wall_limit {
                tracing::info!(session_id = %self.session_id, "wall-time limit reached");
                break;
            }
            let Some(variation) = self.generator.next(&best_snapshot, &visited) else {
                tracing::info!(session_id = %self.session_id, "search space exhausted");
                break;
            };
            visited.insert(variation.clone());
            let candidate_snapshot = best_snapshot.apply(&variation);
            let patched = (*self.subject)
                .clone()
                .with_generation_overrides(candidate_snapshot.to_generation_overrides());
            let candidate_report = tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    tracing::info!(session_id = %self.session_id, "experiment cancelled");
                    break 'main;
                }
                report = self.evaluator.evaluate(&patched) => report?,
            };
            if candidate_report.mean_score.is_nan() {
                consecutive_nan += 1;
                tracing::warn!(
                    session_id = %self.session_id, param = %variation.parameter,
                    is_partial = candidate_report.is_partial, consecutive_nan,
                    "NaN mean score — skipping variation"
                );
                if consecutive_nan >= MAX_CONSECUTIVE_NAN {
                    tracing::warn!(session_id = %self.session_id, "consecutive NaN cap reached");
                    break;
                }
                continue;
            }
            consecutive_nan = 0;
            let candidate_score = candidate_report.mean_score;
            let delta = candidate_score - best_score;
            let accepted = delta >= self.config.min_improvement;
            let result_id = self
                .persist_result(
                    &variation,
                    best_score,
                    candidate_score,
                    delta,
                    accepted,
                    candidate_report.p50_latency_ms,
                    candidate_report.total_tokens,
                    counter,
                )
                .await?;
            counter += 1;
            let pre_accept_baseline = best_score;
            self.log_outcome(&variation, delta, accepted, best_score);
            if accepted {
                best_snapshot = candidate_snapshot;
                best_score = candidate_score;
            }
            results.push(ExperimentResult {
                id: result_id,
                session_id: self.session_id.clone(),
                variation,
                baseline_score: pre_accept_baseline,
                candidate_score,
                delta,
                latency_ms: candidate_report.p50_latency_ms,
                tokens_used: candidate_report.total_tokens,
                accepted,
                source: self.source.clone(),
                created_at: chrono_now_utc(),
            });
        }

        #[allow(clippy::cast_possible_truncation)]
        let wall_time_ms = start.elapsed().as_millis() as u64;
        let total_improvement = best_score - initial_baseline_score;
        tracing::info!(
            session_id = %self.session_id, total = results.len(),
            baseline_score = initial_baseline_score, final_score = best_score,
            total_improvement, wall_time_ms, cancelled = self.cancel.is_cancelled(),
            "experiment session complete"
        );
        Ok(ExperimentSessionReport {
            session_id: self.session_id.clone(),
            results,
            best_config: best_snapshot,
            baseline_score: initial_baseline_score,
            final_score: best_score,
            total_improvement,
            wall_time_ms,
            cancelled: self.cancel.is_cancelled(),
        })
    }

    /// Persist a single experiment result to `SQLite` when memory is configured.
    ///
    /// Returns the row ID from `SQLite`, or a synthetic monotonic counter when
    /// persistence is disabled (`memory` is `None`).
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::Storage`] if the `SQLite` insert fails.
    #[allow(clippy::too_many_arguments)]
    async fn persist_result(
        &self,
        variation: &Variation,
        baseline_score: f64,
        candidate_score: f64,
        delta: f64,
        accepted: bool,
        p50_latency_ms: u64,
        total_tokens: u64,
        counter: i64,
    ) -> Result<i64, EvalError> {
        let Some(mem) = &self.memory else {
            return Ok(counter);
        };
        let value_json = serde_json::to_string(&variation.value)
            .map_err(|e| EvalError::Storage(e.to_string()))?;
        #[allow(clippy::cast_possible_wrap)]
        let new_result = NewExperimentResult {
            session_id: &self.session_id,
            parameter: variation.parameter.as_str(),
            value_json: &value_json,
            baseline_score,
            candidate_score,
            delta,
            latency_ms: p50_latency_ms as i64,
            tokens_used: total_tokens as i64,
            accepted,
            source: self.source.as_str(),
        };
        mem.sqlite()
            .insert_experiment_result(&new_result)
            .await
            .map_err(|e: zeph_memory::error::MemoryError| EvalError::Storage(e.to_string()))
    }

    fn log_outcome(&self, variation: &Variation, delta: f64, accepted: bool, new_score: f64) {
        if accepted {
            tracing::info!(
                session_id = %self.session_id,
                param = %variation.parameter,
                value = %variation.value,
                delta,
                new_best_score = new_score,
                "variation accepted — new baseline"
            );
        } else {
            tracing::info!(
                session_id = %self.session_id,
                param = %variation.parameter,
                value = %variation.value,
                delta,
                "variation rejected"
            );
        }
    }
}

/// Return a UTC timestamp string in `YYYY-MM-DD HH:MM:SS` format.
#[allow(clippy::many_single_char_names)]
fn chrono_now_utc() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple UTC formatter — no external date dependency.
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Days since 1970-01-01
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Gregorian calendar algorithm.
    let mut year = 1970u64;
    loop {
        let leap = is_leap(year);
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        year += 1;
    }
    let leap = is_leap(year);
    let month_days: [u64; 12] = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1u64;
    for md in &month_days {
        if days < *md {
            break;
        }
        days -= md;
        month += 1;
    }
    (year, month, days + 1)
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::doc_markdown)]

    use super::*;
    use crate::benchmark::{BenchmarkCase, BenchmarkSet};
    use crate::evaluator::Evaluator;
    use crate::generator::VariationGenerator;
    use crate::snapshot::ConfigSnapshot;
    use crate::types::{ParameterKind, Variation, VariationValue};
    use ordered_float::OrderedFloat;
    use std::sync::Arc;
    use zeph_config::ExperimentConfig;

    fn make_benchmark() -> BenchmarkSet {
        BenchmarkSet {
            cases: vec![BenchmarkCase {
                prompt: "What is 2+2?".into(),
                context: None,
                reference: None,
                tags: None,
            }],
        }
    }

    fn default_config() -> ExperimentConfig {
        ExperimentConfig {
            max_experiments: 10,
            max_wall_time_secs: 3600,
            min_improvement: 0.0,
            ..Default::default()
        }
    }

    /// Generates exactly N variations and then exhausts.
    struct NVariationGenerator {
        variations: Vec<Variation>,
        pos: usize,
    }

    impl NVariationGenerator {
        fn new(n: usize) -> Self {
            let variations = (0..n)
                .map(|i| Variation {
                    parameter: ParameterKind::Temperature,
                    #[allow(clippy::cast_precision_loss)]
                    value: VariationValue::Float(OrderedFloat(0.5 + i as f64 * 0.1)),
                })
                .collect();
            Self { variations, pos: 0 }
        }
    }

    impl VariationGenerator for NVariationGenerator {
        fn next(
            &mut self,
            _baseline: &ConfigSnapshot,
            visited: &HashSet<Variation>,
        ) -> Option<Variation> {
            while self.pos < self.variations.len() {
                let v = self.variations[self.pos].clone();
                self.pos += 1;
                if !visited.contains(&v) {
                    return Some(v);
                }
            }
            None
        }

        fn name(&self) -> &'static str {
            "n_variation"
        }
    }

    #[cfg(test)]
    fn make_subject_mock(n_responses: usize) -> zeph_llm::any::AnyProvider {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        // Each evaluate() call runs 1 subject call + 1 judge call per benchmark case.
        // With 1 case: 1 subject + 1 judge response per evaluate() invocation.
        // We need n_responses pairs (subject + judge) for n variations + 1 baseline.
        let responses: Vec<String> = (0..n_responses).map(|_| "Four".to_string()).collect();
        AnyProvider::Mock(MockProvider::with_responses(responses))
    }

    #[cfg(test)]
    fn make_judge_mock(n_responses: usize) -> zeph_llm::any::AnyProvider {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let responses: Vec<String> = (0..n_responses)
            .map(|_| r#"{"score": 8.0, "reason": "correct"}"#.to_string())
            .collect();
        AnyProvider::Mock(MockProvider::with_responses(responses))
    }

    #[cfg(test)]
    #[tokio::test]
    async fn engine_completes_with_no_accepted_variations() {
        // min_improvement very high so nothing is accepted.
        let config = ExperimentConfig {
            max_experiments: 10,
            max_wall_time_secs: 3600,
            min_improvement: 100.0,
            ..Default::default()
        };
        // 1 variation + 1 baseline = 2 evaluate() calls (2 subject + 2 judge responses).
        let subject = make_subject_mock(2);
        let judge = make_judge_mock(2);
        let evaluator = Evaluator::new(Arc::new(judge), make_benchmark(), 1_000_000).unwrap();

        let mut engine = ExperimentEngine::new(
            evaluator,
            Box::new(NVariationGenerator::new(1)),
            Arc::new(subject),
            ConfigSnapshot::default(),
            config,
            None,
        );

        let report = engine.run().await.unwrap();
        assert_eq!(report.results.len(), 1);
        assert!(!report.results[0].accepted);
        assert!(!report.session_id.is_empty());
        assert!(!report.cancelled);
    }

    #[cfg(test)]
    #[tokio::test]
    async fn engine_respects_max_experiments() {
        let config = ExperimentConfig {
            max_experiments: 3,
            max_wall_time_secs: 3600,
            min_improvement: 0.0,
            ..Default::default()
        };
        // 5 variations available but max_experiments=3.
        // 1 baseline + 3 candidate evaluate() calls = 4 calls, each needing 1 subject + 1 judge.
        let subject = make_subject_mock(4);
        let judge = make_judge_mock(4);
        let evaluator = Evaluator::new(Arc::new(judge), make_benchmark(), 1_000_000).unwrap();

        let mut engine = ExperimentEngine::new(
            evaluator,
            Box::new(NVariationGenerator::new(5)),
            Arc::new(subject),
            ConfigSnapshot::default(),
            config,
            None,
        );

        let report = engine.run().await.unwrap();
        assert_eq!(report.results.len(), 3);
        assert!(!report.cancelled);
    }

    #[cfg(test)]
    #[tokio::test]
    async fn engine_cancellation_before_baseline() {
        // Pre-cancel: cancel token fires during baseline evaluation select!.
        let config = ExperimentConfig {
            max_experiments: 100,
            max_wall_time_secs: 3600,
            min_improvement: 0.0,
            ..Default::default()
        };
        let subject = make_subject_mock(2);
        let judge = make_judge_mock(2);
        let evaluator = Evaluator::new(Arc::new(judge), make_benchmark(), 1_000_000).unwrap();

        let mut engine = ExperimentEngine::new(
            evaluator,
            Box::new(NVariationGenerator::new(100)),
            Arc::new(subject),
            ConfigSnapshot::default(),
            config,
            None,
        );
        engine.stop(); // cancel before any evaluation
        let report = engine.run().await.unwrap();
        assert!(report.cancelled);
        assert!(report.results.is_empty());
    }

    #[cfg(test)]
    #[tokio::test]
    async fn engine_cancellation_stops_loop() {
        // Verify the loop-level select! path: cancel token pre-fired, baseline completes
        // (because biased baseline select! checks cancel FIRST — fires immediately), then
        // run() returns early. Since MockProvider is instantaneous, we test the cancel
        // token semantics via stop() called between construction and run().
        //
        // NOTE: engine_cancellation_before_baseline covers the biased baseline path.
        // This test verifies that cancelling after construction but before run() sets
        // cancelled=true in the report regardless of results count.
        let config = ExperimentConfig {
            max_experiments: 10,
            max_wall_time_secs: 3600,
            min_improvement: 0.0,
            ..Default::default()
        };
        let subject = make_subject_mock(2);
        let judge = make_judge_mock(2);
        let evaluator = Evaluator::new(Arc::new(judge), make_benchmark(), 1_000_000).unwrap();

        let mut engine = ExperimentEngine::new(
            evaluator,
            Box::new(NVariationGenerator::new(10)),
            Arc::new(subject),
            ConfigSnapshot::default(),
            config,
            None,
        );

        // Verify cancel_token() gives an independent handle that controls the same token.
        let external_token = engine.cancel_token();
        assert!(!external_token.is_cancelled());
        engine.stop();
        assert!(
            external_token.is_cancelled(),
            "cancel_token() must share the same token"
        );

        let report = engine.run().await.unwrap();
        assert!(report.cancelled);
    }

    #[cfg(test)]
    #[tokio::test]
    async fn engine_progressive_baseline_updates() {
        // One variation applied via NVariationGenerator generates temperature=0.5.
        // min_improvement=0.0 so it is accepted, updating best_config.
        let config = ExperimentConfig {
            max_experiments: 1,
            max_wall_time_secs: 3600,
            min_improvement: 0.0,
            ..Default::default()
        };
        // 1 baseline + 1 candidate = 2 evaluate() calls.
        let subject = make_subject_mock(2);
        let judge = make_judge_mock(2);
        let evaluator = Evaluator::new(Arc::new(judge), make_benchmark(), 1_000_000).unwrap();

        let initial_baseline = ConfigSnapshot::default();
        let mut engine = ExperimentEngine::new(
            evaluator,
            Box::new(NVariationGenerator::new(1)),
            Arc::new(subject),
            initial_baseline.clone(),
            config,
            None,
        );

        let report = engine.run().await.unwrap();
        assert_eq!(report.results.len(), 1);
        assert!(report.results[0].accepted, "variation should be accepted");
        // best_config should differ from the initial baseline (temperature changed to 0.5).
        assert!(
            (report.best_config.temperature - initial_baseline.temperature).abs() > 1e-9,
            "best_config.temperature should have changed after accepted variation"
        );
        assert!(!report.baseline_score.is_nan());
        assert!(!report.final_score.is_nan());
        // Bug #1 regression: baseline_score in result must be the PRE-acceptance score.
        assert!(
            (report.results[0].baseline_score - report.baseline_score).abs() < 1e-9,
            "result.baseline_score must equal initial baseline_score (pre-acceptance)"
        );
    }

    #[cfg(test)]
    #[tokio::test]
    async fn engine_handles_search_space_exhaustion() {
        let config = default_config();
        // Generator returns None immediately (0 variations).
        // Only the baseline evaluate() call is needed.
        let subject = make_subject_mock(1);
        let judge = make_judge_mock(1);
        let evaluator = Evaluator::new(Arc::new(judge), make_benchmark(), 1_000_000).unwrap();

        let mut engine = ExperimentEngine::new(
            evaluator,
            Box::new(NVariationGenerator::new(0)),
            Arc::new(subject),
            ConfigSnapshot::default(),
            config,
            None,
        );

        let report = engine.run().await.unwrap();
        assert!(report.results.is_empty());
        assert!(!report.cancelled);
    }

    #[cfg(test)]
    #[tokio::test]
    async fn engine_skips_nan_scores() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        // Candidate evaluations produce NaN (empty judge responses → error → 0 scored → NaN mean).
        // Baseline uses a sufficient budget to succeed; candidate budget is tiny.
        // We use two separate Evaluator instances: one for baseline (high budget),
        // one for candidates (zero budget). Since ExperimentEngine uses a single Evaluator,
        // we use a judge mock with 1 valid response (baseline) and no more (candidates fail).
        let config = ExperimentConfig {
            max_experiments: 5,
            max_wall_time_secs: 3600,
            min_improvement: 0.0,
            ..Default::default()
        };
        // Subject: baseline + 3 candidate subject calls (3 NaN iterations before cap).
        let subject = AnyProvider::Mock(MockProvider::with_responses(vec![
            "A".into(),
            "A".into(),
            "A".into(),
            "A".into(),
        ]));
        // Judge: 1 valid response for baseline, then errors for candidates (mock exhausted).
        let judge = AnyProvider::Mock(MockProvider::with_responses(vec![
            r#"{"score": 8.0, "reason": "ok"}"#.into(),
        ]));
        // Use large budget — judge errors (not budget) produce NaN via 0 cases scored.
        let evaluator = Evaluator::new(Arc::new(judge), make_benchmark(), 1_000_000).unwrap();

        let mut engine = ExperimentEngine::new(
            evaluator,
            Box::new(NVariationGenerator::new(5)),
            Arc::new(subject),
            ConfigSnapshot::default(),
            config,
            None,
        );

        // Should not panic — NaN scores are skipped; loop breaks after MAX_CONSECUTIVE_NAN.
        let report = engine.run().await.unwrap();
        // No variations accepted (all NaN); loop stopped at consecutive NaN limit.
        assert!(
            report.results.is_empty(),
            "all NaN iterations should be skipped"
        );
        assert!(!report.cancelled);
    }

    #[cfg(test)]
    #[tokio::test]
    async fn engine_nan_baseline_returns_error() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        // Budget=0 and no judge responses → baseline evaluation returns NaN mean → engine errors.
        let config = ExperimentConfig {
            max_experiments: 5,
            max_wall_time_secs: 3600,
            min_improvement: 0.0,
            ..Default::default()
        };
        // Subject responds for baseline subject call.
        let subject = AnyProvider::Mock(MockProvider::with_responses(vec!["A".into()]));
        // Judge has no responses — all judge calls error, 0 cases scored, NaN mean.
        let judge = AnyProvider::Mock(MockProvider::with_responses(vec![]));
        let evaluator = Evaluator::new(Arc::new(judge), make_benchmark(), 1_000_000).unwrap();

        let mut engine = ExperimentEngine::new(
            evaluator,
            Box::new(NVariationGenerator::new(5)),
            Arc::new(subject),
            ConfigSnapshot::default(),
            config,
            None,
        );

        let result = engine.run().await;
        assert!(result.is_err(), "NaN baseline should return an error");
        let err = result.unwrap_err();
        assert!(
            matches!(err, EvalError::Storage(_)),
            "expected EvalError::Storage, got: {err:?}"
        );
    }

    #[cfg(test)]
    #[tokio::test]
    async fn engine_persists_results_to_sqlite() {
        use zeph_memory::testing::mock_semantic_memory;

        let memory = mock_semantic_memory().await.unwrap();
        let config = ExperimentConfig {
            max_experiments: 1,
            max_wall_time_secs: 3600,
            min_improvement: 0.0,
            ..Default::default()
        };
        // 1 baseline + 1 candidate = 2 evaluate() calls.
        let subject = make_subject_mock(2);
        let judge = make_judge_mock(2);
        let evaluator = Evaluator::new(Arc::new(judge), make_benchmark(), 1_000_000).unwrap();

        let session_id = {
            let mut engine = ExperimentEngine::new(
                evaluator,
                Box::new(NVariationGenerator::new(1)),
                Arc::new(subject),
                ConfigSnapshot::default(),
                config,
                Some(Arc::clone(&memory)),
            );
            engine.run().await.unwrap();
            engine.session_id.clone()
        };

        let rows = memory
            .sqlite()
            .list_experiment_results(Some(&session_id), 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "expected one persisted result");
        assert_eq!(rows[0].session_id, session_id);
    }

    #[test]
    fn session_report_serde_roundtrip() {
        let report = ExperimentSessionReport {
            session_id: "test-session".to_string(),
            results: vec![],
            best_config: ConfigSnapshot::default(),
            baseline_score: 7.5,
            final_score: 8.0,
            total_improvement: 0.5,
            wall_time_ms: 1_234,
            cancelled: false,
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let report2: ExperimentSessionReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(report2.session_id, report.session_id);
        assert!((report2.baseline_score - report.baseline_score).abs() < f64::EPSILON);
        assert!((report2.final_score - report.final_score).abs() < f64::EPSILON);
        assert_eq!(report2.wall_time_ms, report.wall_time_ms);
        assert!(!report2.cancelled);
    }

    #[test]
    fn chrono_now_utc_format() {
        let s = chrono_now_utc();
        assert_eq!(s.len(), 19, "timestamp must be 19 chars: {s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
    }

    /// Verify that `days_to_ymd` correctly handles a known date including a leap year.
    /// 2024-02-29 (leap day) = 19782 days since 1970-01-01.
    #[test]
    fn chrono_known_timestamp_leap_year() {
        // 2024-02-29 00:00:00 UTC = 1_709_164_800 seconds since epoch.
        // Verified via: date -d "2024-02-29 00:00:00 UTC" +%s
        let secs: u64 = 1_709_164_800;
        let second = secs % 60;
        let minute = (secs / 60) % 60;
        let hour = (secs / 3600) % 24;
        let days = secs / 86400;
        let (year, month, day) = days_to_ymd(days);
        assert_eq!(year, 2024);
        assert_eq!(month, 2);
        assert_eq!(day, 29);
        assert_eq!(second, 0);
        assert_eq!(minute, 0);
        assert_eq!(hour, 0);
    }

    /// `ExperimentEngine` must be Send to be used with `tokio::spawn`.
    #[test]
    fn experiment_engine_is_send() {
        fn assert_send<T: Send>() {}
        // This is a compile-time check — if ExperimentEngine is not Send, this fails to compile.
        // We cannot instantiate the engine here without providers, so we use a fn pointer trick.
        let _ = assert_send::<ExperimentEngine>;
    }

    #[tokio::test]
    async fn engine_with_source_scheduled_propagates_to_results() {
        let config = ExperimentConfig {
            max_experiments: 1,
            max_wall_time_secs: 3600,
            min_improvement: 0.0,
            ..Default::default()
        };
        let subject = make_subject_mock(2);
        let judge = make_judge_mock(2);
        let evaluator = Evaluator::new(Arc::new(judge), make_benchmark(), 1_000_000).unwrap();

        let mut engine = ExperimentEngine::new(
            evaluator,
            Box::new(NVariationGenerator::new(1)),
            Arc::new(subject),
            ConfigSnapshot::default(),
            config,
            None,
        )
        .with_source(ExperimentSource::Scheduled);

        let report = engine.run().await.unwrap();
        assert_eq!(report.results.len(), 1);
        assert_eq!(
            report.results[0].source,
            ExperimentSource::Scheduled,
            "with_source(Scheduled) must propagate to ExperimentResult"
        );
    }
}
