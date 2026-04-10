// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Systematic grid sweep strategy for parameter variation.
//!
//! [`GridStep`] iterates each parameter through its discrete steps in order,
//! skipping variations that have already been visited. This gives exhaustive
//! coverage of the search space and is well-suited as a first-pass exploration
//! before switching to a [`Neighborhood`] or [`Random`] strategy.
//!
//! [`Neighborhood`]: crate::Neighborhood
//! [`Random`]: crate::Random

use std::collections::HashSet;

use ordered_float::OrderedFloat;

use super::generator::VariationGenerator;
use super::search_space::SearchSpace;
use super::snapshot::ConfigSnapshot;
use super::types::{Variation, VariationValue};

/// Systematic grid sweep: iterate each parameter through its discrete steps, skip visited.
///
/// Parameters are swept one at a time. For each parameter, all grid points from
/// `min` to `max` (with the configured `step`) are enumerated in order. Already-visited
/// variations are skipped. When all steps for a parameter are exhausted, the next
/// parameter is tried. Returns `None` when the full grid has been visited.
///
/// When a parameter has no discrete `step`, [`GridStep`] falls back to
/// `(max - min) / 20` as the step size.
///
/// # Examples
///
/// ```rust
/// use std::collections::HashSet;
/// use zeph_experiments::{
///     ConfigSnapshot, GridStep, ParameterKind, ParameterRange, SearchSpace, VariationGenerator,
/// };
///
/// let space = SearchSpace {
///     parameters: vec![ParameterRange {
///         kind: ParameterKind::Temperature,
///         min: 0.0,
///         max: 1.0,
///         step: Some(0.5),
///         default: 0.5,
///     }],
/// };
/// let mut generator = GridStep::new(space);
/// let baseline = ConfigSnapshot::default();
/// let mut visited = HashSet::new();
///
/// // Produces 0.0, 0.5, 1.0 in order.
/// let mut count = 0;
/// while let Some(v) = generator.next(&baseline, &visited) {
///     visited.insert(v);
///     count += 1;
/// }
/// assert_eq!(count, 3);
/// ```
pub struct GridStep {
    search_space: SearchSpace,
    current_param: usize,
    current_step: usize,
}

impl GridStep {
    /// Create a new [`GridStep`] generator starting at the first grid point.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::{GridStep, SearchSpace, VariationGenerator};
    ///
    /// let generator = GridStep::new(SearchSpace::default());
    /// assert_eq!(generator.name(), "grid");
    /// ```
    #[must_use]
    pub fn new(search_space: SearchSpace) -> Self {
        Self {
            search_space,
            current_param: 0,
            current_step: 0,
        }
    }
}

impl VariationGenerator for GridStep {
    fn next(
        &mut self,
        _baseline: &ConfigSnapshot,
        visited: &HashSet<Variation>,
    ) -> Option<Variation> {
        while self.current_param < self.search_space.parameters.len() {
            let range = &self.search_space.parameters[self.current_param];
            let step = range.step.unwrap_or_else(|| (range.max - range.min) / 20.0);
            if step <= 0.0 {
                self.current_param += 1;
                self.current_step = 0;
                continue;
            }

            #[allow(clippy::cast_precision_loss)]
            let raw = range.min + step * self.current_step as f64;

            if raw > range.max + f64::EPSILON {
                self.current_param += 1;
                self.current_step = 0;
                continue;
            }

            self.current_step += 1;

            // Quantize to avoid floating-point accumulation before deduplication.
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
        "grid"
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::super::search_space::ParameterRange;
    use super::super::types::ParameterKind;
    use super::*;

    fn single_param_space(min: f64, max: f64, step: f64) -> SearchSpace {
        SearchSpace {
            parameters: vec![ParameterRange {
                kind: ParameterKind::Temperature,
                min,
                max,
                step: Some(step),
                default: min,
            }],
        }
    }

    #[test]
    fn grid_step_produces_values_in_range() {
        let mut generator = GridStep::new(single_param_space(0.0, 1.0, 0.5));
        let baseline = ConfigSnapshot::default();
        let mut visited = HashSet::new();
        let mut values = vec![];
        while let Some(v) = generator.next(&baseline, &visited) {
            visited.insert(v.clone());
            values.push(v.value.as_f64());
        }
        assert_eq!(values.len(), 3, "0.0, 0.5, 1.0");
        for v in &values {
            assert!(*v >= 0.0 && *v <= 1.0);
        }
    }

    #[test]
    fn grid_step_skips_visited() {
        let mut generator = GridStep::new(single_param_space(0.0, 1.0, 0.5));
        let baseline = ConfigSnapshot::default();
        let mut visited = HashSet::new();
        visited.insert(Variation {
            parameter: ParameterKind::Temperature,
            value: VariationValue::Float(OrderedFloat(0.0)),
        });
        let first = generator.next(&baseline, &visited).unwrap();
        assert!(
            (first.value.as_f64() - 0.5).abs() < 1e-10,
            "expected 0.5, got {}",
            first.value.as_f64()
        );
    }

    #[test]
    fn grid_step_returns_none_when_exhausted() {
        let mut generator = GridStep::new(single_param_space(0.0, 0.0, 1.0));
        let baseline = ConfigSnapshot::default();
        let mut visited = HashSet::new();
        // Only one point: 0.0
        generator.next(&baseline, &visited).unwrap();
        visited.insert(Variation {
            parameter: ParameterKind::Temperature,
            value: VariationValue::Float(OrderedFloat(0.0)),
        });
        assert!(generator.next(&baseline, &visited).is_none());
    }

    #[test]
    fn grid_step_multiple_params() {
        let space = SearchSpace {
            parameters: vec![
                ParameterRange {
                    kind: ParameterKind::Temperature,
                    min: 0.0,
                    max: 0.5,
                    step: Some(0.5),
                    default: 0.0,
                },
                ParameterRange {
                    kind: ParameterKind::TopP,
                    min: 0.5,
                    max: 1.0,
                    step: Some(0.5),
                    default: 0.5,
                },
            ],
        };
        let mut generator = GridStep::new(space);
        let baseline = ConfigSnapshot::default();
        let mut visited = HashSet::new();
        let mut results = vec![];
        while let Some(v) = generator.next(&baseline, &visited) {
            visited.insert(v.clone());
            results.push(v);
        }
        // Temperature: 0.0, 0.5 — TopP: 0.5, 1.0
        assert_eq!(results.len(), 4);
        let temp_count = results
            .iter()
            .filter(|v| v.parameter == ParameterKind::Temperature)
            .count();
        let top_p_count = results
            .iter()
            .filter(|v| v.parameter == ParameterKind::TopP)
            .count();
        assert_eq!(temp_count, 2);
        assert_eq!(top_p_count, 2);
    }

    #[test]
    fn grid_step_quantizes_to_avoid_fp_drift() {
        // 0.1 * 7 via accumulation = 0.7000000000000001
        // quantize must snap to 0.7
        let mut generator = GridStep::new(single_param_space(0.0, 1.0, 0.1));
        let baseline = ConfigSnapshot::default();
        let mut visited = HashSet::new();
        let mut values = vec![];
        while let Some(v) = generator.next(&baseline, &visited) {
            visited.insert(v.clone());
            values.push(v.value.as_f64());
        }
        // All values should be clean multiples of 0.1
        for v in &values {
            let rounded = (v * 10.0).round() / 10.0;
            assert!(
                (v - rounded).abs() < 1e-10,
                "value {v} is not a clean multiple of 0.1"
            );
        }
    }

    #[test]
    fn grid_step_empty_space_returns_none() {
        let mut generator = GridStep::new(SearchSpace { parameters: vec![] });
        let baseline = ConfigSnapshot::default();
        let visited = HashSet::new();
        assert!(generator.next(&baseline, &visited).is_none());
    }

    #[test]
    fn grid_step_none_step_uses_fallback() {
        // Parameter with step=None — GridStep falls back to (max-min)/20.0 as step size.
        let space = SearchSpace {
            parameters: vec![ParameterRange {
                kind: ParameterKind::Temperature,
                min: 0.0,
                max: 1.0,
                step: None,
                default: 0.5,
            }],
        };
        let mut generator = GridStep::new(space);
        let baseline = ConfigSnapshot::default();
        let mut visited = HashSet::new();
        let mut count = 0;
        while let Some(v) = generator.next(&baseline, &visited) {
            visited.insert(v.clone());
            count += 1;
        }
        // With step = 1.0/20.0, there should be 21 steps (0.0, 0.05, ..., 1.0)
        assert_eq!(
            count, 21,
            "expected 21 steps for step=None with DEFAULT_STEPS=20"
        );
    }

    #[test]
    fn grid_step_name() {
        let generator = GridStep::new(SearchSpace::default());
        assert_eq!(generator.name(), "grid");
    }
}
