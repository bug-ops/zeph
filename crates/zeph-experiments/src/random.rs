// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Uniform random sampling strategy for parameter variation.

use std::collections::HashSet;
use std::sync::Mutex;

use ordered_float::OrderedFloat;
use rand::Rng as _;
use rand::SeedableRng as _;
use rand::rngs::SmallRng;

use super::generator::VariationGenerator;
use super::search_space::SearchSpace;
use super::snapshot::ConfigSnapshot;
use super::types::{Variation, VariationValue};

/// Maximum number of retry attempts before giving up (space is considered exhausted).
const MAX_RETRIES: usize = 1000;

/// Uniform random sampling within parameter bounds.
///
/// At each call, a parameter is chosen uniformly at random, then a value is
/// sampled uniformly from its `[min, max]` range and quantized to the nearest
/// step (if configured). The sample is rejected if it was already visited.
/// Returns `None` after `MAX_RETRIES` consecutive rejections.
///
/// `rng` is wrapped in a [`Mutex`] so that `Random` implements [`Sync`], which is
/// required by [`VariationGenerator`] to allow [`ExperimentEngine`] to be used
/// with `tokio::spawn`. The mutex is only ever locked from a single thread
/// (the experiment loop is sequential), so there is no contention.
pub struct Random {
    search_space: SearchSpace,
    rng: Mutex<SmallRng>,
}

impl Random {
    /// Create a new `Random` generator with a deterministic seed.
    #[must_use]
    pub fn new(search_space: SearchSpace, seed: u64) -> Self {
        Self {
            search_space,
            rng: Mutex::new(SmallRng::seed_from_u64(seed)),
        }
    }
}

impl VariationGenerator for Random {
    fn next(
        &mut self,
        _baseline: &ConfigSnapshot,
        visited: &HashSet<Variation>,
    ) -> Option<Variation> {
        if self.search_space.parameters.is_empty() {
            return None;
        }
        let mut rng = self.rng.lock().expect("rng mutex poisoned");
        for _ in 0..MAX_RETRIES {
            let idx = rng.gen_range(0..self.search_space.parameters.len());
            let range = &self.search_space.parameters[idx];
            let raw: f64 = rng.gen_range(range.min..=range.max);
            let value = range.quantize(raw);
            let variation = Variation {
                parameter: range.kind,
                value: VariationValue::Float(OrderedFloat(value)),
            };
            if !visited.contains(&variation) {
                return Some(variation);
            }
        }
        None
    }

    fn name(&self) -> &'static str {
        "random"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::manual_range_contains)]

    use std::collections::HashSet;

    use super::super::search_space::ParameterRange;
    use super::super::types::ParameterKind;
    use super::*;

    #[test]
    fn random_produces_values_in_range() {
        let space = SearchSpace {
            parameters: vec![ParameterRange {
                kind: ParameterKind::Temperature,
                min: 0.0,
                max: 1.0,
                step: Some(0.1),
                default: 0.5,
            }],
        };
        let mut generator = Random::new(space, 42);
        let baseline = ConfigSnapshot::default();
        let visited = HashSet::new();
        for _ in 0..20 {
            if let Some(v) = generator.next(&baseline, &visited) {
                let val = v.value.as_f64();
                assert!((0.0..=1.0).contains(&val), "out of range: {val}");
            }
        }
    }

    #[test]
    fn random_skips_visited() {
        let space = SearchSpace {
            parameters: vec![ParameterRange {
                kind: ParameterKind::Temperature,
                min: 0.5,
                max: 0.5,
                step: Some(0.1),
                default: 0.5,
            }],
        };
        let mut generator = Random::new(space, 0);
        let baseline = ConfigSnapshot::default();
        let mut visited = HashSet::new();
        visited.insert(Variation {
            parameter: ParameterKind::Temperature,
            value: VariationValue::Float(OrderedFloat(0.5)),
        });
        // Only one point in space (min==max==0.5), so after visiting it, must return None.
        let result = generator.next(&baseline, &visited);
        assert!(
            result.is_none(),
            "expected None when only option is already visited"
        );
    }

    #[test]
    fn random_empty_space_returns_none() {
        let mut generator = Random::new(SearchSpace { parameters: vec![] }, 0);
        let baseline = ConfigSnapshot::default();
        let visited = HashSet::new();
        assert!(generator.next(&baseline, &visited).is_none());
    }

    #[test]
    fn random_is_deterministic_with_same_seed() {
        let space = SearchSpace::default();
        let baseline = ConfigSnapshot::default();
        let visited = HashSet::new();
        let mut gen1 = Random::new(space.clone(), 123);
        let mut gen2 = Random::new(space, 123);
        let v1 = gen1.next(&baseline, &visited);
        let v2 = gen2.next(&baseline, &visited);
        assert_eq!(v1, v2, "same seed must produce same first variation");
    }

    #[test]
    fn random_quantizes_sampled_values() {
        let space = SearchSpace {
            parameters: vec![ParameterRange {
                kind: ParameterKind::TopP,
                min: 0.1,
                max: 1.0,
                step: Some(0.05),
                default: 0.9,
            }],
        };
        let mut generator = Random::new(space, 7);
        let baseline = ConfigSnapshot::default();
        let visited = HashSet::new();
        for _ in 0..30 {
            if let Some(v) = generator.next(&baseline, &visited) {
                let val = v.value.as_f64();
                // Quantized values must be on the 0.05-step grid anchored at min=0.1:
                // i.e. (val - 0.1) / 0.05 must be an integer.
                let steps = (val - 0.1) / 0.05;
                assert!(
                    (steps - steps.round()).abs() < 1e-10,
                    "value {val} is not on the 0.05-step grid anchored at 0.1"
                );
            }
        }
    }

    #[test]
    fn random_name() {
        let generator = Random::new(SearchSpace::default(), 0);
        assert_eq!(generator.name(), "random");
    }

    #[test]
    fn random_is_sync() {
        fn assert_sync<T: Sync>() {}
        assert_sync::<Random>();
    }
}
