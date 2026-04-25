// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Benchmark result types and writer.
//!
//! [`BenchRun`] is the top-level result record written to `results.json`.
//! [`ResultWriter`] handles serialization to JSON and a human-readable Markdown summary,
//! including partial flushing on SIGINT and resume support.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::BenchError;

/// Status of a benchmark run serialized into `results.json`.
///
/// The `Running` variant is used in-memory during an active run and should never
/// appear in a persisted file.
///
/// # Examples
///
/// ```
/// use zeph_bench::RunStatus;
///
/// assert_ne!(RunStatus::Completed, RunStatus::Interrupted);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// All scenarios finished successfully.
    Completed,
    /// The run was cancelled (e.g. SIGINT) before all scenarios finished.
    Interrupted,
    /// The run is currently in progress; should not appear in a persisted file.
    Running,
}

/// Per-scenario result record persisted inside [`BenchRun::results`].
///
/// # Examples
///
/// ```
/// use zeph_bench::ScenarioResult;
///
/// let r = ScenarioResult {
///     scenario_id: "gaia_t1".into(),
///     score: 1.0,
///     response_excerpt: "1945".into(),
///     error: None,
///     elapsed_ms: 820,
/// };
/// assert!(r.error.is_none());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// Unique identifier for the scenario (matches [`crate::Scenario::id`]).
    pub scenario_id: String,
    /// Numeric score in `[0.0, 1.0]` produced by the evaluator.
    pub score: f64,
    /// First 200 characters of the agent response for quick review.
    pub response_excerpt: String,
    /// Error message if the scenario could not be completed, otherwise `None`.
    pub error: Option<String>,
    /// Wall-clock time in milliseconds for this scenario.
    pub elapsed_ms: u64,
}

/// Aggregate statistics computed from all [`ScenarioResult`]s in a [`BenchRun`].
///
/// Recomputed after every scenario via [`BenchRun::recompute_aggregate`] and persisted
/// into `results.json` so partial runs still contain meaningful statistics.
///
/// # Examples
///
/// ```
/// use zeph_bench::Aggregate;
///
/// let agg = Aggregate {
///     total: 100,
///     mean_score: 0.72,
///     median_score: 0.70,
///     stddev: 0.15,
///     exact_match: 55,
///     error_count: 3,
///     total_elapsed_ms: 240_000,
/// };
/// assert_eq!(agg.total, 100);
/// assert_eq!(agg.error_count, 3);
/// assert!((agg.median_score - 0.70).abs() < f64::EPSILON);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Aggregate {
    /// Number of scenarios included in the statistics.
    pub total: usize,
    /// Arithmetic mean of all per-scenario scores.
    pub mean_score: f64,
    /// Median per-scenario score.
    ///
    /// For an even number of results, the median is the average of the two middle values.
    /// Returns `0.0` when `total == 0`.
    pub median_score: f64,
    /// Population standard deviation of per-scenario scores (divide by N).
    ///
    /// The scenario set is treated as the full population of interest, not a sample.
    /// Returns `0.0` when `total <= 1`.
    pub stddev: f64,
    /// Count of scenarios where `score >= 1.0` (exact match).
    pub exact_match: usize,
    /// Count of scenarios where `score == 0.0` and `error` is `Some(_)`.
    ///
    /// A non-zero value indicates the agent failed to produce a response (e.g. timeout,
    /// LLM API error) rather than simply giving the wrong answer.
    pub error_count: usize,
    /// Sum of [`ScenarioResult::elapsed_ms`] across all scenarios.
    pub total_elapsed_ms: u64,
}

/// Top-level benchmark run record written to `results.json`.
///
/// The schema is a superset of the `LongMemEval` leaderboard submission format (NFR-008),
/// making it directly usable for leaderboard submission after a `longmemeval` run.
///
/// Create a default instance, then populate [`BenchRun::results`] incrementally and
/// call [`BenchRun::recompute_aggregate`] before persisting with [`ResultWriter`].
///
/// # Examples
///
/// ```
/// use zeph_bench::{BenchRun, RunStatus, Aggregate};
///
/// let run = BenchRun {
///     dataset: "gaia".into(),
///     model: "openai/gpt-4o".into(),
///     run_id: "a1b2c3".into(),
///     started_at: "2026-04-09T10:00:00Z".into(),
///     finished_at: String::new(),
///     status: RunStatus::Running,
///     results: vec![],
///     aggregate: Aggregate::default(),
/// };
/// assert_eq!(run.dataset, "gaia");
/// assert!(run.results.is_empty());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchRun {
    /// Dataset name (e.g. `"longmemeval"`).
    pub dataset: String,
    /// Provider/model identifier (e.g. `"openai/gpt-4o"`).
    pub model: String,
    /// UUID v4 uniquely identifying this run.
    pub run_id: String,
    /// RFC 3339 timestamp when the run started.
    pub started_at: String,
    /// RFC 3339 timestamp when the run ended (empty string if interrupted).
    pub finished_at: String,
    /// Run status.
    pub status: RunStatus,
    /// Per-scenario results.
    pub results: Vec<ScenarioResult>,
    /// Aggregate statistics.
    pub aggregate: Aggregate,
}

impl BenchRun {
    /// Recompute [`BenchRun::aggregate`] from the current [`BenchRun::results`] list.
    ///
    /// Call this after appending one or more [`ScenarioResult`]s to keep the
    /// aggregate statistics in sync before writing to disk.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_bench::{BenchRun, RunStatus, ScenarioResult, Aggregate};
    ///
    /// let mut run = BenchRun {
    ///     dataset: "frames".into(),
    ///     model: "openai/gpt-4o-mini".into(),
    ///     run_id: "r1".into(),
    ///     started_at: "2026-01-01T00:00:00Z".into(),
    ///     finished_at: String::new(),
    ///     status: RunStatus::Running,
    ///     results: vec![
    ///         ScenarioResult {
    ///             scenario_id: "frames_0".into(),
    ///             score: 1.0,
    ///             response_excerpt: "Paris".into(),
    ///             error: None,
    ///             elapsed_ms: 500,
    ///         },
    ///     ],
    ///     aggregate: Aggregate::default(),
    /// };
    ///
    /// run.recompute_aggregate();
    /// assert_eq!(run.aggregate.total, 1);
    /// assert!((run.aggregate.mean_score - 1.0).abs() < f64::EPSILON);
    /// assert_eq!(run.aggregate.exact_match, 1);
    /// assert_eq!(run.aggregate.error_count, 0);
    /// ```
    pub fn recompute_aggregate(&mut self) {
        let total = self.results.len();

        if total == 0 {
            self.aggregate = Aggregate::default();
            return;
        }

        #[allow(clippy::cast_precision_loss)]
        let mean_score = self.results.iter().map(|r| r.score).sum::<f64>() / total as f64;

        // Median: sort scores, average the two middle values for even N.
        let mut sorted_scores: Vec<f64> = self.results.iter().map(|r| r.score).collect();
        sorted_scores.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        #[allow(clippy::cast_precision_loss)]
        let median_score = if total % 2 == 1 {
            sorted_scores[total / 2]
        } else {
            f64::midpoint(sorted_scores[total / 2 - 1], sorted_scores[total / 2])
        };

        // Population standard deviation (divide by N).
        #[allow(clippy::cast_precision_loss)]
        let variance = self
            .results
            .iter()
            .map(|r| (r.score - mean_score).powi(2))
            .sum::<f64>()
            / total as f64;
        let stddev = variance.sqrt();

        let exact_match = self.results.iter().filter(|r| r.score >= 1.0).count();
        let error_count = self
            .results
            .iter()
            .filter(|r| r.score == 0.0 && r.error.is_some())
            .count();
        let total_elapsed_ms = self.results.iter().map(|r| r.elapsed_ms).sum();

        self.aggregate = Aggregate {
            total,
            mean_score,
            median_score,
            stddev,
            exact_match,
            error_count,
            total_elapsed_ms,
        };
    }

    /// Return the set of scenario IDs already present in [`BenchRun::results`].
    ///
    /// Used by the `--resume` logic to determine which scenarios can be skipped.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_bench::{BenchRun, RunStatus, ScenarioResult, Aggregate};
    ///
    /// let run = BenchRun {
    ///     dataset: "gaia".into(),
    ///     model: "openai/gpt-4o".into(),
    ///     run_id: "r2".into(),
    ///     started_at: "2026-01-01T00:00:00Z".into(),
    ///     finished_at: String::new(),
    ///     status: RunStatus::Interrupted,
    ///     results: vec![
    ///         ScenarioResult {
    ///             scenario_id: "t1".into(),
    ///             score: 1.0,
    ///             response_excerpt: "1945".into(),
    ///             error: None,
    ///             elapsed_ms: 300,
    ///         },
    ///     ],
    ///     aggregate: Aggregate::default(),
    /// };
    ///
    /// let done = run.completed_ids();
    /// assert!(done.contains("t1"));
    /// assert!(!done.contains("t2"));
    /// ```
    #[must_use]
    pub fn completed_ids(&self) -> HashSet<String> {
        self.results.iter().map(|r| r.scenario_id.clone()).collect()
    }
}

/// Writes `results.json` and `summary.md` to an output directory.
///
/// Files are written atomically by flushing to a `.tmp` sibling file and then
/// renaming, so a concurrent SIGINT cannot leave a half-written JSON file.
///
/// # Examples
///
/// ```no_run
/// use zeph_bench::{ResultWriter, BenchRun, RunStatus, Aggregate};
///
/// let writer = ResultWriter::new("/tmp/my-bench-run").unwrap();
/// println!("results at {}", writer.results_path().display());
/// ```
pub struct ResultWriter {
    output_dir: PathBuf,
}

impl ResultWriter {
    /// Create a writer targeting `output_dir`.
    ///
    /// The directory is created automatically (single level) if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Io`] if the directory cannot be created.
    pub fn new(output_dir: impl Into<PathBuf>) -> Result<Self, BenchError> {
        let output_dir = output_dir.into();
        if !output_dir.exists() {
            std::fs::create_dir_all(&output_dir)?;
        }
        Ok(Self { output_dir })
    }

    /// Absolute path of `results.json` inside the output directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::Path;
    /// use zeph_bench::ResultWriter;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let writer = ResultWriter::new(dir.path()).unwrap();
    /// assert!(writer.results_path().ends_with("results.json"));
    /// ```
    #[must_use]
    pub fn results_path(&self) -> PathBuf {
        self.output_dir.join("results.json")
    }

    /// Absolute path of `summary.md` inside the output directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_bench::ResultWriter;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let writer = ResultWriter::new(dir.path()).unwrap();
    /// assert!(writer.summary_path().ends_with("summary.md"));
    /// ```
    #[must_use]
    pub fn summary_path(&self) -> PathBuf {
        self.output_dir.join("summary.md")
    }

    /// Load an existing `results.json` for resume.
    ///
    /// Returns `None` when the file does not exist (treat as fresh run).
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Io`] on read failure, or [`BenchError::InvalidFormat`] if
    /// the file exists but cannot be deserialized.
    pub fn load_existing(&self) -> Result<Option<BenchRun>, BenchError> {
        let path = self.results_path();
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&path)?;
        let run: BenchRun =
            serde_json::from_str(&data).map_err(|e| BenchError::InvalidFormat(e.to_string()))?;
        Ok(Some(run))
    }

    /// Write `run` to `results.json` and `summary.md` atomically (best-effort).
    ///
    /// # Errors
    ///
    /// Returns [`BenchError`] on serialization or I/O failure.
    pub fn write(&self, run: &BenchRun) -> Result<(), BenchError> {
        self.write_json(run)?;
        self.write_markdown(run)?;
        Ok(())
    }

    fn write_json(&self, run: &BenchRun) -> Result<(), BenchError> {
        let json = serde_json::to_string_pretty(run)
            .map_err(|e| BenchError::InvalidFormat(e.to_string()))?;
        write_atomic(&self.results_path(), json.as_bytes())?;
        Ok(())
    }

    fn write_markdown(&self, run: &BenchRun) -> Result<(), BenchError> {
        let mut md = String::new();
        let _ = writeln!(md, "# Benchmark Results: {}\n", run.dataset);
        let _ = writeln!(md, "- **Model**: {}", run.model);
        let _ = writeln!(md, "- **Run ID**: {}", run.run_id);
        let _ = writeln!(md, "- **Status**: {:?}", run.status);
        let _ = writeln!(md, "- **Started**: {}", run.started_at);
        if !run.finished_at.is_empty() {
            let _ = writeln!(md, "- **Finished**: {}", run.finished_at);
        }
        let _ = writeln!(
            md,
            "- **Mean score**: {:.4} (median: {:.4}, stddev: {:.4})\n",
            run.aggregate.mean_score, run.aggregate.median_score, run.aggregate.stddev
        );
        let _ = writeln!(
            md,
            "- **Exact match**: {}/{} | **Errors**: {}\n",
            run.aggregate.exact_match, run.aggregate.total, run.aggregate.error_count
        );

        md.push_str("| scenario_id | score | response_excerpt | error |\n");
        md.push_str("|-------------|-------|------------------|-------|\n");
        for r in &run.results {
            let excerpt = r.response_excerpt.replace('|', "\\|");
            let error = r.error.as_deref().unwrap_or("").replace('|', "\\|");
            let _ = writeln!(
                md,
                "| {} | {:.4} | {} | {} |",
                r.scenario_id, r.score, excerpt, error
            );
        }

        write_atomic(&self.summary_path(), md.as_bytes())?;
        Ok(())
    }
}

/// Write `data` to `path` using a temp file + rename for atomicity.
fn write_atomic(path: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_run() -> BenchRun {
        BenchRun {
            dataset: "longmemeval".into(),
            model: "openai/gpt-4o".into(),
            run_id: "test-run-001".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: "2026-01-01T00:01:00Z".into(),
            status: RunStatus::Completed,
            results: vec![
                ScenarioResult {
                    scenario_id: "s1".into(),
                    score: 1.0,
                    response_excerpt: "The answer is 42.".into(),
                    error: None,
                    elapsed_ms: 1000,
                },
                ScenarioResult {
                    scenario_id: "s2".into(),
                    score: 0.0,
                    response_excerpt: String::new(),
                    error: Some("timeout".into()),
                    elapsed_ms: 5000,
                },
            ],
            aggregate: Aggregate::default(),
        }
    }

    #[test]
    fn recompute_aggregate_correct() {
        let mut run = make_run();
        run.recompute_aggregate();
        assert_eq!(run.aggregate.total, 2);
        assert!((run.aggregate.mean_score - 0.5).abs() < f64::EPSILON);
        // median for [0.0, 1.0] sorted = average of middle two = 0.5
        assert!((run.aggregate.median_score - 0.5).abs() < f64::EPSILON);
        // population stddev: mean=0.5, variance=((1.0-0.5)^2+(0.0-0.5)^2)/2 = 0.25, stddev=0.5
        assert!((run.aggregate.stddev - 0.5).abs() < f64::EPSILON);
        assert_eq!(run.aggregate.exact_match, 1);
        // s2 has score=0.0 and error=Some("timeout")
        assert_eq!(run.aggregate.error_count, 1);
        assert_eq!(run.aggregate.total_elapsed_ms, 6000);
    }

    #[test]
    fn recompute_aggregate_single_result() {
        let mut run = make_run();
        run.results.retain(|r| r.scenario_id == "s1");
        run.recompute_aggregate();
        assert_eq!(run.aggregate.total, 1);
        assert!((run.aggregate.mean_score - 1.0).abs() < f64::EPSILON);
        assert!((run.aggregate.median_score - 1.0).abs() < f64::EPSILON);
        assert!(run.aggregate.stddev.abs() < f64::EPSILON);
        assert_eq!(run.aggregate.error_count, 0);
    }

    #[test]
    fn recompute_aggregate_empty_results() {
        let mut run = make_run();
        run.results.clear();
        run.recompute_aggregate();
        assert_eq!(run.aggregate.total, 0);
        assert!(run.aggregate.mean_score.abs() < f64::EPSILON);
        assert!(run.aggregate.median_score.abs() < f64::EPSILON);
        assert!(run.aggregate.stddev.abs() < f64::EPSILON);
        assert_eq!(run.aggregate.error_count, 0);
    }

    #[test]
    fn recompute_aggregate_error_count_only_zero_score_with_error() {
        let mut run = make_run();
        // Add a scenario with score=0.0 but no error — should NOT count as error
        run.results.push(ScenarioResult {
            scenario_id: "s3".into(),
            score: 0.0,
            response_excerpt: "wrong answer".into(),
            error: None,
            elapsed_ms: 100,
        });
        run.recompute_aggregate();
        // s2 has error, s3 does not — error_count should be 1
        assert_eq!(run.aggregate.error_count, 1);
    }

    #[test]
    fn completed_ids_returns_all_scenario_ids() {
        let run = make_run();
        let ids = run.completed_ids();
        assert!(ids.contains("s1"));
        assert!(ids.contains("s2"));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn json_round_trip() {
        let mut run = make_run();
        run.recompute_aggregate();
        let json = serde_json::to_string_pretty(&run).unwrap();
        let decoded: BenchRun = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.dataset, run.dataset);
        assert_eq!(decoded.run_id, run.run_id);
        assert_eq!(decoded.results.len(), 2);
        assert_eq!(decoded.status, RunStatus::Completed);
        assert_eq!(decoded.aggregate.exact_match, run.aggregate.exact_match);
    }

    #[test]
    fn interrupted_status_serializes_correctly() {
        let mut run = make_run();
        run.status = RunStatus::Interrupted;
        let json = serde_json::to_string(&run).unwrap();
        assert!(json.contains("\"interrupted\""));
    }

    #[test]
    fn write_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let writer = ResultWriter::new(dir.path()).unwrap();

        assert!(writer.load_existing().unwrap().is_none());

        let mut run = make_run();
        run.recompute_aggregate();
        writer.write(&run).unwrap();

        let loaded = writer.load_existing().unwrap().unwrap();
        assert_eq!(loaded.run_id, run.run_id);
        assert_eq!(loaded.results.len(), 2);
        assert_eq!(loaded.aggregate.exact_match, 1);
    }

    #[test]
    fn summary_md_contains_table_header() {
        let dir = tempfile::tempdir().unwrap();
        let writer = ResultWriter::new(dir.path()).unwrap();
        let mut run = make_run();
        run.recompute_aggregate();
        writer.write(&run).unwrap();

        let md = std::fs::read_to_string(writer.summary_path()).unwrap();
        assert!(md.contains("| scenario_id | score |"));
        assert!(md.contains("s1"));
        assert!(md.contains("s2"));
    }

    #[test]
    fn write_creates_output_dir_if_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let new_dir = tmp.path().join("new_subdir");
        assert!(!new_dir.exists());
        ResultWriter::new(&new_dir).unwrap();
        assert!(new_dir.exists());
    }

    #[test]
    fn resume_skips_completed_scenarios() {
        let dir = tempfile::tempdir().unwrap();
        let writer = ResultWriter::new(dir.path()).unwrap();

        // Write partial results (only s1 done).
        let mut partial = make_run();
        partial.results.retain(|r| r.scenario_id == "s1");
        partial.status = RunStatus::Interrupted;
        partial.recompute_aggregate();
        writer.write(&partial).unwrap();

        let loaded = writer.load_existing().unwrap().unwrap();
        let done = loaded.completed_ids();
        assert!(done.contains("s1"));
        assert!(!done.contains("s2"));
    }
}
