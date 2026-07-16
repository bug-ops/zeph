// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Active-set resolution for MemGuard-inspired type-aware retrieval composition (spec 004-16,
//! issue #6086).
//!
//! This module resolves the [`zeph_common::memory::FunctionalType`] set that
//! `zeph_context::assembler::schedule_context_fetchers` gates on, from
//! [`zeph_config::memory::TypeAwareComposeConfig`]. It never touches storage or write paths —
//! retrieval-only, fetch-time composition.

use zeph_common::memory::{FunctionalType, MemoryRoute, MemoryRouter};
use zeph_config::memory::TypeAwareComposeConfig;
use zeph_memory::{HeuristicRouter, IntentClass};

/// Static `IntentClass -> FunctionalType[]` widening table (spec 004-16 §3 Q3).
///
/// Used only when `intent_scoped = true`: it *adds* types to an already-resolved active set,
/// it never narrows. `IntentClass` is `#[non_exhaustive]`, so an unrecognised future variant
/// falls back to widening with nothing (conservative: no accidental over-composition).
fn intent_functional_types(intent: IntentClass) -> &'static [FunctionalType] {
    match intent {
        IntentClass::ProfileLookup => &[FunctionalType::UserFact],
        IntentClass::TargetedRetrieval => &[
            FunctionalType::Episodic,
            FunctionalType::UserFact,
            FunctionalType::CrossSessionSummary,
            FunctionalType::GraphFact,
        ],
        IntentClass::DeepReasoning => &[
            FunctionalType::Episodic,
            FunctionalType::ReasoningStrategy,
            FunctionalType::CrossSessionSummary,
            FunctionalType::GraphFact,
        ],
        _ => &[],
    }
}

/// Resolve the active `FunctionalType` set for the current turn.
///
/// Returns an empty `Vec` when `config.enabled` is `false` or when `default_compose_types`
/// is empty and `intent_scoped` is `false` — both cases mean "no type gating", which
/// `schedule_context_fetchers` treats identically to today's unfiltered composition
/// (spec 004-16 edge cases: `enabled = false` and empty `default_compose_types` are the same
/// no-op code path).
///
/// `intent_scoped = true` uses [`HeuristicRouter`] — a pure, synchronous, no-I/O function of
/// `query` — to classify the query into an [`IntentClass`] and widen the set via the static
/// table above. This adds no new LLM call (spec 004-16 §5 Multi-Model note): it reuses the same
/// heuristic router `MemFlow` tiered retrieval already uses for its no-LLM fallback path.
#[must_use]
pub fn resolve_active_functional_types(
    config: &TypeAwareComposeConfig,
    query: &str,
) -> Vec<FunctionalType> {
    if !config.enabled {
        return Vec::new();
    }

    let mut active = config.default_compose_types.clone();

    if config.intent_scoped {
        let route: MemoryRoute = HeuristicRouter.route(query);
        let intent = IntentClass::from_route(route);
        for t in intent_functional_types(intent) {
            if !active.contains(t) {
                active.push(*t);
            }
        }
    }

    tracing::debug!(
        enabled = config.enabled,
        intent_scoped = config.intent_scoped,
        ?active,
        "type-aware compose: resolved active set"
    );

    active
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_config_resolves_to_empty_set() {
        let config = TypeAwareComposeConfig {
            enabled: false,
            default_compose_types: vec![FunctionalType::UserFact],
            intent_scoped: true,
        };
        assert!(resolve_active_functional_types(&config, "anything").is_empty());
    }

    #[test]
    fn enabled_with_empty_default_and_no_intent_scoping_resolves_to_empty_set() {
        let config = TypeAwareComposeConfig {
            enabled: true,
            default_compose_types: Vec::new(),
            intent_scoped: false,
        };
        assert!(resolve_active_functional_types(&config, "anything").is_empty());
    }

    #[test]
    fn enabled_with_default_types_and_no_intent_scoping_returns_default_types() {
        let config = TypeAwareComposeConfig {
            enabled: true,
            default_compose_types: vec![FunctionalType::UserFact],
            intent_scoped: false,
        };
        let active = resolve_active_functional_types(&config, "what is my name");
        assert_eq!(active, vec![FunctionalType::UserFact]);
    }

    #[test]
    fn intent_scoped_widens_default_set_without_duplicates() {
        let config = TypeAwareComposeConfig {
            enabled: true,
            default_compose_types: vec![FunctionalType::UserFact],
            intent_scoped: true,
        };
        // A graph-style query routes to IntentClass::DeepReasoning via HeuristicRouter, which
        // widens with Episodic/ReasoningStrategy/CrossSessionSummary/GraphFact.
        let active = resolve_active_functional_types(&config, "why did the deploy fail?");
        assert!(active.contains(&FunctionalType::UserFact));
        // UserFact must appear exactly once even though the widening table for some intents
        // could otherwise duplicate an already-present type.
        assert_eq!(
            active
                .iter()
                .filter(|t| **t == FunctionalType::UserFact)
                .count(),
            1
        );
    }

    #[test]
    fn intent_functional_types_never_include_behavioral_rule() {
        // BehavioralRule is always-on/ungated (fetch_corrections) — the widening table must
        // never need to name it, since it is composed regardless of the active set.
        for intent in [
            IntentClass::ProfileLookup,
            IntentClass::TargetedRetrieval,
            IntentClass::DeepReasoning,
        ] {
            assert!(!intent_functional_types(intent).contains(&FunctionalType::BehavioralRule));
        }
    }
}
