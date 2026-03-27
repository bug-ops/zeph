// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the tool dependency graph (issue #2024).
//!
//! Covers multi-turn dependency chains, deadlock fallback, cycle detection,
//! preference boost capping, always-on/name-mentioned bypass, session scope,
//! and unknown tool ID handling.

use std::collections::{HashMap, HashSet};

use zeph_tools::config::ToolDependency;
use zeph_tools::schema_filter::{InclusionReason, ToolDependencyGraph, ToolFilterResult};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_dep(requires: &[&str], prefers: &[&str]) -> ToolDependency {
    ToolDependency {
        requires: requires.iter().map(|s| (*s).to_owned()).collect(),
        prefers: prefers.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn build_graph(rules: &[(&str, &[&str], &[&str])]) -> ToolDependencyGraph {
    let map: HashMap<String, ToolDependency> = rules
        .iter()
        .map(|(id, req, pref)| ((*id).to_owned(), make_dep(req, pref)))
        .collect();
    ToolDependencyGraph::new(map)
}

fn completed(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

fn always_on(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

/// Build a `ToolFilterResult` that pre-includes `included_ids`.
/// Uses a no-embedding filter so every tool gets the `NoEmbedding` reason.
fn make_filter_result(
    included_ids: &[&str],
    always_on_ids: &[&str],
    name_mentioned_ids: &[&str],
) -> ToolFilterResult {
    let mut included = HashSet::new();
    let mut inclusion_reasons = Vec::new();

    for id in always_on_ids {
        included.insert((*id).to_owned());
        inclusion_reasons.push(((*id).to_owned(), InclusionReason::AlwaysOn));
    }
    for id in name_mentioned_ids {
        if included.insert((*id).to_owned()) {
            inclusion_reasons.push(((*id).to_owned(), InclusionReason::NameMentioned));
        }
    }
    for id in included_ids {
        if included.insert((*id).to_owned()) {
            inclusion_reasons.push(((*id).to_owned(), InclusionReason::SimilarityRank));
        }
    }

    let excluded = Vec::new();
    let scores: Vec<(String, f32)> = included.iter().map(|id| (id.clone(), 0.5_f32)).collect();

    ToolFilterResult {
        included,
        excluded,
        scores,
        inclusion_reasons,
        dependency_exclusions: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// 1. Multi-turn dependency chains: search → format_results → save
// ---------------------------------------------------------------------------

/// Turn 1: only `search` is available (no prerequisites completed).
/// `format_results` requires `search`; `save` requires `format_results`.
#[test]
fn multi_turn_chain_turn1_only_search_available() {
    let graph = build_graph(&[
        ("format_results", &["search"], &[]),
        ("save", &["format_results"], &[]),
    ]);

    // Turn 1: nothing completed yet.
    let mut result = make_filter_result(&["search", "format_results", "save"], &[], &[]);
    graph.apply(&mut result, &completed(&[]), 0.15, 0.20, &always_on(&[]));

    // `format_results` and `save` should be gated.
    assert!(
        result.included.contains("search"),
        "search must be available on turn 1"
    );
    assert!(
        !result.included.contains("format_results"),
        "format_results must be gated until search completes"
    );
    assert!(
        !result.included.contains("save"),
        "save must be gated until format_results completes"
    );

    assert_eq!(result.dependency_exclusions.len(), 2);
    let gated_ids: HashSet<&str> = result
        .dependency_exclusions
        .iter()
        .map(|e| e.tool_id.as_str())
        .collect();
    assert!(gated_ids.contains("format_results"));
    assert!(gated_ids.contains("save"));
}

/// Turn 2: `search` has completed → `format_results` unlocks; `save` still gated.
#[test]
fn multi_turn_chain_turn2_format_results_unlocked() {
    let graph = build_graph(&[
        ("format_results", &["search"], &[]),
        ("save", &["format_results"], &[]),
    ]);

    let mut result = make_filter_result(&["search", "format_results", "save"], &[], &[]);
    graph.apply(
        &mut result,
        &completed(&["search"]),
        0.15,
        0.20,
        &always_on(&[]),
    );

    assert!(result.included.contains("search"));
    assert!(
        result.included.contains("format_results"),
        "format_results must unlock after search completes"
    );
    assert!(
        !result.included.contains("save"),
        "save must remain gated until format_results completes"
    );

    assert_eq!(result.dependency_exclusions.len(), 1);
    assert_eq!(result.dependency_exclusions[0].tool_id, "save");
}

/// Turn 3: both `search` and `format_results` completed → all three tools available.
#[test]
fn multi_turn_chain_turn3_all_tools_available() {
    let graph = build_graph(&[
        ("format_results", &["search"], &[]),
        ("save", &["format_results"], &[]),
    ]);

    let mut result = make_filter_result(&["search", "format_results", "save"], &[], &[]);
    graph.apply(
        &mut result,
        &completed(&["search", "format_results"]),
        0.15,
        0.20,
        &always_on(&[]),
    );

    assert!(result.included.contains("search"));
    assert!(result.included.contains("format_results"));
    assert!(
        result.included.contains("save"),
        "save must unlock after format_results completes"
    );
    assert!(result.dependency_exclusions.is_empty());
}

// ---------------------------------------------------------------------------
// 2. Deadlock fallback (CRIT-01)
// ---------------------------------------------------------------------------

/// All non-always-on tools are blocked by unmet dependencies.
/// Deadlock fallback must disable hard gates for the turn.
#[test]
fn deadlock_fallback_all_non_always_on_blocked() {
    // `tool_a` requires `missing_prerequisite` which will never be completed.
    // `tool_b` requires `also_missing`.
    // Both non-always-on tools would be gated → deadlock → fallback activates.
    let graph = build_graph(&[
        ("tool_a", &["missing_prerequisite"], &[]),
        ("tool_b", &["also_missing"], &[]),
    ]);

    let mut result = make_filter_result(&["tool_a", "tool_b"], &["bash"], &[]);
    graph.apply(
        &mut result,
        &completed(&[]),
        0.15,
        0.20,
        &always_on(&["bash"]),
    );

    // Deadlock fallback: both tools must remain available.
    assert!(
        result.included.contains("tool_a"),
        "tool_a must remain available via deadlock fallback"
    );
    assert!(
        result.included.contains("tool_b"),
        "tool_b must remain available via deadlock fallback"
    );
    assert!(
        result.dependency_exclusions.is_empty(),
        "dependency_exclusions must be cleared on deadlock fallback"
    );
}

/// Deadlock is NOT triggered if at least one non-always-on tool passes the gate.
#[test]
fn no_deadlock_when_at_least_one_tool_passes() {
    let graph = build_graph(&[("gated_tool", &["needed"], &[])]);

    // `free_tool` has no requires → always passes gate.
    // Only `gated_tool` is blocked.
    let mut result = make_filter_result(&["gated_tool", "free_tool"], &["bash"], &[]);
    graph.apply(
        &mut result,
        &completed(&[]),
        0.15,
        0.20,
        &always_on(&["bash"]),
    );

    // No deadlock: `gated_tool` excluded, `free_tool` stays.
    assert!(
        !result.included.contains("gated_tool"),
        "gated_tool should be excluded"
    );
    assert!(
        result.included.contains("free_tool"),
        "free_tool must remain available"
    );
    assert_eq!(result.dependency_exclusions.len(), 1);
    assert_eq!(result.dependency_exclusions[0].tool_id, "gated_tool");
}

// ---------------------------------------------------------------------------
// 3. Cycle detection (CRIT-02)
// ---------------------------------------------------------------------------

/// Simple two-node cycle: A requires B, B requires A.
/// Both must be unconditionally available after graph construction.
#[test]
fn cycle_simple_two_node_both_released() {
    let graph = build_graph(&[("tool_a", &["tool_b"], &[]), ("tool_b", &["tool_a"], &[])]);

    assert!(
        graph.requirements_met("tool_a", &completed(&[])),
        "tool_a must be unconditionally available after cycle resolution"
    );
    assert!(
        graph.requirements_met("tool_b", &completed(&[])),
        "tool_b must be unconditionally available after cycle resolution"
    );
}

/// Three-node cycle: A → B → C → A.
/// All three cycle participants must have their requires cleared.
#[test]
fn cycle_three_node_all_released() {
    let graph = build_graph(&[
        ("tool_a", &["tool_c"], &[]),
        ("tool_b", &["tool_a"], &[]),
        ("tool_c", &["tool_b"], &[]),
    ]);

    assert!(graph.requirements_met("tool_a", &completed(&[])));
    assert!(graph.requirements_met("tool_b", &completed(&[])));
    assert!(graph.requirements_met("tool_c", &completed(&[])));
}

/// Mixed graph: cycle {C, D} + independent non-cycle chain (E requires F, no cycle).
/// The DFS collect-all-in-stack algorithm marks all nodes on the path to a back-edge as
/// cycled. The test therefore uses a disconnected non-cycle chain so we can verify that
/// tools NOT reachable from a cycle remain properly gated.
#[test]
fn cycle_mixed_graph_non_cycle_tools_remain_gated() {
    // Disconnected graph: A→B (no cycle) and C↔D (cycle).
    // A and B are in a separate DFS tree and are NOT part of the cycle.
    let graph = build_graph(&[
        ("tool_a", &["tool_b"], &[]), // linear: A requires B
        ("tool_c", &["tool_d"], &[]),
        ("tool_d", &["tool_c"], &[]), // cycle: c <-> d
    ]);

    // Cycle participants tool_c and tool_d are released.
    assert!(graph.requirements_met("tool_c", &completed(&[])));
    assert!(graph.requirements_met("tool_d", &completed(&[])));

    // tool_a is NOT in a cycle: it remains gated until tool_b is completed.
    assert!(!graph.requirements_met("tool_a", &completed(&[])));
    assert!(graph.requirements_met("tool_a", &completed(&["tool_b"])));

    // tool_b has no requires in this graph → always available.
    assert!(graph.requirements_met("tool_b", &completed(&[])));
}

/// Self-loop: A requires A.
#[test]
fn cycle_self_loop_tool_released() {
    let graph = build_graph(&[("tool_a", &["tool_a"], &[])]);
    assert!(
        graph.requirements_met("tool_a", &completed(&[])),
        "self-loop must be resolved: tool available unconditionally"
    );
}

// ---------------------------------------------------------------------------
// 4. Preference boost capping (HIGH-04)
// ---------------------------------------------------------------------------

/// Tool with base score 0.35 + 3 satisfied `prefers` at 0.15 each = 0.45 boost,
/// but capped at `max_total_boost=0.20` → final boost = 0.20, not 0.45.
#[test]
fn preference_boost_capped_at_max_total_boost() {
    let graph = build_graph(&[("format", &[], &["dep_a", "dep_b", "dep_c"])]);

    let boost = graph.preference_boost(
        "format",
        &completed(&["dep_a", "dep_b", "dep_c"]),
        0.15,
        0.20,
    );
    // Expected: min(3 * 0.15, 0.20) = 0.20
    assert!(
        (boost - 0.20).abs() < 1e-5,
        "boost must be capped at max_total_boost=0.20, got {boost}"
    );
}

/// Partial satisfaction: 1 of 3 `prefers` met → boost = 0.15 (under cap).
#[test]
fn preference_boost_partial_satisfaction_not_capped() {
    let graph = build_graph(&[("format", &[], &["dep_a", "dep_b", "dep_c"])]);

    let boost = graph.preference_boost("format", &completed(&["dep_a"]), 0.15, 0.20);
    assert!(
        (boost - 0.15).abs() < 1e-5,
        "single satisfied prefers should yield exactly 0.15, got {boost}"
    );
}

/// Verify that scores are re-sorted after boost application.
#[test]
fn preference_boost_re_sorts_scores_descending() {
    let graph = build_graph(&[
        // `format` has 2 satisfied prefers → boost = 0.30, capped at 0.20
        ("format", &[], &["dep_a", "dep_b"]),
    ]);

    // Start: format score = 0.30, other_tool score = 0.60.
    // After boost (capped 0.20): format = 0.50, other_tool = 0.60.
    // other_tool should still rank first.
    let mut result = ToolFilterResult {
        included: ["format".to_owned(), "other_tool".to_owned()].into(),
        excluded: Vec::new(),
        scores: vec![("other_tool".to_owned(), 0.60), ("format".to_owned(), 0.30)],
        inclusion_reasons: vec![
            ("other_tool".to_owned(), InclusionReason::SimilarityRank),
            ("format".to_owned(), InclusionReason::SimilarityRank),
        ],
        dependency_exclusions: Vec::new(),
    };

    graph.apply(
        &mut result,
        &completed(&["dep_a", "dep_b"]),
        0.15,
        0.20,
        &always_on(&[]),
    );

    // format score should be approximately 0.50
    let format_score = result
        .scores
        .iter()
        .find(|(id, _)| id == "format")
        .map(|(_, s)| *s);
    assert!(
        format_score.is_some_and(|s| (s - 0.50).abs() < 1e-4),
        "format score after boost should be ~0.50, got {format_score:?}"
    );

    // Scores must be sorted descending.
    let scores: Vec<f32> = result.scores.iter().map(|(_, s)| *s).collect();
    let mut sorted = scores.clone();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    assert_eq!(
        scores, sorted,
        "scores must be sorted descending after boost"
    );
}

// ---------------------------------------------------------------------------
// 5. Name-mentioned & always-on bypass
// ---------------------------------------------------------------------------

/// Name-mentioned tools bypass hard dependency gates by design.
#[test]
fn name_mentioned_bypasses_hard_gate() {
    let graph = build_graph(&[("secret_tool", &["never_completed"], &[])]);

    // `secret_tool` is name-mentioned → bypasses gate.
    let mut result = make_filter_result(&[], &[], &["secret_tool"]);
    graph.apply(&mut result, &completed(&[]), 0.15, 0.20, &always_on(&[]));

    assert!(
        result.included.contains("secret_tool"),
        "name-mentioned tool must bypass hard dependency gate"
    );
    assert!(
        result.dependency_exclusions.is_empty(),
        "name-mentioned tool must not appear in dependency_exclusions"
    );
}

/// Always-on tools bypass hard dependency gates even if configured with `requires`.
#[test]
fn always_on_bypasses_hard_gate() {
    let graph = build_graph(&[("bash", &["impossible"], &[])]);

    let mut result = make_filter_result(&[], &["bash"], &[]);
    graph.apply(
        &mut result,
        &completed(&[]),
        0.15,
        0.20,
        &always_on(&["bash"]),
    );

    assert!(
        result.included.contains("bash"),
        "always-on tool must bypass hard dependency gate"
    );
    assert!(result.dependency_exclusions.is_empty());
}

/// A tool included via `SimilarityRank` is subject to hard gates (not bypassed).
/// Deadlock fallback does NOT trigger here because `free_tool` (no requires) keeps
/// one non-always-on tool available after gating.
#[test]
fn similarity_rank_tool_is_gated() {
    let graph = build_graph(&[("apply_patch", &["read_file"], &[])]);

    // `free_tool` has no requires → always passes gate.
    // With `free_tool` present, applying the gate to `apply_patch` does not
    // trigger deadlock fallback (non_always_on_included=2, to_exclude=1).
    let mut result = make_filter_result(&["apply_patch", "free_tool"], &["bash"], &[]);
    graph.apply(
        &mut result,
        &completed(&[]),
        0.15,
        0.20,
        &always_on(&["bash"]),
    );

    assert!(
        !result.included.contains("apply_patch"),
        "similarity-rank tool with unmet requires must be excluded"
    );
    assert!(
        result.included.contains("free_tool"),
        "free_tool with no requires must remain"
    );
    assert_eq!(result.dependency_exclusions.len(), 1);
    assert_eq!(
        result.dependency_exclusions[0].unmet_requires,
        vec!["read_file"]
    );
}

// ---------------------------------------------------------------------------
// 6. Session scope: completed tools persist across turns, clear on reset
// ---------------------------------------------------------------------------

/// Simulates `completed_tool_ids` growing monotonically across turns.
#[test]
fn session_scope_completed_ids_grow_across_turns() {
    let graph = build_graph(&[
        ("format_results", &["search"], &[]),
        ("save", &["format_results"], &[]),
    ]);

    // Turn 1: nothing done.
    let mut done: HashSet<String> = HashSet::new();
    assert!(!graph.requirements_met("format_results", &done));
    assert!(!graph.requirements_met("save", &done));

    // "search" completes.
    done.insert("search".into());

    // Turn 2: format_results unlocked.
    assert!(graph.requirements_met("format_results", &done));
    assert!(!graph.requirements_met("save", &done));

    // "format_results" completes.
    done.insert("format_results".into());

    // Turn 3: save unlocked.
    assert!(graph.requirements_met("save", &done));
}

/// On session clear (`/clear` equivalent), `completed_tool_ids` is reset.
#[test]
fn session_scope_clear_resets_completed_ids() {
    let graph = build_graph(&[("save", &["search"], &[])]);

    let mut done: HashSet<String> = HashSet::new();
    done.insert("search".into());

    // save is accessible.
    assert!(graph.requirements_met("save", &done));

    // Simulate /clear: reset completed state.
    done.clear();

    // save is gated again after clear.
    assert!(!graph.requirements_met("save", &done));
}

// ---------------------------------------------------------------------------
// 7. Unknown tool IDs in config (graceful handling)
// ---------------------------------------------------------------------------

/// A configured rule referencing a tool not in the available set should not
/// affect tools that are available.
#[test]
fn unknown_tool_in_requires_tool_still_available_if_requires_met() {
    // `real_tool` requires `real_prereq`, which will be in completed set.
    // `ghost_tool` is configured but never appears in included set → no-op.
    let graph = build_graph(&[
        ("real_tool", &["real_prereq"], &[]),
        ("ghost_tool", &["something"], &[]), // ghost_tool never in filter result
    ]);

    let mut result = make_filter_result(&["real_tool"], &["bash"], &[]);
    graph.apply(
        &mut result,
        &completed(&["real_prereq"]),
        0.15,
        0.20,
        &always_on(&["bash"]),
    );

    assert!(
        result.included.contains("real_tool"),
        "real_tool with met requires must be included"
    );
    assert!(result.dependency_exclusions.is_empty());
}

/// A `requires` entry referencing a non-existent tool ID means the prerequisite
/// can never be completed → tool is gated until deadlock fallback applies.
#[test]
fn unknown_required_tool_id_gates_dependent_tool() {
    let graph = build_graph(&[("tool_a", &["nonexistent_tool_xyz"], &[])]);

    let mut result = make_filter_result(&["tool_a"], &["bash"], &[]);
    graph.apply(
        &mut result,
        &completed(&[]),
        0.15,
        0.20,
        &always_on(&["bash"]),
    );

    // tool_a has unmet requires (nonexistent_tool_xyz never completes).
    // No deadlock: bash (always-on) is the only other tool, so tool_a is gated.
    // But wait: non_always_on_included=1, to_exclude.len()=1 → deadlock fallback!
    // So tool_a must remain included.
    assert!(
        result.included.contains("tool_a"),
        "deadlock fallback must apply: tool_a is the only non-always-on tool"
    );
}

/// With two non-always-on tools, one gated, one free → no deadlock, gated is excluded.
#[test]
fn unknown_required_tool_id_no_deadlock_when_other_tool_free() {
    let graph = build_graph(&[("tool_a", &["nonexistent_tool_xyz"], &[])]);

    // `free_tool` has no requires → always passes.
    let mut result = make_filter_result(&["tool_a", "free_tool"], &["bash"], &[]);
    graph.apply(
        &mut result,
        &completed(&[]),
        0.15,
        0.20,
        &always_on(&["bash"]),
    );

    // No deadlock: free_tool still passes gate.
    assert!(
        !result.included.contains("tool_a"),
        "tool_a must be gated (unknown requires)"
    );
    assert!(
        result.included.contains("free_tool"),
        "free_tool with no requires must be included"
    );
    assert_eq!(result.dependency_exclusions.len(), 1);
    assert_eq!(
        result.dependency_exclusions[0].unmet_requires,
        vec!["nonexistent_tool_xyz"]
    );
}

// ---------------------------------------------------------------------------
// 8. filter_tool_names: used in native tool loop (iteration 1+)
// ---------------------------------------------------------------------------

/// `filter_tool_names` lets through tools with met requirements and always-on.
#[test]
fn filter_tool_names_passes_met_and_always_on() {
    let graph = build_graph(&[("apply_patch", &["read"], &[]), ("save", &["write"], &[])]);

    let names = &["bash", "read", "apply_patch", "save", "free_tool"];
    let ao = always_on(&["bash"]);

    // `read` is completed → `apply_patch` passes. `write` not completed → `save` blocked.
    let passed = graph.filter_tool_names(names, &completed(&["read"]), &ao);
    let passed_set: HashSet<&&str> = passed.iter().collect();

    assert!(passed_set.contains(&"bash"), "always-on must pass");
    assert!(
        passed_set.contains(&"read"),
        "read has no requires → passes"
    );
    assert!(
        passed_set.contains(&"apply_patch"),
        "apply_patch requires met"
    );
    assert!(
        passed_set.contains(&"free_tool"),
        "free_tool not in graph → passes"
    );
    assert!(
        !passed_set.contains(&"save"),
        "save requires write, not met"
    );
}

/// `filter_tool_names` with empty graph passes all names.
#[test]
fn filter_tool_names_empty_graph_passes_all() {
    let graph = ToolDependencyGraph::new(HashMap::new());
    let names = &["bash", "grep", "save"];
    let passed = graph.filter_tool_names(names, &completed(&[]), &always_on(&["bash"]));
    assert_eq!(passed.len(), 3, "empty graph must pass all tool names");
}

// ---------------------------------------------------------------------------
// 9. DependencyConfig TOML deserialization
// ---------------------------------------------------------------------------

/// Verify that the TOML config for [tools.dependencies] deserializes correctly.
#[test]
fn toml_dependency_config_deserializes() {
    let toml_str = r#"
        [dependencies]
        enabled = true
        boost_per_dep = 0.10
        max_total_boost = 0.30

        [dependencies.rules.format_results]
        requires = ["search"]
        prefers = ["validate"]

        [dependencies.rules.save]
        requires = ["format_results"]
    "#;

    let config: zeph_tools::config::ToolsConfig = toml::from_str(toml_str).unwrap();
    let dep = &config.dependencies;

    assert!(dep.enabled);
    assert!((dep.boost_per_dep - 0.10).abs() < 1e-6);
    assert!((dep.max_total_boost - 0.30).abs() < 1e-6);

    let fmt = dep
        .rules
        .get("format_results")
        .expect("format_results rule must exist");
    assert_eq!(fmt.requires, vec!["search"]);
    assert_eq!(fmt.prefers, vec!["validate"]);

    let save = dep.rules.get("save").expect("save rule must exist");
    assert_eq!(save.requires, vec!["format_results"]);
    assert!(save.prefers.is_empty());
}

/// Default `DependencyConfig`: disabled, defaults for boost values.
#[test]
fn dependency_config_default_disabled() {
    let config = zeph_tools::config::DependencyConfig::default();
    assert!(
        !config.enabled,
        "dependency config must be disabled by default"
    );
    assert!((config.boost_per_dep - 0.15).abs() < 1e-6);
    assert!((config.max_total_boost - 0.20).abs() < 1e-6);
    assert!(config.rules.is_empty());
}

/// An empty [dependencies] section uses all defaults.
#[test]
fn toml_empty_dependencies_section_uses_defaults() {
    let toml_str = "[dependencies]";
    let config: zeph_tools::config::ToolsConfig = toml::from_str(toml_str).unwrap();
    let dep = &config.dependencies;

    assert!(!dep.enabled);
    assert!((dep.boost_per_dep - 0.15).abs() < 1e-6);
    assert!((dep.max_total_boost - 0.20).abs() < 1e-6);
    assert!(dep.rules.is_empty());
}

// ---------------------------------------------------------------------------
// 10. unmet_requires diagnostic output
// ---------------------------------------------------------------------------

/// `unmet_requires` returns only the prerequisites that are not yet completed.
#[test]
fn unmet_requires_returns_only_missing_deps() {
    let graph = build_graph(&[("save", &["search", "validate", "format_results"], &[])]);

    // Complete `search` and `validate` but not `format_results`.
    let unmet = graph.unmet_requires("save", &completed(&["search", "validate"]));
    assert_eq!(unmet, vec!["format_results"]);
}

/// `unmet_requires` returns empty for unconfigured tools.
#[test]
fn unmet_requires_empty_for_unconfigured_tool() {
    let graph = build_graph(&[("save", &["search"], &[])]);
    let unmet = graph.unmet_requires("unknown_tool", &completed(&[]));
    assert!(unmet.is_empty());
}

/// `unmet_requires` returns empty when all deps satisfied.
#[test]
fn unmet_requires_empty_when_all_satisfied() {
    let graph = build_graph(&[("save", &["a", "b"], &[])]);
    let unmet = graph.unmet_requires("save", &completed(&["a", "b"]));
    assert!(unmet.is_empty());
}

// ---------------------------------------------------------------------------
// 11. is_empty guard
// ---------------------------------------------------------------------------

#[test]
fn is_empty_returns_true_for_default_graph() {
    let graph = ToolDependencyGraph::default();
    assert!(graph.is_empty());
}

#[test]
fn is_empty_returns_false_when_rules_present() {
    let graph = build_graph(&[("tool_a", &["tool_b"], &[])]);
    assert!(!graph.is_empty());
}

// ---------------------------------------------------------------------------
// 12. apply is a no-op for empty graph
// ---------------------------------------------------------------------------

#[test]
fn apply_noop_for_empty_graph() {
    let graph = ToolDependencyGraph::default();
    let mut result = make_filter_result(&["tool_a", "tool_b"], &["bash"], &[]);
    let original_included: HashSet<String> = result.included.clone();

    graph.apply(
        &mut result,
        &completed(&[]),
        0.15,
        0.20,
        &always_on(&["bash"]),
    );

    assert_eq!(
        result.included, original_included,
        "empty graph must not change included set"
    );
    assert!(result.dependency_exclusions.is_empty());
}
