// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Search space definition for parameter variation experiments.

use serde::{Deserialize, Serialize};

use super::error::EvalError;
use super::types::ParameterKind;

/// A continuous or discrete range for a single tunable parameter.
///
/// When `step` is `Some`, the parameter is treated as discrete: values are
/// quantized to the nearest grid point anchored at `min`. When `step` is `None`
/// the parameter is treated as continuous and generators fall back to an internal
/// default step count (typically 20 divisions).
///
/// Invariants enforced by [`ParameterRange::new`]:
/// - `min < max` (both must be finite)
/// - `min <= default <= max` (`default` must be finite)
/// - `step`, when `Some`, must be finite and positive
///
/// # Examples
///
/// ```rust
/// use zeph_experiments::{ParameterRange, ParameterKind};
///
/// let range = ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, Some(0.1), 0.7).unwrap();
///
/// assert_eq!(range.step_count(), Some(11));
/// assert!((range.clamp(2.0) - 1.0).abs() < f64::EPSILON);
/// assert!((range.quantize(0.73) - 0.7).abs() < 1e-10);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RawParameterRange")]
pub struct ParameterRange {
    kind: ParameterKind,
    min: f64,
    max: f64,
    step: Option<f64>,
    default: f64,
}

/// Deserialization shadow of [`ParameterRange`] with the same fields but no invariants.
///
/// Deserializing straight into `ParameterRange` would populate its private fields directly,
/// bypassing [`ParameterRange::new`]'s validation. Routing through this struct and
/// [`ParameterRange::try_from`] instead forces every deserialized value through the same
/// checks as programmatic construction.
#[derive(Deserialize)]
struct RawParameterRange {
    kind: ParameterKind,
    min: f64,
    max: f64,
    step: Option<f64>,
    default: f64,
}

impl TryFrom<RawParameterRange> for ParameterRange {
    type Error = EvalError;

    fn try_from(raw: RawParameterRange) -> Result<Self, Self::Error> {
        Self::new(raw.kind, raw.min, raw.max, raw.step, raw.default)
    }
}

impl ParameterRange {
    /// Fallback number of grid divisions used by [`effective_step`] when a parameter has
    /// no discrete `step` configured. Gives a reasonable granularity for continuous
    /// parameters without requiring an explicit step in the search space definition.
    ///
    /// [`effective_step`]: Self::effective_step
    const DEFAULT_STEP_DIVISIONS: f64 = 20.0;

    /// Construct a validated `ParameterRange`.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::InvalidRange`] if `min >= max` or either bound is non-finite.
    /// Returns [`EvalError::DefaultOutOfRange`] if `default` is outside `[min, max]`.
    ///
    /// `step` is not validated by this constructor; non-positive or non-finite values
    /// are treated as `None` by [`step_count`] and [`quantize`].
    ///
    /// [`step_count`]: Self::step_count
    /// [`quantize`]: Self::quantize
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::{ParameterRange, ParameterKind, EvalError};
    ///
    /// let r = ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, Some(0.1), 0.7).unwrap();
    /// assert!((r.min() - 0.0).abs() < f64::EPSILON);
    /// assert!((r.max() - 1.0).abs() < f64::EPSILON);
    /// assert!((r.default_value() - 0.7).abs() < f64::EPSILON);
    ///
    /// assert!(matches!(
    ///     ParameterRange::new(ParameterKind::Temperature, 1.0, 0.0, None, 0.5),
    ///     Err(EvalError::InvalidRange { .. })
    /// ));
    /// assert!(matches!(
    ///     ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, None, 2.0),
    ///     Err(EvalError::DefaultOutOfRange { .. })
    /// ));
    /// ```
    pub fn new(
        kind: ParameterKind,
        min: f64,
        max: f64,
        step: Option<f64>,
        default: f64,
    ) -> Result<Self, EvalError> {
        if !min.is_finite() || !max.is_finite() || min >= max {
            return Err(EvalError::InvalidRange { min, max });
        }
        if !default.is_finite() || default < min || default > max {
            return Err(EvalError::DefaultOutOfRange { default, min, max });
        }
        Ok(Self {
            kind,
            min,
            max,
            step,
            default,
        })
    }

    /// Return the [`ParameterKind`] this range applies to.
    #[must_use]
    pub fn kind(&self) -> ParameterKind {
        self.kind
    }

    /// Return the minimum value (inclusive).
    #[must_use]
    pub fn min(&self) -> f64 {
        self.min
    }

    /// Return the maximum value (inclusive).
    #[must_use]
    pub fn max(&self) -> f64 {
        self.max
    }

    /// Return the discrete step size, or `None` for a continuous range.
    #[must_use]
    pub fn step(&self) -> Option<f64> {
        self.step
    }

    /// Return the configured step, or a fallback of `(max - min) / 20` for continuous ranges.
    ///
    /// Generator strategies ([`GridStep`], [`Neighborhood`]) call this as the single source
    /// of truth for the default granularity applied when a parameter has no explicit `step`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::{ParameterRange, ParameterKind};
    ///
    /// let r = ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, Some(0.1), 0.7).unwrap();
    /// assert!((r.effective_step() - 0.1).abs() < f64::EPSILON);
    ///
    /// let r_continuous = ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, None, 0.5).unwrap();
    /// assert!((r_continuous.effective_step() - 0.05).abs() < f64::EPSILON);
    /// ```
    ///
    /// [`GridStep`]: crate::GridStep
    /// [`Neighborhood`]: crate::Neighborhood
    #[must_use]
    pub fn effective_step(&self) -> f64 {
        self.step
            .unwrap_or_else(|| (self.max - self.min) / Self::DEFAULT_STEP_DIVISIONS)
    }

    /// Return the default (baseline) value.
    ///
    /// Named `default_value` to avoid shadowing the `Default` trait keyword.
    #[must_use]
    pub fn default_value(&self) -> f64 {
        self.default
    }

    /// Number of discrete grid points in this range, or `None` if `step` is not set or ≤ 0.
    ///
    /// The count is `floor((max - min) / step) + 1`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::{ParameterRange, ParameterKind};
    ///
    /// let r = ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, Some(0.5), 0.5).unwrap();
    /// assert_eq!(r.step_count(), Some(3)); // 0.0, 0.5, 1.0
    ///
    /// let r_continuous = ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, None, 0.5).unwrap();
    /// assert_eq!(r_continuous.step_count(), None);
    /// ```
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
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::{ParameterRange, ParameterKind};
    ///
    /// let r = ParameterRange::new(ParameterKind::TopP, 0.1, 1.0, Some(0.1), 0.9).unwrap();
    /// assert!((r.clamp(2.0) - 1.0).abs() < f64::EPSILON);
    /// assert!((r.clamp(-1.0) - 0.1).abs() < f64::EPSILON);
    /// ```
    #[must_use]
    pub fn clamp(&self, value: f64) -> f64 {
        value.clamp(self.min, self.max)
    }

    /// Return `true` if `value` lies within `[min, max]` (inclusive).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::{ParameterRange, ParameterKind};
    ///
    /// let r = ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, Some(0.1), 0.7).unwrap();
    /// assert!(r.contains(0.5));
    /// assert!(!r.contains(1.1));
    /// ```
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
            return self.clamp((quantized * 100.0).round() / 100.0);
        }
        value
    }
}

/// The set of parameter ranges that define the experiment search space.
///
/// The default search space covers five parameters: `temperature`, `top_p`, `top_k`,
/// `frequency_penalty`, and `presence_penalty`. Custom spaces can be constructed
/// by providing any subset of [`ParameterRange`] values.
///
/// When deserialized from config with `[serde(default)]`, missing fields are filled
/// from [`Default::default`].
///
/// # Examples
///
/// ```rust
/// use zeph_experiments::{SearchSpace, ParameterKind};
///
/// let space = SearchSpace::default();
/// assert!(space.grid_size() > 0);
/// assert!(space.range_for(ParameterKind::Temperature).is_some());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchSpace {
    /// The parameter ranges in this search space.
    pub parameters: Vec<ParameterRange>,
}

impl Default for SearchSpace {
    fn default() -> Self {
        Self {
            parameters: vec![
                ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, Some(0.1), 0.7)
                    .expect("default Temperature range is valid"),
                ParameterRange::new(ParameterKind::TopP, 0.1, 1.0, Some(0.05), 0.9)
                    .expect("default TopP range is valid"),
                ParameterRange::new(ParameterKind::TopK, 1.0, 100.0, Some(5.0), 40.0)
                    .expect("default TopK range is valid"),
                ParameterRange::new(ParameterKind::FrequencyPenalty, -2.0, 2.0, Some(0.2), 0.0)
                    .expect("default FrequencyPenalty range is valid"),
                ParameterRange::new(ParameterKind::PresencePenalty, -2.0, 2.0, Some(0.2), 0.0)
                    .expect("default PresencePenalty range is valid"),
            ],
        }
    }
}

impl SearchSpace {
    /// Find the range for a given [`ParameterKind`], if present.
    ///
    /// Returns `None` if the search space does not include the requested kind.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::{SearchSpace, ParameterKind};
    ///
    /// let space = SearchSpace::default();
    /// let temp = space.range_for(ParameterKind::Temperature).unwrap();
    /// assert!((temp.default_value() - 0.7).abs() < f64::EPSILON);
    ///
    /// // RetrievalTopK is not in the default space
    /// assert!(space.range_for(ParameterKind::RetrievalTopK).is_none());
    /// ```
    #[must_use]
    pub fn range_for(&self, kind: ParameterKind) -> Option<&ParameterRange> {
        self.parameters.iter().find(|r| r.kind() == kind)
    }

    /// Total number of discrete grid points across all parameters that have a step.
    ///
    /// This equals the number of distinct variations a [`GridStep`] generator will
    /// produce before returning `None`. Parameters without a `step` are not counted.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::SearchSpace;
    ///
    /// let size = SearchSpace::default().grid_size();
    /// assert!(size > 0);
    ///
    /// assert_eq!(SearchSpace { parameters: vec![] }.grid_size(), 0);
    /// ```
    ///
    /// [`GridStep`]: crate::GridStep
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
    use std::assert_matches;

    fn make_range(
        kind: ParameterKind,
        min: f64,
        max: f64,
        step: Option<f64>,
        default: f64,
    ) -> ParameterRange {
        ParameterRange::new(kind, min, max, step, default).unwrap()
    }

    #[test]
    fn new_valid_range() {
        let r = make_range(ParameterKind::Temperature, 0.0, 1.0, Some(0.5), 0.5);
        assert_eq!(r.kind(), ParameterKind::Temperature);
        assert!((r.min() - 0.0).abs() < f64::EPSILON);
        assert!((r.max() - 1.0).abs() < f64::EPSILON);
        assert!((r.default_value() - 0.5).abs() < f64::EPSILON);
        assert_eq!(r.step(), Some(0.5));
    }

    #[test]
    fn new_invalid_range_min_ge_max() {
        assert_matches!(
            ParameterRange::new(ParameterKind::Temperature, 1.0, 0.0, None, 0.5),
            Err(EvalError::InvalidRange { .. })
        );
        // equal bounds also invalid
        assert_matches!(
            ParameterRange::new(ParameterKind::Temperature, 0.5, 0.5, None, 0.5),
            Err(EvalError::InvalidRange { .. })
        );
    }

    #[test]
    fn new_invalid_range_nonfinite_bounds() {
        assert_matches!(
            ParameterRange::new(ParameterKind::Temperature, f64::NAN, 1.0, None, 0.5),
            Err(EvalError::InvalidRange { .. })
        );
        assert_matches!(
            ParameterRange::new(ParameterKind::Temperature, 0.0, f64::INFINITY, None, 0.5),
            Err(EvalError::InvalidRange { .. })
        );
    }

    #[test]
    fn new_invalid_default_out_of_range() {
        assert_matches!(
            ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, None, 2.0),
            Err(EvalError::DefaultOutOfRange { .. })
        );
        assert_matches!(
            ParameterRange::new(ParameterKind::Temperature, 0.0, 1.0, None, -0.1),
            Err(EvalError::DefaultOutOfRange { .. })
        );
    }

    #[test]
    fn step_count_with_step() {
        let r = make_range(ParameterKind::Temperature, 0.0, 1.0, Some(0.5), 0.5);
        assert_eq!(r.step_count(), Some(3)); // 0.0, 0.5, 1.0
    }

    #[test]
    fn step_count_no_step() {
        let r = make_range(ParameterKind::Temperature, 0.0, 1.0, None, 0.5);
        assert_eq!(r.step_count(), None);
    }

    #[test]
    fn step_count_zero_step() {
        // step=Some(0.0) passes construction (step not validated), but step_count returns None
        let mut r = make_range(ParameterKind::Temperature, 0.0, 1.0, None, 0.5);
        r.step = Some(0.0);
        assert_eq!(r.step_count(), None);
    }

    #[test]
    fn clamp_below_min() {
        let r = make_range(ParameterKind::TopP, 0.1, 1.0, Some(0.1), 0.9);
        assert!((r.clamp(-1.0) - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_above_max() {
        let r = make_range(ParameterKind::TopP, 0.1, 1.0, Some(0.1), 0.9);
        assert!((r.clamp(2.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_within_range() {
        let r = make_range(ParameterKind::Temperature, 0.0, 2.0, Some(0.1), 0.7);
        assert!((r.clamp(1.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn contains_within_range() {
        let r = make_range(ParameterKind::Temperature, 0.0, 2.0, Some(0.1), 0.7);
        assert!(r.contains(1.0));
        assert!(r.contains(0.0));
        assert!(r.contains(2.0));
        assert!(!r.contains(-0.1));
        assert!(!r.contains(2.1));
    }

    #[test]
    fn quantize_snaps_to_nearest_step() {
        let r = make_range(ParameterKind::Temperature, 0.0, 2.0, Some(0.1), 0.7);
        let q = r.quantize(0.73);
        assert!((q - 0.7).abs() < 1e-10, "expected 0.7, got {q}");
    }

    #[test]
    fn quantize_no_step_returns_value_unchanged() {
        let r = make_range(ParameterKind::Temperature, 0.0, 2.0, None, 0.7);
        assert!((r.quantize(1.234) - 1.234).abs() < f64::EPSILON);
    }

    #[test]
    fn quantize_clamps_result() {
        let r = make_range(ParameterKind::Temperature, 0.0, 1.0, Some(0.1), 0.5);
        let q = r.quantize(100.0);
        assert!(q <= 1.0, "quantize must clamp to max");
    }

    #[test]
    fn quantize_avoids_fp_accumulation() {
        let r = make_range(ParameterKind::Temperature, 0.0, 2.0, Some(0.1), 0.7);
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
        // Temperature: 11, TopP: 19, TopK: 20, Freq: 21, Pres: 21 = 92
        assert!(size > 0);
        assert!(size < 200);
    }

    #[test]
    fn range_for_finds_temperature() {
        let space = SearchSpace::default();
        let range = space.range_for(ParameterKind::Temperature);
        assert!(range.is_some());
        assert!((range.unwrap().default_value() - 0.7).abs() < f64::EPSILON);
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
        let r = make_range(ParameterKind::TopK, 1.0, 100.0, Some(5.0), 40.0);
        let q = r.quantize(6.0);
        assert!(
            (q - 6.0).abs() < 1e-10,
            "expected 6.0 (min-anchored grid), got {q}"
        );
        let q2 = r.quantize(3.0);
        assert!((q2 - 1.0).abs() < 1e-10, "expected 1.0, got {q2}");
    }

    #[test]
    fn quantize_negative_step_returns_unchanged() {
        let mut r = make_range(ParameterKind::Temperature, 0.0, 2.0, None, 0.7);
        r.step = Some(-0.1);
        assert!((r.quantize(0.75) - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn parameter_range_is_valid_for_default() {
        for r in &SearchSpace::default().parameters {
            // All ranges constructed via new() are valid by invariant
            assert!(
                r.min() < r.max(),
                "default range {:?} has min >= max",
                r.kind()
            );
        }
    }

    #[test]
    fn deserialize_rejects_inverted_range() {
        let json = r#"{"kind":"temperature","min":2.0,"max":0.0,"step":null,"default":1.0}"#;
        let result: Result<ParameterRange, _> = serde_json::from_str(json);
        assert!(result.is_err(), "min > max must fail deserialization");
    }

    #[test]
    fn deserialize_rejects_default_out_of_range() {
        let json = r#"{"kind":"temperature","min":0.0,"max":1.0,"step":null,"default":5.0}"#;
        let result: Result<ParameterRange, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "default outside [min, max] must fail deserialization"
        );
    }

    #[test]
    fn deserialize_rejects_nonfinite_bounds() {
        // JSON has no NaN/Infinity literal, so use TOML (which does) to exercise the
        // `is_finite()` check in `ParameterRange::new` via the deserialization path.
        let toml_src = "kind = \"temperature\"\nmin = nan\nmax = 1.0\nstep = 0.1\ndefault = 0.5\n";
        let result: Result<ParameterRange, _> = toml::from_str(toml_src);
        assert!(result.is_err(), "non-finite min must fail deserialization");
    }

    #[test]
    fn deserialize_accepts_valid_range() {
        let json = r#"{"kind":"temperature","min":0.0,"max":1.0,"step":0.1,"default":0.5}"#;
        let r: ParameterRange = serde_json::from_str(json).unwrap();
        assert!((r.min() - 0.0).abs() < f64::EPSILON);
        assert!((r.max() - 1.0).abs() < f64::EPSILON);
        assert!((r.default_value() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn deserialize_search_space_rejects_invalid_range() {
        let json = r#"{"parameters":[{"kind":"temperature","min":1.0,"max":0.0,"step":null,"default":0.5}]}"#;
        let result: Result<SearchSpace, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "SearchSpace must reject an inverted range in any member"
        );
    }

    #[test]
    fn roundtrip_serialize_deserialize_preserves_range() {
        let r = make_range(ParameterKind::TopP, 0.1, 1.0, Some(0.05), 0.9);
        let json = serde_json::to_string(&r).unwrap();
        let r2: ParameterRange = serde_json::from_str(&json).unwrap();
        assert_eq!(r.kind(), r2.kind());
        assert!((r.min() - r2.min()).abs() < f64::EPSILON);
        assert!((r.max() - r2.max()).abs() < f64::EPSILON);
        assert!((r.default_value() - r2.default_value()).abs() < f64::EPSILON);
    }
}
