// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{BenchError, BenchRun};

/// Score delta for a single scenario between memory-on and memory-off runs.
///
/// Produced by [`BaselineComparison::compute`] for each scenario that appears
/// in both runs.
///
/// # Examples
///
/// ```
/// use zeph_bench::baseline::ScenarioDelta;
///
/// let delta = ScenarioDelta {
///     scenario_id: "q_001".into(),
///     score_with_memory: 1.0,
///     score_without_memory: 0.5,
///     delta: 0.5,
/// };
/// assert!(delta.delta > 0.0, "positive delta means memory helped");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioDelta {
    /// Scenario identifier (matches [`crate::Scenario::id`]).
    pub scenario_id: String,
    /// Score from the memory-on run.
    pub score_with_memory: f64,
    /// Score from the memory-off run.
    pub score_without_memory: f64,
    /// `score_with_memory - score_without_memory`. Positive = memory helped.
    pub delta: f64,
}

/// Comparison between two benchmark runs (memory-on vs memory-off).
///
/// Use [`BaselineComparison::compute`] to join two [`BenchRun`]s by scenario ID
/// and compute per-scenario deltas and an aggregate mean delta.
///
/// # Examples
///
/// ```
/// use zeph_bench::{BenchRun, RunStatus, ScenarioResult, Aggregate};
/// use zeph_bench::baseline::BaselineComparison;
///
/// fn make_run(run_id: &str, scores: &[(&str, f64)]) -> BenchRun {
///     BenchRun {
///         dataset: "test".into(),
///         model: "model".into(),
///         run_id: run_id.into(),
///         started_at: "2026-01-01T00:00:00Z".into(),
///         finished_at: "2026-01-01T00:01:00Z".into(),
///         status: RunStatus::Completed,
///         results: scores.iter().map(|(id, score)| ScenarioResult {
///             scenario_id: id.to_string(),
///             score: *score,
///             response_excerpt: String::new(),
///             error: None,
///             elapsed_ms: 0,
///         }).collect(),
///         aggregate: Aggregate::default(),
///     }
/// }
///
/// let on = make_run("r1", &[("s1", 1.0), ("s2", 0.5)]);
/// let off = make_run("r2", &[("s1", 0.5), ("s2", 0.0)]);
/// let cmp = BaselineComparison::compute(&on, &off);
/// assert_eq!(cmp.deltas.len(), 2);
/// assert!((cmp.aggregate_delta - 0.5).abs() < f64::EPSILON);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineComparison {
    /// Dataset name (from the memory-on run).
    pub dataset: String,
    /// Model identifier (from the memory-on run).
    pub model: String,
    /// Run ID of the memory-on run.
    pub run_id_memory_on: String,
    /// Run ID of the memory-off run.
    pub run_id_memory_off: String,
    /// Per-scenario deltas, sorted by `scenario_id`.
    ///
    /// Only scenarios present in **both** runs are included (inner join).
    pub deltas: Vec<ScenarioDelta>,
    /// Arithmetic mean of all `delta` values. `0.0` if no scenarios overlap.
    pub aggregate_delta: f64,
}

impl BaselineComparison {
    /// Compute deltas by joining `memory_on` and `memory_off` runs on `scenario_id`.
    ///
    /// Only scenarios present in **both** runs are included. Non-overlapping
    /// scenarios are silently dropped. `aggregate_delta` is the arithmetic mean
    /// of all per-scenario deltas; `0.0` when there are no overlapping scenarios.
    #[must_use]
    pub fn compute(memory_on: &BenchRun, memory_off: &BenchRun) -> Self {
        let off_scores: HashMap<&str, f64> = memory_off
            .results
            .iter()
            .map(|r| (r.scenario_id.as_str(), r.score))
            .collect();

        let mut deltas: Vec<ScenarioDelta> = memory_on
            .results
            .iter()
            .filter_map(|r| {
                let score_off = *off_scores.get(r.scenario_id.as_str())?;
                Some(ScenarioDelta {
                    scenario_id: r.scenario_id.clone(),
                    score_with_memory: r.score,
                    score_without_memory: score_off,
                    delta: r.score - score_off,
                })
            })
            .collect();

        deltas.sort_by(|a, b| a.scenario_id.cmp(&b.scenario_id));

        #[allow(clippy::cast_precision_loss)]
        let aggregate_delta = if deltas.is_empty() {
            0.0
        } else {
            deltas.iter().map(|d| d.delta).sum::<f64>() / deltas.len() as f64
        };

        Self {
            dataset: memory_on.dataset.clone(),
            model: memory_on.model.clone(),
            run_id_memory_on: memory_on.run_id.clone(),
            run_id_memory_off: memory_off.run_id.clone(),
            deltas,
            aggregate_delta,
        }
    }

    /// Write this comparison as pretty-printed JSON to `{output_dir}/comparison.json`.
    ///
    /// The file is written atomically via a `.tmp` sibling + rename, so a concurrent
    /// SIGINT cannot leave a half-written file.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::InvalidFormat`] on serialization failure and
    /// [`BenchError::Io`] on write failure.
    pub fn write_comparison_json(&self, output_dir: &Path) -> Result<(), BenchError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| BenchError::InvalidFormat(e.to_string()))?;
        write_atomic(&output_dir.join("comparison.json"), json.as_bytes())?;
        Ok(())
    }

    /// Append a delta table section to the Markdown file at `summary_path`.
    ///
    /// Creates the file if it does not exist. The section header is
    /// `## Baseline Comparison (Memory On vs Off)` followed by a Markdown table
    /// of per-scenario deltas and a final aggregate delta line.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Io`] on read/write failure.
    pub fn write_delta_table(&self, summary_path: &Path) -> Result<(), BenchError> {
        use std::fs::OpenOptions;
        use std::io::Write as _;

        let mut section = String::new();
        let _ = writeln!(section);
        let _ = writeln!(section, "## Baseline Comparison (Memory On vs Off)");
        let _ = writeln!(section);
        let _ = writeln!(section, "| scenario_id | memory_on | memory_off | delta |");
        let _ = writeln!(section, "|-------------|-----------|------------|-------|");
        for d in &self.deltas {
            let sign = if d.delta >= 0.0 { "+" } else { "" };
            let _ = writeln!(
                section,
                "| {} | {:.4} | {:.4} | {sign}{:.4} |",
                d.scenario_id, d.score_with_memory, d.score_without_memory, d.delta
            );
        }
        let sign = if self.aggregate_delta >= 0.0 { "+" } else { "" };
        let _ = writeln!(
            section,
            "\n**Aggregate delta**: {sign}{:.4} (mean score improvement with memory)",
            self.aggregate_delta
        );

        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(summary_path)?;
        file.write_all(section.as_bytes())?;
        Ok(())
    }
}

/// Write `data` to `path` atomically via a `.tmp` sibling + rename.
fn write_atomic(path: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Aggregate, RunStatus, ScenarioResult};

    fn make_run(run_id: &str, scores: &[(&str, f64)]) -> BenchRun {
        BenchRun {
            dataset: "test-dataset".into(),
            model: "test-model".into(),
            run_id: run_id.into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: "2026-01-01T00:01:00Z".into(),
            status: RunStatus::Completed,
            results: scores
                .iter()
                .map(|(id, score)| ScenarioResult {
                    scenario_id: id.to_string(),
                    score: *score,
                    response_excerpt: String::new(),
                    error: None,
                    elapsed_ms: 0,
                })
                .collect(),
            aggregate: Aggregate::default(),
        }
    }

    #[test]
    fn compute_correct_aggregate_delta() {
        let on = make_run("r1", &[("s1", 1.0), ("s2", 0.5)]);
        let off = make_run("r2", &[("s1", 0.5), ("s2", 0.0)]);
        let cmp = BaselineComparison::compute(&on, &off);
        assert_eq!(cmp.deltas.len(), 2);
        // mean delta = (0.5 + 0.5) / 2 = 0.5
        assert!((cmp.aggregate_delta - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_handles_missing_scenarios_gracefully() {
        // off run has s1 but not s2 — s2 is excluded from deltas
        let on = make_run("r1", &[("s1", 1.0), ("s2", 0.5)]);
        let off = make_run("r2", &[("s1", 0.5)]);
        let cmp = BaselineComparison::compute(&on, &off);
        assert_eq!(cmp.deltas.len(), 1);
        assert_eq!(cmp.deltas[0].scenario_id, "s1");
    }

    #[test]
    fn compute_empty_overlap_returns_zero_aggregate() {
        let on = make_run("r1", &[("s1", 1.0)]);
        let off = make_run("r2", &[("s2", 0.5)]);
        let cmp = BaselineComparison::compute(&on, &off);
        assert!(cmp.deltas.is_empty());
        assert!(cmp.aggregate_delta.abs() < f64::EPSILON);
    }

    #[test]
    fn compute_sorts_deltas_by_scenario_id() {
        let on = make_run("r1", &[("z_last", 1.0), ("a_first", 0.5)]);
        let off = make_run("r2", &[("z_last", 0.5), ("a_first", 0.0)]);
        let cmp = BaselineComparison::compute(&on, &off);
        assert_eq!(cmp.deltas[0].scenario_id, "a_first");
        assert_eq!(cmp.deltas[1].scenario_id, "z_last");
    }

    #[test]
    fn json_round_trip() {
        let on = make_run("r1", &[("s1", 1.0)]);
        let off = make_run("r2", &[("s1", 0.5)]);
        let cmp = BaselineComparison::compute(&on, &off);
        let json = serde_json::to_string_pretty(&cmp).unwrap();
        let decoded: BaselineComparison = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.dataset, cmp.dataset);
        assert_eq!(decoded.deltas.len(), 1);
        assert!((decoded.aggregate_delta - cmp.aggregate_delta).abs() < f64::EPSILON);
    }

    #[test]
    fn write_delta_table_appends_section() {
        let dir = tempfile::tempdir().unwrap();
        let summary = dir.path().join("summary.md");
        std::fs::write(&summary, "# Header\n").unwrap();
        let on = make_run("r1", &[("s1", 1.0)]);
        let off = make_run("r2", &[("s1", 0.5)]);
        let cmp = BaselineComparison::compute(&on, &off);
        cmp.write_delta_table(&summary).unwrap();
        let content = std::fs::read_to_string(&summary).unwrap();
        assert!(content.contains("# Header"));
        assert!(content.contains("## Baseline Comparison"));
        assert!(content.contains("s1"));
    }

    #[test]
    fn write_delta_table_creates_file_if_absent() {
        let dir = tempfile::tempdir().unwrap();
        let summary = dir.path().join("new_summary.md");
        let on = make_run("r1", &[("s1", 1.0)]);
        let off = make_run("r2", &[("s1", 0.5)]);
        let cmp = BaselineComparison::compute(&on, &off);
        cmp.write_delta_table(&summary).unwrap();
        assert!(summary.exists());
        let content = std::fs::read_to_string(&summary).unwrap();
        assert!(content.contains("## Baseline Comparison"));
    }

    #[test]
    fn write_comparison_json_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let on = make_run("r1", &[("s1", 1.0)]);
        let off = make_run("r2", &[("s1", 0.5)]);
        let cmp = BaselineComparison::compute(&on, &off);
        cmp.write_comparison_json(dir.path()).unwrap();
        let json = std::fs::read_to_string(dir.path().join("comparison.json")).unwrap();
        let decoded: BaselineComparison = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.run_id_memory_on, "r1");
        assert_eq!(decoded.run_id_memory_off, "r2");
    }
}
