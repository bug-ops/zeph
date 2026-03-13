// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dependency graph for intra-turn tool call ordering.
//!
//! When an LLM returns multiple `tool_use` blocks where one call's arguments reference
//! another call's `tool_use_id`, those calls must execute in topological order. This
//! module builds a lightweight DAG from the tool call batch and partitions it into
//! independent parallel tiers using Kahn's algorithm.
//!
//! # Cycle handling
//!
//! Cyclic `tool_use_id` references are malformed LLM output. When a cycle is detected,
//! [`ToolCallDag::tiers`] returns a single tier containing all call indices in original
//! order — the caller should execute them sequentially. Independent (non-cyclic) calls
//! are included in that single fallback tier as well, since mixing parallel and sequential
//! execution within a partial cycle adds complexity for no practical benefit.

use serde_json::Value;
use zeph_llm::provider::ToolUseRequest;

/// A partition of tool call indices that can execute concurrently.
#[derive(Debug, Clone)]
pub(super) struct Tier {
    /// Indices into the original `tool_calls` slice, in the order they should execute.
    pub indices: Vec<usize>,
}

/// Dependency graph over a batch of [`ToolUseRequest`]s.
///
/// Each node is an index into the original `tool_calls` slice. An edge `i → j` means
/// call `i` depends on call `j` (i.e., `j` must complete before `i` can start).
pub(super) struct ToolCallDag {
    /// `deps[i]` = list of indices that call `i` depends on.
    deps: Vec<Vec<usize>>,
    len: usize,
}

impl ToolCallDag {
    /// Build a dependency graph from `tool_calls`.
    ///
    /// A call at index `i` is considered to depend on call at index `j` if any leaf
    /// string value in `tool_calls[i].input` **exactly equals** `tool_calls[j].id`.
    /// Substring matches are intentionally excluded to avoid false positives with
    /// short provider-generated IDs (e.g. Ollama uses sequential integers: "0", "1", …).
    pub fn build(tool_calls: &[ToolUseRequest]) -> Self {
        let len = tool_calls.len();
        let mut deps: Vec<Vec<usize>> = vec![Vec::new(); len];

        for (i, tc_i) in tool_calls.iter().enumerate() {
            let values = extract_string_values(&tc_i.input);
            for (j, tc_j) in tool_calls.iter().enumerate() {
                if i == j {
                    continue;
                }
                // IMP-01: exact whole-value matching — the string must *equal* the id,
                // not merely contain it as a substring.
                if values.iter().any(|v| v == &tc_j.id) {
                    deps[i].push(j);
                }
            }
        }

        Self { deps, len }
    }

    /// Returns `true` when no call depends on any other call.
    ///
    /// When trivial, [`tiers`] returns a single tier containing all call indices.
    /// Callers can use this to log or trace the dispatch mode.
    #[must_use]
    pub(super) fn is_trivial(&self) -> bool {
        self.deps.iter().all(Vec::is_empty)
    }

    /// Partition calls into ordered tiers using Kahn's algorithm.
    ///
    /// Returns a `Vec<Tier>` where each tier's calls are independent of one another
    /// and only depend on calls in earlier tiers. Tiers must be executed in order;
    /// calls within the same tier can run concurrently.
    ///
    /// # Cycle detection
    ///
    /// If the dependency graph contains a cycle (malformed LLM output), a warning is
    /// logged and a single tier containing **all** indices in original order is returned.
    /// The caller must execute them sequentially. This is the simplest, safest fallback:
    /// the cycle case is abnormal and optimizing it (e.g., running non-cyclic calls in
    /// parallel) adds complexity for no practical benefit.
    pub fn tiers(&self) -> Vec<Tier> {
        if self.len == 0 {
            return Vec::new();
        }

        // Kahn's algorithm: compute in-degree for each node.
        let mut in_degree: Vec<usize> = vec![0; self.len];
        // Build reverse adjacency for "who depends on me" lookups.
        let mut rev: Vec<Vec<usize>> = vec![Vec::new(); self.len];
        for (i, node_deps) in self.deps.iter().enumerate() {
            for &j in node_deps {
                in_degree[i] += 1;
                rev[j].push(i);
            }
        }

        let mut queue: Vec<usize> = (0..self.len).filter(|&i| in_degree[i] == 0).collect();

        let mut tiers: Vec<Tier> = Vec::new();
        let mut processed = 0_usize;

        while !queue.is_empty() {
            // All nodes currently in the queue form one tier.
            let current = std::mem::take(&mut queue);
            processed += current.len();
            // Discover next tier: decrement in-degree of dependents.
            let mut next: Vec<usize> = Vec::new();
            for &node in &current {
                for &dep_of_node in &rev[node] {
                    in_degree[dep_of_node] -= 1;
                    if in_degree[dep_of_node] == 0 {
                        next.push(dep_of_node);
                    }
                }
            }
            tiers.push(Tier { indices: current });
            queue = next;
        }

        if processed < self.len {
            // Cycle detected — fall back to sequential execution of all calls.
            // IMP-03: on cycle, execute ALL calls (not just the cyclic subset) in original
            // order. This is the simplest, safest fallback for malformed LLM output.
            tracing::warn!(
                total = self.len,
                processed,
                "tool_call_dag: cycle detected in tool_use_id references — \
                 falling back to fully sequential execution of all tool calls"
            );
            return vec![Tier {
                indices: (0..self.len).collect(),
            }];
        }

        tiers
    }
}

/// Recursively collect all leaf string values from a JSON value.
///
/// Only leaf nodes (strings) are collected; object keys are ignored. Arrays and
/// objects are traversed recursively. Used for exact-match dependency detection
/// against `tool_use_id` values, both at DAG build time and at tier dispatch time
/// (IMP-02 prerequisite failure propagation in `native.rs`).
pub(super) fn extract_string_values(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_strings(value, &mut out);
    out
}

fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(arr) => {
            for item in arr {
                collect_strings(item, out);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_strings(v, out);
            }
        }
        // Numbers, booleans, null — not strings, skip.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_call(id: &str, input: Value) -> ToolUseRequest {
        ToolUseRequest {
            id: id.to_owned(),
            name: "test_tool".to_owned(),
            input,
        }
    }

    // Helper: flatten tier vec into a vec of vecs of indices for easy assertion.
    fn tier_indices(tiers: &[Tier]) -> Vec<Vec<usize>> {
        tiers.iter().map(|t| t.indices.clone()).collect()
    }

    #[test]
    fn dag_empty_is_trivial() {
        let dag = ToolCallDag::build(&[]);
        assert!(dag.is_trivial());
        assert!(dag.tiers().is_empty());
    }

    #[test]
    fn dag_single_call_is_trivial() {
        let calls = [make_call("id0", json!({"x": "hello"}))];
        let dag = ToolCallDag::build(&calls);
        assert!(dag.is_trivial());
        let tiers = dag.tiers();
        assert_eq!(tier_indices(&tiers), vec![vec![0usize]]);
    }

    #[test]
    fn dag_no_dependencies_is_trivial() {
        let calls = [
            make_call("toolu_aaa", json!({"file": "foo.txt"})),
            make_call("toolu_bbb", json!({"cmd": "ls"})),
            make_call("toolu_ccc", json!({"url": "https://example.com"})),
        ];
        let dag = ToolCallDag::build(&calls);
        assert!(dag.is_trivial());
        let tiers = dag.tiers();
        // All three in a single tier (order may vary; check set equality via sort).
        assert_eq!(tiers.len(), 1);
        let mut got = tiers[0].indices.clone();
        got.sort_unstable();
        assert_eq!(got, vec![0usize, 1, 2]);
    }

    #[test]
    fn dag_single_dependency_two_tiers() {
        // B depends on A: A must run first, then B.
        let calls = [
            make_call("id_a", json!({"x": 1})),
            make_call("id_b", json!({"prerequisite": "id_a"})), // exact match
        ];
        let dag = ToolCallDag::build(&calls);
        assert!(!dag.is_trivial());
        let tiers = dag.tiers();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].indices, vec![0usize]); // A first
        assert_eq!(tiers[1].indices, vec![1usize]); // B second
    }

    #[test]
    fn dag_linear_chain() {
        // A → B → C (each depends on the previous)
        let calls = [
            make_call("id_a", json!({})),
            make_call("id_b", json!({"dep": "id_a"})),
            make_call("id_c", json!({"dep": "id_b"})),
        ];
        let dag = ToolCallDag::build(&calls);
        let tiers = dag.tiers();
        assert_eq!(tiers.len(), 3);
        assert_eq!(tiers[0].indices, vec![0usize]);
        assert_eq!(tiers[1].indices, vec![1usize]);
        assert_eq!(tiers[2].indices, vec![2usize]);
    }

    #[test]
    fn dag_diamond_dependency() {
        // A → {B, C} → D
        // D depends on B and C; B and C each depend on A.
        let calls = [
            make_call("id_a", json!({})),
            make_call("id_b", json!({"source": "id_a"})),
            make_call("id_c", json!({"source": "id_a"})),
            make_call("id_d", json!({"left": "id_b", "right": "id_c"})),
        ];
        let dag = ToolCallDag::build(&calls);
        let tiers = dag.tiers();
        assert_eq!(tiers.len(), 3);
        assert_eq!(tiers[0].indices, vec![0usize]); // A
        let mut tier1 = tiers[1].indices.clone();
        tier1.sort_unstable();
        assert_eq!(tier1, vec![1usize, 2]); // B and C (parallel)
        assert_eq!(tiers[2].indices, vec![3usize]); // D
    }

    #[test]
    fn dag_cycle_falls_back_to_sequential() {
        // A → B → A (cycle)
        let calls = [
            make_call("id_a", json!({"dep": "id_b"})),
            make_call("id_b", json!({"dep": "id_a"})),
        ];
        let dag = ToolCallDag::build(&calls);
        let tiers = dag.tiers();
        // Cycle: single tier with all indices in original order.
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].indices, vec![0usize, 1]);
    }

    #[test]
    fn dag_partial_cycle_with_independent() {
        // A → B → A (cycle) + C (independent)
        // IMP-03/SUG-03: all calls must appear in the single fallback tier.
        let calls = [
            make_call("id_a", json!({"dep": "id_b"})),
            make_call("id_b", json!({"dep": "id_a"})),
            make_call("id_c", json!({"x": "hello"})),
        ];
        let dag = ToolCallDag::build(&calls);
        let tiers = dag.tiers();
        assert_eq!(tiers.len(), 1);
        let mut got = tiers[0].indices.clone();
        got.sort_unstable();
        assert_eq!(got, vec![0usize, 1, 2]);
    }

    #[test]
    fn dag_short_id_no_false_positive() {
        // IMP-01 / SUG-02: Ollama-style sequential IDs ("0", "1", "2").
        // Tool B has input {"retries": "1"} — should NOT depend on tool A with id="1".
        let calls = [
            make_call("1", json!({"cmd": "ls"})),
            make_call("2", json!({"retries": "1", "count": "10"})),
        ];
        // With exact matching we CANNOT distinguish coincidental equality from
        // intentional reference when both the id and the value are "1". Exact matching
        // DOES create a dependency here — this is a known limitation documented in the
        // handoff. The important guarantee: it does NOT match when the id is merely a
        // SUBSTRING (e.g., id="1" inside "10"), which is the key regression from the
        // original substring approach.
        let dag = ToolCallDag::build(&calls);
        // "retries": "1" exactly equals id "1" → dependency IS detected (unavoidable with
        // exact matching and short provider IDs; document this known limitation).
        assert!(
            !dag.is_trivial(),
            "exact match on id='1' with value '1' creates a dependency (known limitation)"
        );
        // The important fix: id="1" must NOT match "10" as a substring.
        // This test verifies that "retries": "10" does NOT falsely match id="1"
        // (substring matching would match "1" inside "10"; exact matching does not).
        let calls2 = [
            make_call("1", json!({"cmd": "ls"})),
            make_call("2", json!({"count": "10", "flag": "true"})),
        ];
        let dag2 = ToolCallDag::build(&calls2);
        assert!(dag2.is_trivial(), "id='1' must not match substring in '10'");
        // Also verify that id="call_abc" does not match "call_abcdef" as substring.
        let calls3 = [
            make_call("call_abc", json!({"cmd": "ls"})),
            make_call("call_xyz", json!({"path": "call_abcdef/file.txt"})),
        ];
        let dag3 = ToolCallDag::build(&calls3);
        assert!(dag3.is_trivial(), "id must not match as substring of path");
    }

    #[test]
    fn dag_nested_json_strings_detected() {
        // Dependency hidden in nested JSON structure.
        let calls = [
            make_call("id_src", json!({})),
            make_call("id_dst", json!({"nested": {"deep": ["id_src"]}})),
        ];
        let dag = ToolCallDag::build(&calls);
        assert!(!dag.is_trivial());
        let tiers = dag.tiers();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].indices, vec![0usize]);
        assert_eq!(tiers[1].indices, vec![1usize]);
    }

    #[test]
    fn dag_self_reference_ignored() {
        // A call referencing its own id must not create a self-edge.
        let calls = [make_call("id_self", json!({"ref": "id_self"}))];
        let dag = ToolCallDag::build(&calls);
        assert!(dag.is_trivial());
    }
}
