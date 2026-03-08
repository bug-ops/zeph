// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `VariationGenerator` trait for parameter variation strategies.

use std::collections::HashSet;

use super::snapshot::ConfigSnapshot;
use super::types::Variation;

/// A strategy for generating parameter variations one at a time.
///
/// Each call to [`VariationGenerator::next`] must produce a variation that
/// changes exactly one parameter from the baseline. The caller is responsible
/// for tracking visited variations and passing them to `next`.
///
/// Implementations hold mutable state (position cursor, RNG seed) and are
/// therefore `Send` but not required to be `Sync`. The experiment engine loop
/// is sequential and accesses the generator exclusively.
pub trait VariationGenerator: Send {
    /// Produce the next untested variation, or `None` if the space is exhausted.
    ///
    /// `baseline` is the current best-known configuration snapshot.
    /// `visited` is the set of all variations already tested in this run.
    fn next(
        &mut self,
        baseline: &ConfigSnapshot,
        visited: &HashSet<Variation>,
    ) -> Option<Variation>;

    /// Strategy name for logging and metrics.
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
