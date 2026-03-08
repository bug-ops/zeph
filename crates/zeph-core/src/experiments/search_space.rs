// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Search space definition for parameter variation experiments.

use serde::{Deserialize, Serialize};

use super::types::ParameterKind;

/// A continuous or discrete range for a single tunable parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterRange {
    pub kind: ParameterKind,
    pub min: f64,
    pub max: f64,
    /// Discrete step size. `None` means continuous (deduplication is effectively disabled).
    pub step: Option<f64>,
    pub default: f64,
}

impl ParameterRange {
    /// Number of discrete steps in this range, or `None` if step is not set or is non-positive.
    #[must_use]
    pub fn step_count(&self) -> Option<usize> {
        let step = self.step?;
        if step <= 0.0 {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(((self.max - self.min) / step).floor() as usize + 1)
    }

    /// Clamp `value` to `[min, max]`.
    #[must_use]
    pub fn clamp(&self, value: f64) -> f64 {
        value.clamp(self.min, self.max)
    }

    /// Return `true` if `value` is within `[min, max]`.
    #[must_use]
    pub fn contains(&self, value: f64) -> bool {
        (self.min..=self.max).contains(&value)
    }

    /// Quantize `value` to the nearest grid step anchored at `min`.
    ///
    /// Formula: `min + ((value - min) / step).round() * step`, then clamped to `[min, max]`.
    /// Anchoring at `min` ensures grid points align to `{min, min+step, min+2*step, ...}`.
    #[must_use]
    pub fn quantize(&self, value: f64) -> f64 {
        if let Some(step) = self.step
            && step > 0.0
        {
            let quantized = self.min + ((value - self.min) / step).round() * step;
            return self.clamp(quantized);
        }
        value
    }

    /// Validate that this range is internally consistent.
    ///
    /// Returns `false` if `min > max`, any value is non-finite, or `step` is non-positive.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.min.is_finite()
            && self.max.is_finite()
            && self.default.is_finite()
            && self.min <= self.max
            && self.step.is_none_or(|s| s.is_finite() && s > 0.0)
    }
}

/// The set of parameter ranges that define the experiment search space.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchSpace {
    pub parameters: Vec<ParameterRange>,
}

impl Default for SearchSpace {
    fn default() -> Self {
        Self {
            parameters: vec![
                ParameterRange {
                    kind: ParameterKind::Temperature,
                    min: 0.0,
                    max: 2.0,
                    step: Some(0.1),
                    default: 0.7,
                },
                ParameterRange {
                    kind: ParameterKind::TopP,
                    min: 0.1,
                    max: 1.0,
                    step: Some(0.05),
                    default: 0.9,
                },
                ParameterRange {
                    kind: ParameterKind::TopK,
                    min: 1.0,
                    max: 100.0,
                    step: Some(5.0),
                    default: 40.0,
                },
                ParameterRange {
                    kind: ParameterKind::FrequencyPenalty,
                    min: -2.0,
                    max: 2.0,
                    step: Some(0.2),
                    default: 0.0,
                },
                ParameterRange {
                    kind: ParameterKind::PresencePenalty,
                    min: -2.0,
                    max: 2.0,
                    step: Some(0.2),
                    default: 0.0,
                },
            ],
        }
    }
}

impl SearchSpace {
    /// Find the range for a given `ParameterKind`, if present.
    #[must_use]
    pub fn range_for(&self, kind: ParameterKind) -> Option<&ParameterRange> {
        self.parameters.iter().find(|r| r.kind == kind)
    }

    /// Validate all parameter ranges in this search space.
    ///
    /// Returns `false` if any range has `min > max`, non-finite values, or non-positive step.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.parameters.iter().all(ParameterRange::is_valid)
    }

    /// Total number of grid points across all parameters that have a step.
    ///
    /// This is the number of distinct variations a `GridStep` strategy will generate.
    #[must_use]
    pub fn grid_size(&self) -> usize {
        self.parameters
            .iter()
            .filter_map(ParameterRange::step_count)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_count_with_step() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 0.0,
            max: 1.0,
            step: Some(0.5),
            default: 0.5,
        };
        assert_eq!(r.step_count(), Some(3)); // 0.0, 0.5, 1.0
    }

    #[test]
    fn step_count_no_step() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 0.0,
            max: 1.0,
            step: None,
            default: 0.5,
        };
        assert_eq!(r.step_count(), None);
    }

    #[test]
    fn step_count_zero_step() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 0.0,
            max: 1.0,
            step: Some(0.0),
            default: 0.5,
        };
        assert_eq!(r.step_count(), None);
    }

    #[test]
    fn clamp_below_min() {
        let r = ParameterRange {
            kind: ParameterKind::TopP,
            min: 0.1,
            max: 1.0,
            step: Some(0.1),
            default: 0.9,
        };
        assert!((r.clamp(-1.0) - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_above_max() {
        let r = ParameterRange {
            kind: ParameterKind::TopP,
            min: 0.1,
            max: 1.0,
            step: Some(0.1),
            default: 0.9,
        };
        assert!((r.clamp(2.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_within_range() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 0.0,
            max: 2.0,
            step: Some(0.1),
            default: 0.7,
        };
        assert!((r.clamp(1.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn contains_within_range() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 0.0,
            max: 2.0,
            step: Some(0.1),
            default: 0.7,
        };
        assert!(r.contains(1.0));
        assert!(r.contains(0.0));
        assert!(r.contains(2.0));
        assert!(!r.contains(-0.1));
        assert!(!r.contains(2.1));
    }

    #[test]
    fn quantize_snaps_to_nearest_step() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 0.0,
            max: 2.0,
            step: Some(0.1),
            default: 0.7,
        };
        // 0.73 should snap to 0.7
        let q = r.quantize(0.73);
        assert!((q - 0.7).abs() < 1e-10, "expected 0.7, got {q}");
    }

    #[test]
    fn quantize_no_step_returns_value_unchanged() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 0.0,
            max: 2.0,
            step: None,
            default: 0.7,
        };
        assert!((r.quantize(1.234) - 1.234).abs() < f64::EPSILON);
    }

    #[test]
    fn quantize_clamps_result() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 0.0,
            max: 1.0,
            step: Some(0.1),
            default: 0.5,
        };
        // Large value quantizes to nearest step, then clamped
        let q = r.quantize(100.0);
        assert!(q <= 1.0, "quantize must clamp to max");
    }

    #[test]
    fn quantize_avoids_fp_accumulation() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 0.0,
            max: 2.0,
            step: Some(0.1),
            default: 0.7,
        };
        // 0.1 * 7 accumulates to 0.7000000000000001 via addition, quantize must fix this
        let accumulated = 0.1_f64 * 7.0;
        let q = r.quantize(accumulated);
        assert!(
            (q - 0.7).abs() < 1e-10,
            "expected 0.7, got {q} (accumulated={accumulated})"
        );
    }

    #[test]
    fn default_search_space_has_five_parameters() {
        let space = SearchSpace::default();
        assert_eq!(space.parameters.len(), 5);
    }

    #[test]
    fn default_grid_size_is_reasonable() {
        let space = SearchSpace::default();
        let size = space.grid_size();
        // Temperature: 21, TopP: 19, TopK: 20, Freq: 21, Pres: 21 = 102
        assert!(size > 0);
        assert!(size < 200);
    }

    #[test]
    fn range_for_finds_temperature() {
        let space = SearchSpace::default();
        let range = space.range_for(ParameterKind::Temperature);
        assert!(range.is_some());
        assert!((range.unwrap().default - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn range_for_missing_returns_none() {
        let space = SearchSpace::default();
        let range = space.range_for(ParameterKind::RetrievalTopK);
        assert!(range.is_none());
    }

    #[test]
    fn grid_size_empty_space_is_zero() {
        let space = SearchSpace { parameters: vec![] };
        assert_eq!(space.grid_size(), 0);
    }

    #[test]
    fn quantize_with_nonzero_min_anchors_to_min() {
        // TopK: min=1.0, step=5.0 => grid should be {1, 6, 11, 16, ...}
        let r = ParameterRange {
            kind: ParameterKind::TopK,
            min: 1.0,
            max: 100.0,
            step: Some(5.0),
            default: 40.0,
        };
        // 6.0 should stay at 6.0, not be shifted to 5.0
        let q = r.quantize(6.0);
        assert!(
            (q - 6.0).abs() < 1e-10,
            "expected 6.0 (min-anchored grid), got {q}"
        );
        // 3.0 is between 1.0 and 6.0; rounds to nearest => 1.0
        let q2 = r.quantize(3.0);
        assert!((q2 - 1.0).abs() < 1e-10, "expected 1.0, got {q2}");
    }

    #[test]
    fn quantize_negative_step_returns_unchanged() {
        // step <= 0 guard: quantize falls back to returning the value as-is
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 0.0,
            max: 2.0,
            step: Some(-0.1),
            default: 0.7,
        };
        assert!((r.quantize(0.75) - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn parameter_range_is_valid_for_default() {
        for r in &SearchSpace::default().parameters {
            assert!(r.is_valid(), "default range {:?} is invalid", r.kind);
        }
    }

    #[test]
    fn parameter_range_invalid_when_min_gt_max() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: 2.0,
            max: 0.0,
            step: Some(0.1),
            default: 1.0,
        };
        assert!(!r.is_valid());
    }

    #[test]
    fn parameter_range_invalid_when_nonfinite() {
        let r = ParameterRange {
            kind: ParameterKind::Temperature,
            min: f64::NAN,
            max: 2.0,
            step: Some(0.1),
            default: 0.7,
        };
        assert!(!r.is_valid());
    }

    #[test]
    fn search_space_is_valid_for_default() {
        assert!(SearchSpace::default().is_valid());
    }

    #[test]
    fn search_space_invalid_when_range_inverted() {
        let space = SearchSpace {
            parameters: vec![ParameterRange {
                kind: ParameterKind::Temperature,
                min: 2.0,
                max: 0.0,
                step: Some(0.1),
                default: 1.0,
            }],
        };
        assert!(!space.is_valid());
    }
}
