// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Neighborhood perturbation strategy for parameter variation.
//!
//! [`Neighborhood`] is a local-search strategy that generates variations by
//! perturbing the current baseline value of a randomly chosen parameter by a
//! small random amount proportional to the configured `radius`. It is most
//! effective after a coarse [`GridStep`] sweep has identified a promising region.
//!
//! [`GridStep`]: crate::GridStep

use std::collections::HashSet;

use ordered_float::OrderedFloat;
use rand::Rng as _;
use rand::SeedableRng as _;
use rand::rngs::SmallRng;

use super::error::EvalError;
use super::generator::VariationGenerator;
use super::search_space::SearchSpace;
use super::snapshot::ConfigSnapshot;
use super::types::{Variation, VariationValue};

/// Maximum number of retry attempts before giving up (space is considered exhausted).
const MAX_RETRIES: usize = 1000;

/// Fallback number of steps used when a parameter has no discrete step configured.
///
/// This gives a reasonable granularity for continuous parameters without requiring
/// an explicit step in the search space definition.
const DEFAULT_STEPS: f64 = 20.0;

/// Perturbation strategy that explores the neighborhood of the current baseline.
///
/// At each call, a parameter is chosen uniformly at random. The new value is
/// computed as `baseline_value ± U(-radius, radius) * step`, then clamped and
/// quantized to the nearest grid step. Useful after a [`GridStep`] sweep has
/// narrowed the search to a promising region.
///
/// The generator is seeded deterministically via `seed`, making experiments
/// reproducible. `radius` must be finite and positive (enforced in [`Neighborhood::new`]).
///
/// # Examples
///
/// ```rust
/// use std::collections::HashSet;
/// use zeph_experiments::{ConfigSnapshot, Neighborhood, SearchSpace, VariationGenerator};
///
/// let mut generator = Neighborhood::new(SearchSpace::default(), 1.0, 42).unwrap();
/// let baseline = ConfigSnapshot::default();
/// let visited = HashSet::new();
///
/// // Each call perturbs a random parameter by a small amount.
/// if let Some(v) = generator.next(&baseline, &visited) {
///     let val = v.value.as_f64();
///     assert!(val.is_finite());
/// }
/// ```
///
/// [`GridStep`]: crate::GridStep
pub struct Neighborhood {
    search_space: SearchSpace,
    radius: f64,
    rng: SmallRng,
}

impl Neighborhood {
    /// Create a new `Neighborhood` generator.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::InvalidRadius`] if `radius` is not finite and positive.
    pub fn new(search_space: SearchSpace, radius: f64, seed: u64) -> Result<Self, EvalError> {
        if !radius.is_finite() || radius <= 0.0 {
            return Err(EvalError::InvalidRadius { radius });
        }
        Ok(Self {
            search_space,
            radius,
            rng: SmallRng::seed_from_u64(seed),
        })
    }
}

impl VariationGenerator for Neighborhood {
    fn next(
        &mut self,
        baseline: &ConfigSnapshot,
        visited: &HashSet<Variation>,
    ) -> Option<Variation> {
        if self.search_space.parameters.is_empty() {
            return None;
        }
        for _ in 0..MAX_RETRIES {
            let idx = self.rng.gen_range(0..self.search_space.parameters.len());
            let range = &self.search_space.parameters[idx];
            let current = baseline.get(range.kind);
            // DEFAULT_STEPS is used when step is None (continuous parameter).
            let step = range
                .step
                .unwrap_or_else(|| (range.max - range.min) / DEFAULT_STEPS);
            let delta = self.rng.gen_range(-self.radius..=self.radius) * step;
            // Skip zero perturbations — they produce the baseline value, wasting an attempt.
            if delta.abs() < f64::EPSILON {
                continue;
            }
            let raw = current + delta;
            let value = range.quantize(range.clamp(raw));
            // Skip if the quantized value equals the baseline (no effective change).
            if (value - current).abs() < f64::EPSILON {
                continue;
            }
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
        "neighborhood"
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::collapsible_if,
        clippy::field_reassign_with_default,
        clippy::manual_midpoint,
        clippy::manual_range_contains
    )]

    use std::collections::HashSet;

    use super::super::search_space::ParameterRange;
    use super::super::types::ParameterKind;
    use super::*;

    fn make_space(kind: ParameterKind, min: f64, max: f64, step: f64) -> SearchSpace {
        SearchSpace {
            parameters: vec![ParameterRange {
                kind,
                min,
                max,
                step: Some(step),
                default: f64::midpoint(min, max),
            }],
        }
    }

    #[test]
    fn neighborhood_produces_values_in_range() {
        let space = make_space(ParameterKind::Temperature, 0.0, 2.0, 0.1);
        let mut generator = Neighborhood::new(space, 1.0, 42).unwrap();
        let baseline = ConfigSnapshot::default();
        let visited = HashSet::new();
        for _ in 0..20 {
            if let Some(v) = generator.next(&baseline, &visited) {
                let val = v.value.as_f64();
                assert!((0.0..=2.0).contains(&val), "out of range: {val}");
            }
        }
    }

    #[test]
    fn neighborhood_is_deterministic_with_same_seed() {
        let space = SearchSpace::default();
        let baseline = ConfigSnapshot::default();
        let visited = HashSet::new();
        let mut gen1 = Neighborhood::new(space.clone(), 1.0, 99).unwrap();
        let mut gen2 = Neighborhood::new(space, 1.0, 99).unwrap();
        let v1 = gen1.next(&baseline, &visited);
        let v2 = gen2.next(&baseline, &visited);
        assert_eq!(v1, v2, "same seed must produce same first variation");
    }

    #[test]
    fn neighborhood_skips_visited() {
        // Single-point space: min == max == 0.5, step 0.1
        let space = make_space(ParameterKind::Temperature, 0.5, 0.5, 0.1);
        let mut generator = Neighborhood::new(space, 1.0, 0).unwrap();
        let baseline = ConfigSnapshot::default();
        let mut visited = HashSet::new();
        visited.insert(Variation {
            parameter: ParameterKind::Temperature,
            value: VariationValue::Float(OrderedFloat(0.5)),
        });
        assert!(generator.next(&baseline, &visited).is_none());
    }

    #[test]
    fn neighborhood_empty_space_returns_none() {
        let mut generator = Neighborhood::new(SearchSpace { parameters: vec![] }, 1.0, 0).unwrap();
        let baseline = ConfigSnapshot::default();
        let visited = HashSet::new();
        assert!(generator.next(&baseline, &visited).is_none());
    }

    #[test]
    fn neighborhood_zero_radius_returns_error() {
        let result = Neighborhood::new(SearchSpace::default(), 0.0, 0);
        assert!(result.is_err(), "zero radius must be rejected");
    }

    #[test]
    fn neighborhood_negative_radius_returns_error() {
        let result = Neighborhood::new(SearchSpace::default(), -1.0, 0);
        assert!(result.is_err(), "negative radius must be rejected");
    }

    #[test]
    fn neighborhood_nan_radius_returns_error() {
        let result = Neighborhood::new(SearchSpace::default(), f64::NAN, 0);
        assert!(result.is_err(), "NaN radius must be rejected");
    }

    #[test]
    fn neighborhood_step_none_uses_default_steps() {
        // Continuous parameter (step=None) — neighborhood must still produce values.
        let space = SearchSpace {
            parameters: vec![super::super::search_space::ParameterRange {
                kind: ParameterKind::Temperature,
                min: 0.0,
                max: 2.0,
                step: None,
                default: 1.0,
            }],
        };
        let mut generator = Neighborhood::new(space, 1.0, 77).unwrap();
        let baseline = ConfigSnapshot::default();
        let visited = HashSet::new();
        // With DEFAULT_STEPS=20, perturbation step = 2.0/20.0 = 0.1; must get at least one result.
        let mut got_any = false;
        for _ in 0..50 {
            if generator.next(&baseline, &visited).is_some() {
                got_any = true;
                break;
            }
        }
        assert!(
            got_any,
            "should produce at least one variation for continuous parameter"
        );
    }

    #[test]
    fn neighborhood_quantizes_perturbed_values() {
        let space = make_space(ParameterKind::TopP, 0.1, 1.0, 0.05);
        let mut generator = Neighborhood::new(space, 2.0, 11).unwrap();
        let mut baseline = ConfigSnapshot::default();
        baseline.top_p = 0.5;
        let visited = HashSet::new();
        for _ in 0..30 {
            if let Some(v) = generator.next(&baseline, &visited) {
                let val = v.value.as_f64();
                // Quantized values must be multiples of 0.05 anchored at min=0.1:
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
    fn neighborhood_name() {
        let generator = Neighborhood::new(SearchSpace::default(), 1.0, 0).unwrap();
        assert_eq!(generator.name(), "neighborhood");
    }

    #[test]
    fn neighborhood_perturbs_around_baseline() {
        // Baseline temperature 0.7, radius 1.0, step 0.1 => perturbation in [-0.1, +0.1]
        // All values should be in [0.6, 0.8] within [0.0, 2.0]
        let space = make_space(ParameterKind::Temperature, 0.0, 2.0, 0.1);
        let mut generator = Neighborhood::new(space, 1.0, 55).unwrap();
        let baseline = ConfigSnapshot::default(); // temperature = 0.7
        let visited = HashSet::new();
        let mut temp_values = vec![];
        for _ in 0..50 {
            if let Some(v) = generator.next(&baseline, &visited)
                && v.parameter == ParameterKind::Temperature
            {
                temp_values.push(v.value.as_f64());
            }
        }
        assert!(
            !temp_values.is_empty(),
            "should produce temperature variations"
        );
        // All values must be within ±1 step of 0.7 (i.e., ±0.1, so [0.6, 0.8])
        for val in &temp_values {
            assert!(
                *val >= 0.6 - 1e-10 && *val <= 0.8 + 1e-10,
                "value {val} not within ±1 step of 0.7"
            );
        }
    }
}
