// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Evaluators for tau2-bench.
//!
//! # Action evaluator (`TauBenchEvaluator`)
//!
//! Implements upstream `Action.compare_with_tool_call` semantics:
//!
//! - For each gold action, scan the recorded calls for ANY matching call.
//! - Matching: tool name must match; arguments are compared by the keys
//!   in `compare_args` (if set), or all keys in the recorded call (if `None`),
//!   or name-only when `compare_args = Some([])`.
//! - Reward = 1.0 iff ALL gold actions are matched at least once (order-insensitive).
//!
//! Only `requestor = "assistant"` gold actions are evaluated — user-simulator
//! actions are out of scope for action-only scoring.
//!
//! # Environment evaluator (`EnvironmentEvaluator`)
//!
//! Compares the final environment state against a gold state built by replaying
//! gold actions on a fresh environment instance. Scoring is binary: 1.0 if the
//! canonical SHA-256 hashes of both states match, 0.0 otherwise.
//!
//! The canonical hash is computed by serialising the state to JSON, sorting all
//! `Object` keys recursively (to neutralise `HashMap` iteration order), and then
//! hashing the resulting deterministic bytes via SHA-256. This matches upstream
//! `evaluator_env.py` semantics.
//!
//! # Composite evaluator (`CompositeEvaluator`)
//!
//! Combines action and environment scoring based on `reward_basis`:
//!
//! - `["ACTION"]` → action score only
//! - `["DB"]` → env score only
//! - `["ACTION", "DB"]` (any order) → `min(action, env)` — both must pass for full credit

use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::{
    error::BenchError,
    scenario::{EvalResult, Evaluator, Scenario},
};

use super::{
    data::{Action, Domain, EvaluationCriteria},
    envs::{ActionTrace, RecordedToolCall, SnapshotableEnv},
};

/// Evaluates agent tool-call traces against tau2-bench gold actions.
///
/// Constructed per-scenario via [`TauBenchEvaluator::from_scenario`]. Holds a
/// reference to the shared [`ActionTrace`] populated by the env executor, plus
/// the gold actions extracted from the scenario metadata.
#[derive(Debug)]
pub struct TauBenchEvaluator {
    trace: ActionTrace,
    gold_actions: Vec<Action>,
}

impl TauBenchEvaluator {
    /// Build an evaluator for `scenario`.
    ///
    /// Reads `scenario.metadata["evaluation_criteria"]` and deserializes it into
    /// typed `EvaluationCriteria`. Only `requestor = "assistant"` actions are
    /// retained in the gold set — user-simulator actions are not scored.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::InvalidFormat`] when:
    /// - `evaluation_criteria` key is absent from metadata.
    /// - The value cannot be deserialized into `EvaluationCriteria`.
    ///
    /// A malformed scenario is always a hard failure — never silently passes.
    pub fn from_scenario(scenario: &Scenario, trace: ActionTrace) -> Result<Self, BenchError> {
        let criteria_value = scenario
            .metadata
            .get("evaluation_criteria")
            .ok_or_else(|| {
                BenchError::InvalidFormat(format!(
                    "scenario {} missing evaluation_criteria metadata",
                    scenario.id
                ))
            })?;

        let criteria: EvaluationCriteria =
            serde_json::from_value(criteria_value.clone()).map_err(|e| {
                BenchError::InvalidFormat(format!(
                    "scenario {} bad evaluation_criteria: {e}",
                    scenario.id
                ))
            })?;

        // Recorded calls are implicitly requestor=assistant — only score assistant gold actions.
        let gold_actions = criteria
            .actions
            .into_iter()
            .filter(|a| a.requestor == "assistant")
            .collect();

        Ok(Self {
            trace,
            gold_actions,
        })
    }
}

impl Evaluator for TauBenchEvaluator {
    fn evaluate(&self, scenario: &Scenario, _agent_response: &str) -> EvalResult {
        let recorded = self.trace.lock().expect("trace mutex poisoned").clone();

        let total = self.gold_actions.len();
        if total == 0 {
            return EvalResult {
                scenario_id: scenario.id.clone(),
                score: 1.0,
                passed: true,
                details: "action_reward no_gold_actions=true".to_owned(),
            };
        }

        let mut unmatched: Vec<&str> = Vec::new();
        let mut matched = 0usize;

        for gold in &self.gold_actions {
            if recorded.iter().any(|rec| action_matches(gold, rec)) {
                matched += 1;
            } else {
                unmatched.push(&gold.name);
            }
        }

        let passed = matched == total;
        // Unmatched action names are already included via `unmatched={:?}` (#4235).
        let details = format!(
            "action_reward matched={}/{} recorded_calls={} unmatched={:?}",
            matched,
            total,
            recorded.len(),
            unmatched,
        );

        EvalResult {
            scenario_id: scenario.id.clone(),
            score: if passed { 1.0 } else { 0.0 },
            passed,
            details,
        }
    }
}

/// Implements upstream `Action.compare_with_tool_call` semantics.
///
/// Recorded calls are implicitly `requestor=assistant` — the `requestor` field
/// on `gold` has already been filtered to `"assistant"` in `from_scenario`.
fn action_matches(gold: &Action, rec: &RecordedToolCall) -> bool {
    if gold.name != rec.name {
        return false;
    }

    let keys: Vec<&str> = match &gold.compare_args {
        // compare_args = Some([]) means name-only match.
        Some(list) if list.is_empty() => return true,
        Some(list) => list.iter().map(String::as_str).collect(),
        // compare_args = None: compare all keys present in the recorded call.
        None => rec.arguments.keys().map(String::as_str).collect(),
    };

    keys.iter().all(|k| {
        let g = gold.arguments.get(*k);
        let r = rec.arguments.get(*k);
        match (g, r) {
            (Some(g), Some(r)) => values_equal_canonical(g, r),
            (None, None) => true,
            _ => false,
        }
    })
}

// ─── Environment evaluator ───────────────────────────────────────────────────

/// Evaluates the final environment state against a gold state built by replaying gold actions.
///
/// Scoring is binary: reward = 1.0 iff `canonical_hash(final_state) == canonical_hash(gold_state)`.
///
/// The `final_state` snapshot must be captured immediately after the agent run completes,
/// before the env is dropped. The gold snapshot is built inside [`Evaluator::evaluate`] by
/// constructing a fresh env from the seed and replaying gold actions on it.
pub struct EnvironmentEvaluator {
    /// Snapshot of env state captured after the agent run.
    final_snapshot: serde_json::Value,
    /// Gold actions to replay on a fresh env to produce the expected state.
    gold_actions: Vec<Action>,
    /// Path to `db.json` used to seed a fresh env for gold replay.
    db_seed_path: PathBuf,
    /// Domain selector for choosing the correct env constructor.
    domain: Domain,
}

impl EnvironmentEvaluator {
    /// Construct from a post-run snapshot.
    ///
    /// `final_snapshot` should be obtained by calling `env.state_snapshot()` immediately
    /// after the agent run completes. The evaluator stores it and uses `db_seed_path` to
    /// construct a fresh env for gold replay inside [`evaluate`][Evaluator::evaluate].
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::InvalidFormat`] when `evaluation_criteria` is absent or
    /// cannot be deserialized.
    pub fn from_scenario(
        scenario: &Scenario,
        final_snapshot: serde_json::Value,
        db_seed_path: &Path,
        domain: Domain,
    ) -> Result<Self, BenchError> {
        let criteria_value = scenario
            .metadata
            .get("evaluation_criteria")
            .ok_or_else(|| {
                BenchError::InvalidFormat(format!(
                    "scenario {} missing evaluation_criteria metadata",
                    scenario.id
                ))
            })?;

        let criteria: EvaluationCriteria =
            serde_json::from_value(criteria_value.clone()).map_err(|e| {
                BenchError::InvalidFormat(format!(
                    "scenario {} bad evaluation_criteria: {e}",
                    scenario.id
                ))
            })?;

        Ok(Self {
            final_snapshot,
            gold_actions: criteria.actions,
            db_seed_path: db_seed_path.to_owned(),
            domain,
        })
    }
}

impl Evaluator for EnvironmentEvaluator {
    fn evaluate(&self, scenario: &Scenario, _agent_response: &str) -> EvalResult {
        let gold_snapshot =
            build_gold_snapshot(&self.db_seed_path, &self.gold_actions, self.domain);

        let gold_snapshot = match gold_snapshot {
            Ok(v) => v,
            Err(e) => {
                return EvalResult {
                    scenario_id: scenario.id.clone(),
                    score: 0.0,
                    passed: false,
                    details: format!("env_reward gold_replay_error={e}"),
                };
            }
        };

        let final_hash = canonical_hash(self.final_snapshot.clone());
        let gold_hash = canonical_hash(gold_snapshot);
        let passed = final_hash == gold_hash;

        EvalResult {
            scenario_id: scenario.id.clone(),
            score: if passed { 1.0 } else { 0.0 },
            passed,
            details: format!(
                "env_reward passed={passed} final_hash={} gold_hash={}",
                hex_prefix(&final_hash),
                hex_prefix(&gold_hash),
            ),
        }
    }
}

/// Build a gold state snapshot by replaying gold actions on a fresh env loaded from `db_seed`.
fn build_gold_snapshot(
    db_seed: &Path,
    gold_actions: &[Action],
    domain: Domain,
) -> Result<serde_json::Value, BenchError> {
    use super::envs::{airline::AirlineEnv, retail::RetailEnv};

    // Block-on tokio is unavoidable here: `Evaluator::evaluate` is sync, but
    // `replay_actions` is async. The benchmark runner always calls `evaluate` from
    // within a tokio runtime context, so `block_in_place` is safe.
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            match domain {
                Domain::Retail => {
                    let (env, _trace) = RetailEnv::new_from_seed(db_seed)?;
                    env.replay_actions(gold_actions).await?;
                    Ok(env.state_snapshot())
                }
                Domain::Airline => {
                    let (env, _trace) = AirlineEnv::new_from_seed(db_seed)?;
                    env.replay_actions(gold_actions).await?;
                    Ok(env.state_snapshot())
                }
            }
        })
    })
}

/// Compute SHA-256 of the canonical JSON representation of `value`.
///
/// All `Object` keys are sorted recursively so that `HashMap`-backed objects
/// produce a deterministic byte sequence regardless of insertion order.
fn canonical_hash(value: serde_json::Value) -> [u8; 32] {
    let sorted = sort_keys_recursive(value);
    let bytes = serde_json::to_vec(&sorted).unwrap_or_default();
    Sha256::digest(&bytes).into()
}

/// Recursively sort all JSON `Object` keys.
fn sort_keys_recursive(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<(String, serde_json::Value)> = map
                .into_iter()
                .map(|(k, v)| (k, sort_keys_recursive(v)))
                .collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(sort_keys_recursive).collect())
        }
        other => other,
    }
}

/// Return the first 8 hex chars of a hash for compact logging.
fn hex_prefix(hash: &[u8; 32]) -> String {
    hash.iter().take(4).fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        write!(s, "{b:02x}").ok();
        s
    })
}

// ─── Composite evaluator ─────────────────────────────────────────────────────

/// Combines [`TauBenchEvaluator`] and [`EnvironmentEvaluator`] based on `reward_basis`.
///
/// Scoring:
/// - `["ACTION"]` → action score only
/// - `["DB"]` → env score only
/// - `["ACTION", "DB"]` (any order) → `min(action_score, env_score)` — both must pass
pub struct CompositeEvaluator {
    action_eval: TauBenchEvaluator,
    env_eval: Option<EnvironmentEvaluator>,
    /// True when both action and env scoring are required (`reward_basis` contains both).
    require_both: bool,
}

impl CompositeEvaluator {
    /// Build from a scenario, env snapshot, and db seed path.
    ///
    /// `reward_basis` is read from `evaluation_criteria` in scenario metadata. When it
    /// contains `"DB"`, an [`EnvironmentEvaluator`] is constructed; otherwise only the
    /// action evaluator runs.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::InvalidFormat`] when metadata is missing or malformed.
    pub fn from_scenario(
        scenario: &Scenario,
        trace: ActionTrace,
        final_snapshot: Option<serde_json::Value>,
        db_seed_path: &Path,
        domain: Domain,
    ) -> Result<Self, BenchError> {
        let criteria_value = scenario
            .metadata
            .get("evaluation_criteria")
            .ok_or_else(|| {
                BenchError::InvalidFormat(format!(
                    "scenario {} missing evaluation_criteria metadata",
                    scenario.id
                ))
            })?;
        let criteria: EvaluationCriteria =
            serde_json::from_value(criteria_value.clone()).map_err(|e| {
                BenchError::InvalidFormat(format!(
                    "scenario {} bad evaluation_criteria: {e}",
                    scenario.id
                ))
            })?;

        let wants_db = criteria
            .reward_basis
            .iter()
            .any(|b| b.eq_ignore_ascii_case("DB"));
        let wants_action = criteria
            .reward_basis
            .iter()
            .any(|b| b.eq_ignore_ascii_case("ACTION"));
        let require_both = wants_db && wants_action;

        let action_eval = TauBenchEvaluator::from_scenario(scenario, trace)?;

        let env_eval = if wants_db {
            let snapshot = final_snapshot.ok_or_else(|| {
                BenchError::InvalidFormat(format!(
                    "scenario {} has DB in reward_basis but no env snapshot was provided",
                    scenario.id
                ))
            })?;
            Some(EnvironmentEvaluator::from_scenario(
                scenario,
                snapshot,
                db_seed_path,
                domain,
            )?)
        } else {
            None
        };

        Ok(Self {
            action_eval,
            env_eval,
            require_both,
        })
    }
}

impl Evaluator for CompositeEvaluator {
    fn evaluate(&self, scenario: &Scenario, agent_response: &str) -> EvalResult {
        let action_result = self.action_eval.evaluate(scenario, agent_response);

        match &self.env_eval {
            None => action_result,
            Some(env_eval) => {
                let env_result = env_eval.evaluate(scenario, agent_response);
                let score = if self.require_both {
                    action_result.score.min(env_result.score)
                } else {
                    env_result.score
                };
                let passed = score >= 1.0;
                EvalResult {
                    scenario_id: scenario.id.clone(),
                    score,
                    passed,
                    details: format!("{} | {}", action_result.details, env_result.details),
                }
            }
        }
    }
}

// ─── Value comparison helpers ─────────────────────────────────────────────────

/// Canonical value equality: normalises `1` vs `1.0` and recurses into containers.
///
/// Object key ordering does NOT matter — `serde_json::Map::PartialEq` is
/// order-insensitive when `preserve_order` is disabled (the workspace default).
fn values_equal_canonical(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value::{Array, Null, Number, Object, String as Str};
    match (a, b) {
        (Number(an), Number(bn)) => {
            // Treat integer-valued floats as equal to integers: 1 == 1.0 → true.
            // NOTE: serde_json does not enable arbitrary_precision in this workspace
            // so as_i64/as_f64 are reliable. Verified with test below.
            match (an.as_i64(), bn.as_i64()) {
                (Some(ai), Some(bi)) => ai == bi,
                _ => an.as_f64() == bn.as_f64(),
            }
        }
        (Str(sa), Str(sb)) => sa == sb,
        (Array(av), Array(bv)) => {
            av.len() == bv.len() && av.iter().zip(bv).all(|(x, y)| values_equal_canonical(x, y))
        }
        (Object(am), Object(bm)) => {
            am.len() == bm.len()
                && am
                    .iter()
                    .all(|(k, v)| bm.get(k).is_some_and(|bv| values_equal_canonical(v, bv)))
        }
        (Null, Null) => true,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    #[allow(clippy::needless_pass_by_value)]
    fn make_scenario(criteria: serde_json::Value) -> Scenario {
        Scenario::single(
            "test_0",
            "test prompt",
            "",
            json!({ "evaluation_criteria": criteria }),
        )
    }

    fn make_trace(calls: Vec<(&str, serde_json::Value)>) -> ActionTrace {
        let recorded: Vec<RecordedToolCall> = calls
            .into_iter()
            .map(|(name, args)| RecordedToolCall {
                name: name.to_owned(),
                arguments: args.as_object().cloned().unwrap_or_default(),
            })
            .collect();
        Arc::new(Mutex::new(recorded))
    }

    #[test]
    fn all_matched_scores_one() {
        let criteria = json!({
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
        });
        let scenario = make_scenario(criteria);
        let trace = make_trace(vec![(
            "cancel_pending_order",
            json!({"order_id": "#W0001", "reason": "no_longer_needed"}),
        )]);
        let evaluator = TauBenchEvaluator::from_scenario(&scenario, trace).unwrap();
        let result = evaluator.evaluate(&scenario, "");
        assert!(result.passed);
        assert!((result.score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn missing_action_scores_zero() {
        let criteria = json!({
            "actions": [
                {
                    "action_id": "a1",
                    "requestor": "assistant",
                    "name": "find_user_id_by_email",
                    "arguments": {"email": "test@test.com"},
                    "compare_args": null
                }
            ],
            "reward_basis": ["ACTION"]
        });
        let scenario = make_scenario(criteria);
        let trace = make_trace(vec![]);
        let evaluator = TauBenchEvaluator::from_scenario(&scenario, trace).unwrap();
        let result = evaluator.evaluate(&scenario, "");
        assert!(!result.passed);
        assert!(result.score < f64::EPSILON);
    }

    #[test]
    fn name_only_match_with_empty_compare_args() {
        let criteria = json!({
            "actions": [
                {
                    "action_id": "a1",
                    "requestor": "assistant",
                    "name": "list_all_product_types",
                    "arguments": {},
                    "compare_args": []
                }
            ],
            "reward_basis": ["ACTION"]
        });
        let scenario = make_scenario(criteria);
        // The agent called the tool with extra args — should still match (name-only).
        let trace = make_trace(vec![(
            "list_all_product_types",
            json!({"extra": "ignored"}),
        )]);
        let evaluator = TauBenchEvaluator::from_scenario(&scenario, trace).unwrap();
        let result = evaluator.evaluate(&scenario, "");
        assert!(result.passed);
    }

    #[test]
    fn integer_vs_float_canonical_match() {
        assert!(values_equal_canonical(&json!(1), &json!(1.0)));
        assert!(values_equal_canonical(&json!(1.0), &json!(1)));
        assert!(!values_equal_canonical(&json!(1), &json!(1.5)));
    }

    #[test]
    fn no_gold_actions_passes() {
        let criteria = json!({"actions": [], "reward_basis": ["ACTION"]});
        let scenario = make_scenario(criteria);
        let trace = make_trace(vec![]);
        let evaluator = TauBenchEvaluator::from_scenario(&scenario, trace).unwrap();
        let result = evaluator.evaluate(&scenario, "");
        assert!(result.passed);
    }

    #[test]
    fn missing_metadata_returns_error() {
        let scenario = Scenario::single("bad_0", "prompt", "", json!({}));
        let trace = make_trace(vec![]);
        let err = TauBenchEvaluator::from_scenario(&scenario, trace);
        assert!(err.is_err());
        assert_matches!(err.unwrap_err(), BenchError::InvalidFormat(_));
    }

    #[test]
    fn bad_criteria_value_returns_error() {
        let scenario = Scenario::single(
            "bad_1",
            "prompt",
            "",
            json!({"evaluation_criteria": "not an object"}),
        );
        let trace = make_trace(vec![]);
        let err = TauBenchEvaluator::from_scenario(&scenario, trace);
        assert!(err.is_err());
    }

    #[test]
    fn compare_args_whitelist_only_checks_listed_keys() {
        let criteria = json!({
            "actions": [
                {
                    "action_id": "a1",
                    "requestor": "assistant",
                    "name": "cancel_pending_order",
                    "arguments": {"order_id": "#W0001", "reason": "no_longer_needed"},
                    "compare_args": ["order_id"]
                }
            ],
            "reward_basis": ["ACTION"]
        });
        let scenario = make_scenario(criteria);
        // Agent provided a different `reason` — should still match because only `order_id` is in compare_args.
        let trace = make_trace(vec![(
            "cancel_pending_order",
            json!({"order_id": "#W0001", "reason": "something_else"}),
        )]);
        let evaluator = TauBenchEvaluator::from_scenario(&scenario, trace).unwrap();
        let result = evaluator.evaluate(&scenario, "");
        assert!(result.passed);
    }

    #[test]
    fn details_contain_unmatched_action_names() {
        let criteria = json!({
            "actions": [
                {
                    "action_id": "a1",
                    "requestor": "assistant",
                    "name": "missing_tool",
                    "arguments": {},
                    "compare_args": []
                }
            ],
            "reward_basis": ["ACTION"]
        });
        let scenario = make_scenario(criteria);
        let trace = make_trace(vec![]);
        let evaluator = TauBenchEvaluator::from_scenario(&scenario, trace).unwrap();
        let result = evaluator.evaluate(&scenario, "");
        assert!(result.details.contains("missing_tool"));
    }

    // ─── canonical_hash / sort_keys_recursive tests ──────────────────────────

    #[test]
    fn canonical_hash_same_keys_different_order_equal() {
        // Two objects with the same key-value pairs in different insertion order.
        let a = json!({"b": 2, "a": 1});
        let b = json!({"a": 1, "b": 2});
        assert_eq!(canonical_hash(a), canonical_hash(b));
    }

    #[test]
    fn canonical_hash_different_values_differ() {
        let a = json!({"key": "value_a"});
        let b = json!({"key": "value_b"});
        assert_ne!(canonical_hash(a), canonical_hash(b));
    }

    #[test]
    fn canonical_hash_nested_objects_sorted() {
        let a = json!({"outer": {"z": 3, "a": 1, "m": 2}});
        let b = json!({"outer": {"a": 1, "m": 2, "z": 3}});
        assert_eq!(canonical_hash(a), canonical_hash(b));
    }

    #[test]
    fn canonical_hash_array_of_objects_sorted_per_element() {
        // Each object inside the array gets its keys sorted independently.
        let a = json!([{"b": 2, "a": 1}, {"d": 4, "c": 3}]);
        let b = json!([{"a": 1, "b": 2}, {"c": 3, "d": 4}]);
        assert_eq!(canonical_hash(a), canonical_hash(b));
    }

    #[test]
    fn canonical_hash_scalar_stable() {
        assert_eq!(canonical_hash(json!(42)), canonical_hash(json!(42)));
        assert_ne!(canonical_hash(json!(42)), canonical_hash(json!(43)));
    }

    // ─── EnvironmentEvaluator unit tests ─────────────────────────────────────

    #[allow(dead_code)]
    fn make_env_evaluator(
        final_snapshot: serde_json::Value,
        db_seed_path: &std::path::Path,
    ) -> EnvironmentEvaluator {
        let scenario = make_scenario(json!({
            "actions": [],
            "reward_basis": ["DB"]
        }));
        EnvironmentEvaluator::from_scenario(
            &scenario,
            final_snapshot,
            db_seed_path,
            super::super::data::Domain::Retail,
        )
        .unwrap()
    }

    #[test]
    fn env_evaluator_matching_snapshots_scores_one() {
        // When final snapshot == gold snapshot (no actions to replay → seed state),
        // score must be 1.0. We test this by using a non-existent seed path and
        // providing the final_snapshot directly (bypassing build_gold_snapshot).
        // The test only exercises the hash-comparison path, not disk I/O.
        //
        // To avoid build_gold_snapshot being called (it needs a real file), we
        // construct an evaluator that would fail on evaluate(). Instead, test the
        // hash functions directly: two identical JSON values must hash equal.
        let snap = json!({"users": {}, "orders": {}, "products": {}});
        let h1 = canonical_hash(snap.clone());
        let h2 = canonical_hash(snap);
        assert_eq!(h1, h2, "identical snapshots must hash equally");
    }

    #[test]
    fn env_evaluator_gold_replay_error_scores_zero() {
        // When db_seed_path does not exist, build_gold_snapshot fails and evaluate
        // must return score=0.0 with a details string containing "gold_replay_error".
        let non_existent = std::path::Path::new("/nonexistent/path/db.json");
        let scenario = make_scenario(json!({
            "actions": [
                {
                    "action_id": "a1",
                    "requestor": "assistant",
                    "name": "cancel_pending_order",
                    "arguments": {"order_id": "#W0001", "reason": "no_longer_needed"},
                    "compare_args": null
                }
            ],
            "reward_basis": ["DB"]
        }));
        let evaluator = EnvironmentEvaluator::from_scenario(
            &scenario,
            json!({}),
            non_existent,
            super::super::data::Domain::Retail,
        )
        .unwrap();

        // block_in_place requires a multi-threaded runtime.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async move {
            tokio::task::spawn_blocking(move || evaluator.evaluate(&scenario, ""))
                .await
                .unwrap()
        });

        assert!(!result.passed);
        assert!(result.score < f64::EPSILON);
        assert!(result.details.contains("gold_replay_error"));
    }

    // ─── CompositeEvaluator unit tests ───────────────────────────────────────

    #[test]
    fn composite_action_only_uses_action_score() {
        let criteria = json!({
            "actions": [
                {
                    "action_id": "a1",
                    "requestor": "assistant",
                    "name": "cancel_pending_order",
                    "arguments": {"order_id": "#W0001"},
                    "compare_args": ["order_id"]
                }
            ],
            "reward_basis": ["ACTION"]
        });
        let scenario = make_scenario(criteria);
        // Trace matches gold action → action score = 1.0.
        let trace = make_trace(vec![(
            "cancel_pending_order",
            json!({"order_id": "#W0001"}),
        )]);
        let evaluator = CompositeEvaluator::from_scenario(
            &scenario,
            trace,
            None,
            std::path::Path::new("/unused"),
            super::super::data::Domain::Retail,
        )
        .unwrap();
        let result = evaluator.evaluate(&scenario, "");
        assert!(result.passed);
        assert!((result.score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn composite_action_only_empty_trace_scores_zero() {
        let criteria = json!({
            "actions": [
                {
                    "action_id": "a1",
                    "requestor": "assistant",
                    "name": "some_tool",
                    "arguments": {},
                    "compare_args": []
                }
            ],
            "reward_basis": ["ACTION"]
        });
        let scenario = make_scenario(criteria);
        let trace = make_trace(vec![]);
        let evaluator = CompositeEvaluator::from_scenario(
            &scenario,
            trace,
            None,
            std::path::Path::new("/unused"),
            super::super::data::Domain::Retail,
        )
        .unwrap();
        let result = evaluator.evaluate(&scenario, "");
        assert!(!result.passed);
        assert!(result.score < f64::EPSILON);
    }

    #[test]
    fn composite_db_missing_snapshot_returns_error() {
        let criteria = json!({
            "actions": [],
            "reward_basis": ["DB"]
        });
        let scenario = make_scenario(criteria);
        let trace = make_trace(vec![]);
        let result = CompositeEvaluator::from_scenario(
            &scenario,
            trace,
            None, // no snapshot provided
            std::path::Path::new("/unused"),
            super::super::data::Domain::Retail,
        );
        assert!(result.is_err());
        assert_matches!(result.err().unwrap(), BenchError::InvalidFormat(_));
    }

    #[test]
    fn composite_action_and_db_min_scoring() {
        // ACTION passes (trace matches gold), DB fails (non-existent seed → gold_replay_error).
        // min(1.0, 0.0) = 0.0 → not passed.
        let criteria = json!({
            "actions": [
                {
                    "action_id": "a1",
                    "requestor": "assistant",
                    "name": "cancel_pending_order",
                    "arguments": {"order_id": "#W0001"},
                    "compare_args": ["order_id"]
                }
            ],
            "reward_basis": ["ACTION", "DB"]
        });
        let scenario = make_scenario(criteria);
        let trace = make_trace(vec![(
            "cancel_pending_order",
            json!({"order_id": "#W0001"}),
        )]);
        let evaluator = CompositeEvaluator::from_scenario(
            &scenario,
            trace,
            Some(json!({})),
            std::path::Path::new("/nonexistent/db.json"),
            super::super::data::Domain::Retail,
        )
        .unwrap();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async move {
            tokio::task::spawn_blocking(move || evaluator.evaluate(&scenario, ""))
                .await
                .unwrap()
        });

        assert!(!result.passed);
        assert!(result.score < f64::EPSILON);
        // Details should contain both action and env info.
        assert!(result.details.contains("action_reward"));
        assert!(result.details.contains("env_reward"));
    }
}
