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

/// Status of a benchmark run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Run completed normally.
    Completed,
    /// Run was interrupted before all scenarios finished.
    Interrupted,
    /// Run is in progress (should not appear in a persisted file).
    Running,
}

/// Per-scenario result record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// Unique identifier for the scenario.
    pub scenario_id: String,
    /// Numeric score in \[0.0, 1.0\].
    pub score: f64,
    /// First 200 characters of the agent response for quick review.
    pub response_excerpt: String,
    /// Error message if the scenario failed, otherwise `None`.
    pub error: Option<String>,
    /// Wall-clock time in milliseconds for this scenario.
    pub elapsed_ms: u64,
}

/// Aggregate statistics over all completed scenarios.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Aggregate {
    /// Number of scenarios that completed (included in mean score calculation).
    pub total: usize,
    /// Average score across all completed scenarios.
    pub mean_score: f64,
    /// Number of scenarios with score == 1.0.
    pub exact_match: usize,
    /// Total wall-clock time in milliseconds.
    pub total_elapsed_ms: u64,
}

/// Top-level benchmark run record — written to `results.json`.
///
/// Schema is a superset of the `LongMemEval` leaderboard submission format (NFR-008).
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
    /// Recompute `aggregate` from the current `results` list.
    pub fn recompute_aggregate(&mut self) {
        let total = self.results.len();
        #[allow(clippy::cast_precision_loss)]
        let mean_score = if total == 0 {
            0.0
        } else {
            self.results.iter().map(|r| r.score).sum::<f64>() / total as f64
        };
        let exact_match = self.results.iter().filter(|r| r.score >= 1.0).count();
        let total_elapsed_ms = self.results.iter().map(|r| r.elapsed_ms).sum();
        self.aggregate = Aggregate {
            total,
            mean_score,
            exact_match,
            total_elapsed_ms,
        };
    }

    /// Return the set of scenario IDs already present in `results`.
    #[must_use]
    pub fn completed_ids(&self) -> HashSet<String> {
        self.results.iter().map(|r| r.scenario_id.clone()).collect()
    }
}

/// Writes `results.json` and `summary.md` to an output directory.
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
            std::fs::create_dir(&output_dir)?;
        }
        Ok(Self { output_dir })
    }

    /// Path to `results.json` inside the output directory.
    #[must_use]
    pub fn results_path(&self) -> PathBuf {
        self.output_dir.join("results.json")
    }

    /// Path to `summary.md` inside the output directory.
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
            "- **Mean score**: {:.4} ({}/{} exact)\n",
            run.aggregate.mean_score, run.aggregate.exact_match, run.aggregate.total
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
        assert_eq!(run.aggregate.exact_match, 1);
        assert_eq!(run.aggregate.total_elapsed_ms, 6000);
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
