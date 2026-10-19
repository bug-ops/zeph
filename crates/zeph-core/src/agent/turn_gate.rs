// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Frozen per-turn tool set and the dispatch-time gates that enforce it.
//!
//! The tool array sent to the provider must not change between the iterations of one tool loop,
//! otherwise the cached request prefix is invalidated. The loop therefore resolves a
//! [`TurnToolSet`] once, and enforcement moves from "hide the tool" to "reject the call":
//! [`TurnToolSet`] rejects names outside the set, [`RequiresGate`] rejects tools whose hard
//! `requires` prerequisites have not completed, and [`DispatchGate`] combines both.

use std::collections::HashSet;
use std::sync::Arc;

use zeph_common::ToolName;
use zeph_llm::provider::ToolDefinition;
use zeph_tools::ToolDependencyGraph;

/// Why a tool call was refused by a dispatch gate.
///
/// A rejection is a policy decision, not an execution failure: callers surface it as a
/// synthetic `[error]` tool result so it never triggers retry or self-reflection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GateRejection {
    /// The tool is not part of this turn's frozen tool set.
    #[error("tool `{tool}` is not available in this turn")]
    NotInTurnSet {
        /// Name of the rejected tool.
        tool: ToolName,
    },
    /// Hard `requires` prerequisites of the tool have not completed yet.
    #[error("tool `{tool}` requires {} to run first", unmet.join(", "))]
    UnmetRequires {
        /// Name of the rejected tool.
        tool: ToolName,
        /// Prerequisite tool ids that have not completed in this session.
        unmet: Vec<String>,
    },
}

/// Names of the tools offered to the model for one user turn.
///
/// Built once at the start of the tool loop and never changed afterwards. Membership is the
/// first dispatch gate: a call to a tool outside the set is rejected.
#[derive(Debug, Clone, Default)]
pub struct TurnToolSet(Arc<HashSet<ToolName>>);

impl TurnToolSet {
    /// Resolve the set from the full candidate definitions.
    ///
    /// `candidates` is the schema-filter result including tools held back only by unmet
    /// `requires` (those stay visible and are rejected at dispatch); `None` keeps every
    /// definition. Names in `always_present` (built-in tools and the retrieval tool the utility
    /// gate may mandate) are kept even when the filter dropped them.
    pub fn resolve(
        all_defs: &[ToolDefinition],
        candidates: Option<&HashSet<String>>,
        always_present: &HashSet<ToolName>,
    ) -> Self {
        let names = all_defs
            .iter()
            .filter(|d| {
                candidates.is_none_or(|c| c.contains(d.name.as_str()))
                    || always_present.contains(&d.name)
            })
            .map(|d| d.name.clone())
            .collect();
        Self(Arc::new(names))
    }

    /// Whether `tool` is part of the set.
    #[must_use]
    pub fn contains(&self, tool: &str) -> bool {
        self.0.contains(tool)
    }

    /// The definitions of the set's members, preserving the order of `all_defs`.
    pub fn select(&self, all_defs: &[ToolDefinition]) -> Vec<ToolDefinition> {
        all_defs
            .iter()
            .filter(|d| self.0.contains(&d.name))
            .cloned()
            .collect()
    }

    /// Names of the set's members.
    pub fn names(&self) -> impl Iterator<Item = &ToolName> {
        self.0.iter()
    }

    /// Reject `tool` unless it belongs to the set.
    pub fn check(&self, tool: &str) -> Result<(), GateRejection> {
        if self.contains(tool) {
            Ok(())
        } else {
            Err(GateRejection::NotInTurnSet {
                tool: ToolName::new(tool),
            })
        }
    }
}

/// Snapshot of the hard `requires` state used to gate calls at dispatch time.
///
/// The snapshot is rebuilt after every tool tier, so a prerequisite completed in one tier
/// unlocks its dependants in the next. Only `always_on` tools bypass the gate; a tool name
/// mentioned by the user does not.
#[derive(Debug, Clone)]
pub struct RequiresGate {
    graph: Option<Arc<ToolDependencyGraph>>,
    completed: HashSet<String>,
    always_on: HashSet<String>,
    deadlocked: bool,
}

impl RequiresGate {
    /// Gate that lets every call through.
    pub fn open() -> Self {
        Self {
            graph: None,
            completed: HashSet::new(),
            always_on: HashSet::new(),
            deadlocked: false,
        }
    }

    /// Snapshot the dependency state.
    ///
    /// When `scope` is given and every non-always-on tool in it is blocked, the gate opens for
    /// the snapshot: the dependency graph must not be able to lock the whole turn out of its
    /// tools. Without a scope (the PASTE path, which runs before the turn set exists) the gate
    /// stays closed on unmet `requires`.
    pub fn snapshot(
        graph: Option<&Arc<ToolDependencyGraph>>,
        completed: &HashSet<String>,
        always_on: &HashSet<String>,
        scope: Option<&TurnToolSet>,
    ) -> Self {
        let graph = graph.filter(|g| !g.is_empty()).map(Arc::clone);
        let deadlocked = match (&graph, scope) {
            (Some(g), Some(scope)) => {
                let mut candidates = scope
                    .names()
                    .filter(|n| !always_on.contains(n.as_str()))
                    .peekable();
                candidates.peek().is_some()
                    && candidates.all(|n| !g.requirements_met(n.as_str(), completed))
            }
            _ => false,
        };
        if deadlocked {
            tracing::warn!(
                "tool dependency graph: all non-always-on tools gated; \
                 disabling hard gates for this snapshot"
            );
        }
        Self {
            graph,
            completed: completed.clone(),
            always_on: always_on.clone(),
            deadlocked,
        }
    }

    /// Reject `tool` while its hard `requires` prerequisites are incomplete.
    pub fn check(&self, tool: &str) -> Result<(), GateRejection> {
        let Some(graph) = self.graph.as_ref().filter(|_| !self.deadlocked) else {
            return Ok(());
        };
        if self.always_on.contains(tool) {
            return Ok(());
        }
        let unmet = graph.unmet_requires(tool, &self.completed);
        if unmet.is_empty() {
            return Ok(());
        }
        Err(GateRejection::UnmetRequires {
            tool: ToolName::new(tool),
            unmet: unmet.into_iter().map(str::to_owned).collect(),
        })
    }
}

/// Both dispatch layers: turn-set membership (when a set exists) and `requires`.
#[derive(Debug, Clone)]
pub struct DispatchGate {
    turn_set: Option<TurnToolSet>,
    requires: RequiresGate,
}

impl DispatchGate {
    /// Gate for the tool loop: membership in `turn_set` plus the `requires` snapshot.
    pub fn new(turn_set: TurnToolSet, requires: RequiresGate) -> Self {
        Self {
            turn_set: Some(turn_set),
            requires,
        }
    }

    /// Gate for callers that run before the turn set exists: `requires` only.
    pub fn requires_only(requires: RequiresGate) -> Self {
        Self {
            turn_set: None,
            requires,
        }
    }

    /// Gate that lets every call through.
    pub fn open() -> Self {
        Self::requires_only(RequiresGate::open())
    }

    /// Check `tool` against both layers, membership first.
    pub fn check(&self, tool: &str) -> Result<(), GateRejection> {
        if let Some(set) = &self.turn_set {
            set.check(tool)?;
        }
        self.requires.check(tool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: String::new(),
            parameters: serde_json::json!({}),
            output_schema: None,
        }
    }

    fn graph(rules: &[(&str, &[&str])]) -> Arc<ToolDependencyGraph> {
        let deps = rules
            .iter()
            .map(|(tool, requires)| {
                (
                    (*tool).to_owned(),
                    zeph_tools::ToolDependency {
                        requires: requires.iter().map(|r| (*r).to_owned()).collect(),
                        prefers: Vec::new(),
                    },
                )
            })
            .collect();
        Arc::new(ToolDependencyGraph::new(deps))
    }

    fn set(names: &[&str]) -> TurnToolSet {
        let defs: Vec<_> = names.iter().map(|n| def(n)).collect();
        TurnToolSet::resolve(&defs, None, &HashSet::new())
    }

    fn done(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn resolve_keeps_candidates_and_always_present() {
        let defs = vec![def("read"), def("write"), def("start_focus"), def("other")];
        let candidates: HashSet<String> = ["read".into(), "write".into()].into();
        let always: HashSet<ToolName> = ["start_focus".into()].into();
        let set = TurnToolSet::resolve(&defs, Some(&candidates), &always);
        assert!(set.contains("read") && set.contains("write") && set.contains("start_focus"));
        assert!(!set.contains("other"));
        let names: Vec<_> = set.select(&defs).into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["read", "write", "start_focus"]);
    }

    #[test]
    fn turn_set_rejects_unknown_tool() {
        let err = set(&["read"]).check("write").unwrap_err();
        assert!(matches!(err, GateRejection::NotInTurnSet { .. }));
    }

    #[test]
    fn requires_gate_rejects_until_prerequisite_completed() {
        let g = graph(&[("write", &["read"])]);
        let scope = set(&["read", "write", "bash"]);
        let blocked = RequiresGate::snapshot(Some(&g), &done(&[]), &done(&["bash"]), Some(&scope));
        let err = blocked.check("write").unwrap_err();
        assert_eq!(
            err,
            GateRejection::UnmetRequires {
                tool: "write".into(),
                unmet: vec!["read".into()],
            }
        );
        assert!(blocked.check("read").is_ok());
        let unlocked =
            RequiresGate::snapshot(Some(&g), &done(&["read"]), &done(&["bash"]), Some(&scope));
        assert!(unlocked.check("write").is_ok());
    }

    #[test]
    fn requires_gate_always_on_bypasses() {
        let g = graph(&[("bash", &["read"])]);
        let scope = set(&["bash", "write"]);
        let gate = RequiresGate::snapshot(Some(&g), &done(&[]), &done(&["bash"]), Some(&scope));
        assert!(gate.check("bash").is_ok());
    }

    #[test]
    fn deadlock_fallback_opens_gate_when_every_tool_is_blocked() {
        let g = graph(&[("write", &["read"]), ("patch", &["read"])]);
        let scope = set(&["write", "patch"]);
        let gate = RequiresGate::snapshot(Some(&g), &done(&[]), &done(&[]), Some(&scope));
        assert!(gate.check("write").is_ok());
        assert!(gate.check("patch").is_ok());
    }

    #[test]
    fn requires_only_snapshot_fails_closed_without_scope() {
        let g = graph(&[("write", &["read"])]);
        let gate = RequiresGate::snapshot(Some(&g), &done(&[]), &done(&[]), None);
        assert!(gate.check("write").is_err());
    }

    #[test]
    fn dispatch_gate_checks_membership_before_requires() {
        let g = graph(&[("write", &["read"])]);
        let scope = set(&["write"]);
        let requires = RequiresGate::snapshot(Some(&g), &done(&[]), &done(&[]), None);
        let gate = DispatchGate::new(scope, requires);
        assert!(matches!(
            gate.check("missing").unwrap_err(),
            GateRejection::NotInTurnSet { .. }
        ));
        assert!(matches!(
            gate.check("write").unwrap_err(),
            GateRejection::UnmetRequires { .. }
        ));
    }

    #[test]
    fn open_gate_allows_everything() {
        assert!(DispatchGate::open().check("anything").is_ok());
    }
}
