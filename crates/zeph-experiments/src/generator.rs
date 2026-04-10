// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`VariationGenerator`] trait for parameter variation strategies.
//!
//! Implement this trait to plug a custom search strategy into [`ExperimentEngine`].
//! Three built-in implementations are provided:
//!
//! - [`GridStep`] — systematic sweep through all discrete grid points.
//! - [`Random`] — uniform random sampling within parameter bounds.
//! - [`Neighborhood`] — perturbation around the current best configuration.
//!
//! [`ExperimentEngine`]: crate::ExperimentEngine
//! [`GridStep`]: crate::GridStep
//! [`Random`]: crate::Random
//! [`Neighborhood`]: crate::Neighborhood

use std::collections::HashSet;

use super::snapshot::ConfigSnapshot;
use super::types::Variation;

/// A strategy for generating parameter variations one at a time.
///
/// Each call to [`VariationGenerator::next`] must produce a variation that changes
/// exactly one parameter from the baseline. The caller ([`ExperimentEngine`]) is
/// responsible for tracking visited variations and passing them back via `visited`.
///
/// Implementations hold mutable state (position cursor, RNG seed) and must be
/// both `Send` and `Sync` so that [`ExperimentEngine`] can be used with
/// `tokio::spawn`. The engine loop accesses the generator exclusively via `&mut self`,
/// so no concurrent access occurs in practice.
///
/// # Implementing a Custom Generator
///
/// ```rust
/// use std::collections::HashSet;
/// use zeph_experiments::{ConfigSnapshot, ParameterKind, Variation, VariationValue, VariationGenerator};
///
/// /// Always suggests temperature = 0.5, then exhausts.
/// struct FixedSuggestion;
///
/// impl VariationGenerator for FixedSuggestion {
///     fn next(&mut self, _baseline: &ConfigSnapshot, visited: &HashSet<Variation>) -> Option<Variation> {
///         let v = Variation {
///             parameter: ParameterKind::Temperature,
///             value: VariationValue::from(0.5_f64),
///         };
///         if visited.contains(&v) { None } else { Some(v) }
///     }
///
///     fn name(&self) -> &'static str { "fixed" }
/// }
///
/// fn main() {
///     let mut variation_gen = FixedSuggestion;
///     let baseline = ConfigSnapshot::default();
///     let mut visited = HashSet::new();
///     let first = variation_gen.next(&baseline, &visited).unwrap();
///     visited.insert(first);
///     assert!(variation_gen.next(&baseline, &visited).is_none());
/// }
/// ```
///
/// [`ExperimentEngine`]: crate::ExperimentEngine
pub trait VariationGenerator: Send + Sync {
    /// Produce the next untested variation, or `None` if the space is exhausted.
    ///
    /// - `baseline` — the current best-known configuration snapshot (updated on acceptance).
    /// - `visited` — all variations already tested in this session; must not be returned again.
    fn next(
        &mut self,
        baseline: &ConfigSnapshot,
        visited: &HashSet<Variation>,
    ) -> Option<Variation>;

    /// Strategy name used in log messages and experiment reports.
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::super::types::{ParameterKind, VariationValue};
    use super::*;
    use ordered_float::OrderedFloat;

    struct AlwaysOne;

    impl VariationGenerator for AlwaysOne {
        fn next(
            &mut self,
            _baseline: &ConfigSnapshot,
            visited: &HashSet<Variation>,
        ) -> Option<Variation> {
            let v = Variation {
                parameter: ParameterKind::Temperature,
                value: VariationValue::Float(OrderedFloat(1.0)),
            };
            if visited.contains(&v) { None } else { Some(v) }
        }

        fn name(&self) -> &'static str {
            "always_one"
        }
    }

    #[test]
    fn generator_returns_variation_when_not_visited() {
        let mut generator = AlwaysOne;
        let baseline = ConfigSnapshot::default();
        let visited = HashSet::new();
        let v = generator.next(&baseline, &visited);
        assert!(v.is_some());
        assert_eq!(v.unwrap().parameter, ParameterKind::Temperature);
    }

    #[test]
    fn generator_returns_none_when_visited() {
        let mut generator = AlwaysOne;
        let baseline = ConfigSnapshot::default();
        let mut visited = HashSet::new();
        visited.insert(Variation {
            parameter: ParameterKind::Temperature,
            value: VariationValue::Float(OrderedFloat(1.0)),
        });
        assert!(generator.next(&baseline, &visited).is_none());
    }

    #[test]
    fn generator_name_is_static_str() {
        let generator = AlwaysOne;
        assert_eq!(generator.name(), "always_one");
    }

    #[test]
    fn generator_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<AlwaysOne>();
    }
}
