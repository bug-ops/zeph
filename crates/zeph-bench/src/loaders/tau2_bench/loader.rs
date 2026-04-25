// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dataset loader for tau2-bench retail and airline domains.

use std::fmt::Write as _;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::{
    error::BenchError,
    scenario::{DatasetLoader, Scenario},
};

#[cfg(test)]
use super::data::EvaluationCriteria;
use super::data::{Domain, Task, UserInstructions};

/// Resolved file paths for one tau2-bench domain.
///
/// All three files must reside in the same directory (the layout produced by
/// `bench download --dataset tau2-bench`).
pub struct DataPaths {
    /// JSON array of task objects.
    pub tasks_json: PathBuf,
    /// JSON database seed file for the environment.
    pub db_json: PathBuf,
    /// JSON split definitions (`base`, `train`, `test`).
    pub split_tasks_json: PathBuf,
}

impl DataPaths {
    /// Resolve the three-file set for `domain` under `root`.
    #[must_use]
    pub fn resolve(root: &Path, domain: Domain) -> Self {
        let dir = root.join(domain.as_str());
        Self {
            tasks_json: dir.join("tasks.json"),
            db_json: dir.join("db.json"),
            split_tasks_json: dir.join("split_tasks.json"),
        }
    }
}

/// Loads tau2-bench scenarios for a single domain.
///
/// The loader reads `tasks.json`, parses each [`Task`] into a [`Scenario`], and
/// stores the serialised `EvaluationCriteria` JSON in `scenario.metadata` for the
/// evaluator to retrieve per-scenario at runtime.
///
/// # Path convention
///
/// Pass the absolute path to `tasks.json` as the `path` argument to
/// [`DatasetLoader::load`]. The `db.json` and `split_tasks.json` files are
/// expected to reside in the same directory.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use zeph_bench::loaders::tau2_bench::loader::Tau2BenchLoader;
/// use zeph_bench::scenario::DatasetLoader;
///
/// let loader = Tau2BenchLoader::retail();
/// let scenarios = loader.load(Path::new("/data/tau2-bench/retail/tasks.json")).unwrap();
/// println!("loaded {} scenarios", scenarios.len());
/// ```
pub struct Tau2BenchLoader {
    /// Domain this loader targets.
    pub domain: Domain,
}

impl Tau2BenchLoader {
    /// Create a loader for the retail domain.
    #[must_use]
    pub fn retail() -> Self {
        Self {
            domain: Domain::Retail,
        }
    }

    /// Create a loader for the airline domain.
    #[must_use]
    pub fn airline() -> Self {
        Self {
            domain: Domain::Airline,
        }
    }
}

impl DatasetLoader for Tau2BenchLoader {
    fn name(&self) -> &'static str {
        match self.domain {
            Domain::Retail => "tau2-bench-retail",
            Domain::Airline => "tau2-bench-airline",
        }
    }

    /// Load all tasks from `path` (must be `tasks.json`).
    ///
    /// All tasks are loaded regardless of `reward_basis` — the evaluator scores
    /// based on the `actions` field which is present in every task.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::InvalidFormat`] when the file cannot be opened or
    /// when JSON parsing fails.
    fn load(&self, path: &Path) -> Result<Vec<Scenario>, BenchError> {
        let file = std::fs::File::open(path)
            .map_err(|e| BenchError::InvalidFormat(format!("open tasks.json: {e}")))?;
        let tasks: Vec<Task> = serde_json::from_reader(BufReader::new(file))
            .map_err(|e| BenchError::InvalidFormat(format!("parse tasks.json: {e}")))?;

        let loader_name = self.name();
        let mut scenarios = Vec::with_capacity(tasks.len());

        for task in tasks {
            let id = format!("{}_{}", loader_name, task.id);
            let prompt = build_prompt(&task);
            let metadata = serde_json::json!({
                "domain": loader_name,
                "tau2_task_id": task.id,
                "evaluation_criteria": task.evaluation_criteria,
            });
            scenarios.push(Scenario::single(id, prompt, "", metadata));
        }

        Ok(scenarios)
    }
}

/// Convert a [`Task`]'s user scenario into a single instruction string.
///
/// This is a deliberate MVP simplification — the full tau2-bench benchmark uses
/// a multi-turn user simulator. Here we collapse it into one upfront prompt.
///
/// # TODO
///
/// TODO(#3417/D4): implement multi-turn user simulator. The current approach
/// works for ACTION-only scoring because the agent sees all information at once,
/// but will under-score tasks where the user simulator drives information
/// exchange across turns.
fn build_prompt(task: &Task) -> String {
    match &task.user_scenario.instructions {
        UserInstructions::Plain(s) => s.clone(),
        UserInstructions::Structured(i) => {
            let mut buf = String::new();
            writeln!(buf, "You are speaking to a customer support agent.").ok();
            writeln!(buf, "\nReason for call:\n{}", i.reason_for_call).ok();
            if let Some(known) = &i.known_info {
                writeln!(buf, "\nKnown information about you:\n{known}").ok();
            }
            writeln!(buf, "\nTask instructions:\n{}", i.task_instructions).ok();
            buf
        }
    }
}

/// Return the `db.json` path that accompanies the given `tasks.json` path.
///
/// # Errors
///
/// Returns [`BenchError::InvalidFormat`] if `tasks_json` has no parent directory.
pub fn db_json_path(tasks_json: &Path) -> Result<PathBuf, BenchError> {
    tasks_json
        .parent()
        .map(|dir| dir.join("db.json"))
        .ok_or_else(|| {
            BenchError::InvalidFormat(
                "tasks.json must have a parent directory containing db.json".into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASKS_FIXTURE: &str = r##"[
      {
        "id": "0",
        "user_scenario": {
          "instructions": {
            "domain": "retail",
            "reason_for_call": "Cancel my order",
            "task_instructions": "Cancel order #W0001",
            "known_info": "Order id: #W0001"
          }
        },
        "evaluation_criteria": {
          "actions": [
            {
              "action_id": "a1",
              "requestor": "assistant",
              "name": "cancel_pending_order",
              "arguments": {"order_id": "#W0001", "reason": "no_longer_needed"},
              "compare_args": ["order_id", "reason"]
            }
          ],
          "reward_basis": ["ACTION"]
        }
      },
      {
        "id": "1",
        "user_scenario": {
          "instructions": "Simple plain prompt"
        },
        "evaluation_criteria": {
          "actions": [],
          "reward_basis": ["ACTION"]
        }
      },
      {
        "id": "2",
        "user_scenario": {
          "instructions": "DB-only task"
        },
        "evaluation_criteria": {
          "actions": [],
          "reward_basis": ["DB"]
        }
      }
    ]"##;

    fn load_from_str(json: &str, domain: Domain) -> Vec<Scenario> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        std::fs::write(&path, json).unwrap();
        let loader = Tau2BenchLoader { domain };
        loader.load(&path).unwrap()
    }

    #[test]
    fn load_all_tasks_regardless_of_reward_basis() {
        // All 3 tasks are loaded — reward_basis filter was removed.
        let scenarios = load_from_str(TASKS_FIXTURE, Domain::Retail);
        assert_eq!(scenarios.len(), 3);
    }

    #[test]
    fn load_builds_correct_ids() {
        let scenarios = load_from_str(TASKS_FIXTURE, Domain::Retail);
        assert_eq!(scenarios[0].id, "tau2-bench-retail_0");
        assert_eq!(scenarios[1].id, "tau2-bench-retail_1");
        assert_eq!(scenarios[2].id, "tau2-bench-retail_2");
    }

    #[test]
    fn load_prompt_from_structured_instructions() {
        let scenarios = load_from_str(TASKS_FIXTURE, Domain::Retail);
        let prompt = scenarios[0].primary_prompt().unwrap();
        assert!(prompt.contains("Cancel my order") || prompt.contains("Cancel order"));
    }

    #[test]
    fn load_prompt_from_plain_instructions() {
        let scenarios = load_from_str(TASKS_FIXTURE, Domain::Retail);
        let prompt = scenarios[1].primary_prompt().unwrap();
        assert_eq!(prompt, "Simple plain prompt");
    }

    #[test]
    fn metadata_contains_evaluation_criteria() {
        let scenarios = load_from_str(TASKS_FIXTURE, Domain::Retail);
        let criteria_value = scenarios[0].metadata.get("evaluation_criteria").unwrap();
        let criteria: EvaluationCriteria = serde_json::from_value(criteria_value.clone()).unwrap();
        assert_eq!(criteria.actions.len(), 1);
        assert_eq!(criteria.actions[0].name, "cancel_pending_order");
    }

    #[test]
    fn metadata_roundtrip_preserves_arguments() {
        let scenarios = load_from_str(TASKS_FIXTURE, Domain::Retail);
        let criteria_value = scenarios[0].metadata.get("evaluation_criteria").unwrap();
        let criteria: EvaluationCriteria = serde_json::from_value(criteria_value.clone()).unwrap();
        let arg = criteria.actions[0].arguments.get("order_id").unwrap();
        assert_eq!(arg.as_str(), Some("#W0001"));
    }

    #[test]
    fn airline_loader_name() {
        let loader = Tau2BenchLoader::airline();
        assert_eq!(loader.name(), "tau2-bench-airline");
    }
}
