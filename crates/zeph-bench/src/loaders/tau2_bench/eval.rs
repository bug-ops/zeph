// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Action-trace evaluator for tau2-bench.
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
//! actions are out of scope for the single-turn MVP.
//!
//! # TODO
//!
//! TODO(#3417/D1): Phase 2 — implement upstream `EnvironmentEvaluator` (DB hash + `env_assertions`).
//! Current scoring is ACTION-only and matches upstream `evaluator_action.py`. To reproduce
//! published numbers using `evaluation_type=ALL`, we additionally need:
//!
//!   1. Snapshot `RetailState`/`AirlineState` after the run (via `Arc::clone` of the env state).
//!   2. Reapply gold actions to a fresh state to build the "gold DB".
//!   3. Compare via stable hash (canonical JSON serialization sorted by keys).
//!
//! See `architect-tau-bench-v2.md` §6 and upstream `evaluator_env.py`.

use crate::{
    error::BenchError,
    scenario::{EvalResult, Evaluator, Scenario},
};

use super::{
    data::{Action, EvaluationCriteria},
    envs::{ActionTrace, RecordedToolCall},
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
        // TODO(critic): append unmatched gold-action names to details for easier debugging.
        // See critic-tau-bench-v2.md M4.
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
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

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
        assert!(matches!(err.unwrap_err(), BenchError::InvalidFormat(_)));
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
}
