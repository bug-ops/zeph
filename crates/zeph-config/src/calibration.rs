// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Validated calibration curve for LLM self-reported confidence.

use serde::{Deserialize, Serialize};

/// Reason a calibration curve was rejected; every variant carries the offending point index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CalibrationError {
    /// A curve needs at least two points (or none for identity).
    #[error("calibration curve needs at least 2 points, got {0}")]
    TooFewPoints(usize),
    /// A coordinate is NaN, infinite, or outside `0.0..=1.0`.
    #[error("calibration point {0} has a coordinate outside 0.0..=1.0 or non-finite")]
    OutOfRange(usize),
    /// The first point must have `raw == 0.0`.
    #[error("calibration curve must start at raw = 0.0")]
    MissingStart,
    /// The last point must have `raw == 1.0`.
    #[error("calibration curve must end at raw = 1.0")]
    MissingEnd,
    /// Raw values must be strictly increasing.
    #[error("calibration point {0}: raw values must be strictly increasing")]
    RawNotIncreasing(usize),
    /// Calibrated values must be non-decreasing so thresholding stays order-preserving.
    #[error("calibration point {0}: calibrated values must be non-decreasing")]
    CalibratedDecreasing(usize),
}

/// Reason a correction-confidence threshold was rejected.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("confidence threshold {0} must be finite and within 0.0..=1.0")]
pub struct ConfidenceThresholdError(f32);

/// Minimum calibrated confidence for an LLM-reported correction to be accepted.
///
/// The value is checked at construction and at deserialization, so a `nan` or out-of-range
/// `correction_confidence_threshold` fails config loading instead of silently disabling
/// (`nan`) or suppressing (`> 1.0`) correction detection.
///
/// # Examples
///
/// ```
/// use zeph_config::ConfidenceThreshold;
///
/// assert_eq!(ConfidenceThreshold::new(0.6).unwrap().get(), 0.6);
/// assert!(ConfidenceThreshold::new(f32::NAN).is_err());
/// assert!(ConfidenceThreshold::new(1.5).is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f32", into = "f32")]
pub struct ConfidenceThreshold(f32);

impl ConfidenceThreshold {
    /// Build a threshold from a value in `0.0..=1.0`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidenceThresholdError`] for NaN, infinite, or out-of-range input.
    pub fn new(value: f32) -> Result<Self, ConfidenceThresholdError> {
        if (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ConfidenceThresholdError(value))
        }
    }

    /// Compile-time constructor for literal defaults; panics (at compile time in `const`
    /// contexts) when `value` is NaN or outside `0.0..=1.0`.
    pub(crate) const fn literal(value: f32) -> Self {
        assert!(
            value >= 0.0 && value <= 1.0,
            "threshold must be within 0.0..=1.0"
        );
        Self(value)
    }

    /// The raw threshold in `0.0..=1.0`.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

impl Default for ConfidenceThreshold {
    fn default() -> Self {
        Self::literal(0.6)
    }
}

impl TryFrom<f32> for ConfidenceThreshold {
    type Error = ConfidenceThresholdError;

    fn try_from(value: f32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ConfidenceThreshold> for f32 {
    fn from(t: ConfidenceThreshold) -> Self {
        t.0
    }
}

/// Monotone piecewise-linear map from a model's self-reported confidence to a calibrated one.
///
/// The curve is a list of `[raw, calibrated]` knots. An empty curve is the identity. Invariants
/// are checked at construction and at deserialization, so an invalid `[skills.learning]`
/// `correction_confidence_calibration` fails config loading instead of skewing the gate.
///
/// # Examples
///
/// ```
/// use zeph_config::ConfidenceCalibration;
///
/// let curve = ConfidenceCalibration::new(vec![[0.0, 0.0], [0.8, 0.4], [1.0, 1.0]]).unwrap();
/// assert!((curve.calibrate(0.8) - 0.4).abs() < 1e-6);
/// assert!((curve.calibrate(0.4) - 0.2).abs() < 1e-6);
/// assert_eq!(ConfidenceCalibration::identity().calibrate(0.7), 0.7);
/// assert!(ConfidenceCalibration::new(vec![[0.0, 0.0]]).is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(try_from = "Vec<[f32; 2]>", into = "Vec<[f32; 2]>")]
pub struct ConfidenceCalibration(Vec<[f32; 2]>);

impl ConfidenceCalibration {
    /// The identity curve: calibrated confidence equals the clamped raw confidence.
    #[must_use]
    pub const fn identity() -> Self {
        Self(Vec::new())
    }

    /// Build a curve from `[raw, calibrated]` knots; an empty list is the identity.
    ///
    /// # Errors
    ///
    /// Returns [`CalibrationError`] when the knots violate an invariant: fewer than two points,
    /// a coordinate outside `0.0..=1.0`, first raw not `0.0`, last raw not `1.0`, raw not strictly
    /// increasing, or calibrated decreasing.
    #[allow(clippy::float_cmp)] // endpoints must be exactly 0.0 and 1.0
    pub fn new(points: Vec<[f32; 2]>) -> Result<Self, CalibrationError> {
        if points.is_empty() {
            return Ok(Self::identity());
        }
        if points.len() < 2 {
            return Err(CalibrationError::TooFewPoints(points.len()));
        }
        if let Some(i) = points
            .iter()
            .position(|p| !p.iter().all(|v| (0.0..=1.0).contains(v)))
        {
            return Err(CalibrationError::OutOfRange(i));
        }
        if points[0][0] != 0.0 {
            return Err(CalibrationError::MissingStart);
        }
        if points[points.len() - 1][0] != 1.0 {
            return Err(CalibrationError::MissingEnd);
        }
        for (i, w) in points.windows(2).enumerate() {
            if w[1][0] <= w[0][0] {
                return Err(CalibrationError::RawNotIncreasing(i + 1));
            }
            if w[1][1] < w[0][1] {
                return Err(CalibrationError::CalibratedDecreasing(i + 1));
            }
        }
        Ok(Self(points))
    }

    /// `true` when the curve is the identity.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.0.is_empty()
    }

    /// Map a raw confidence to its calibrated value in `0.0..=1.0`.
    ///
    /// Non-finite input maps to `0.0` (so it never passes a threshold); finite input is clamped
    /// to `0.0..=1.0` first.
    #[must_use]
    pub fn calibrate(&self, raw: f32) -> f32 {
        if !raw.is_finite() {
            return 0.0;
        }
        let raw = raw.clamp(0.0, 1.0);
        if self.0.is_empty() {
            return raw;
        }
        let upper = self
            .0
            .partition_point(|p| p[0] <= raw)
            .min(self.0.len() - 1);
        let [x1, y1] = self.0[upper];
        let [x0, y0] = self.0[upper.saturating_sub(1)];
        // Strictly increasing raw values guarantee x1 > x0 once upper >= 1.
        y0 + (y1 - y0) * ((raw - x0) / (x1 - x0))
    }
}

impl std::str::FromStr for ConfidenceCalibration {
    type Err = String;

    /// Parse a TOML inline array such as `[[0.0, 0.0], [1.0, 1.0]]` through the same serde path
    /// as the config file; blank input is the identity.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        #[derive(Deserialize)]
        struct Wrap {
            curve: ConfidenceCalibration,
        }
        if s.trim().is_empty() {
            return Ok(Self::identity());
        }
        toml::from_str::<Wrap>(&format!("curve = {s}"))
            .map(|w| w.curve)
            .map_err(|e| e.message().to_owned())
    }
}

impl TryFrom<Vec<[f32; 2]>> for ConfidenceCalibration {
    type Error = CalibrationError;

    fn try_from(points: Vec<[f32; 2]>) -> Result<Self, Self::Error> {
        Self::new(points)
    }
}

impl From<ConfidenceCalibration> for Vec<[f32; 2]> {
    fn from(curve: ConfidenceCalibration) -> Self {
        curve.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
    }

    fn curve() -> ConfidenceCalibration {
        ConfidenceCalibration::new(vec![[0.0, 0.0], [0.8, 0.4], [1.0, 1.0]]).unwrap()
    }

    #[test]
    fn empty_is_identity() {
        let id = ConfidenceCalibration::new(vec![]).unwrap();
        assert!(id.is_identity());
        for raw in [0.0, 0.3, 0.6, 1.0] {
            assert_close(id.calibrate(raw), raw);
        }
    }

    #[test]
    fn rejects_each_invariant() {
        assert_eq!(
            ConfidenceCalibration::new(vec![[0.0, 0.0]]),
            Err(CalibrationError::TooFewPoints(1))
        );
        assert_eq!(
            ConfidenceCalibration::new(vec![[0.0, 0.0], [1.0, 1.5]]),
            Err(CalibrationError::OutOfRange(1))
        );
        assert_eq!(
            ConfidenceCalibration::new(vec![[0.0, f32::NAN], [1.0, 1.0]]),
            Err(CalibrationError::OutOfRange(0))
        );
        assert_eq!(
            ConfidenceCalibration::new(vec![[0.1, 0.0], [1.0, 1.0]]),
            Err(CalibrationError::MissingStart)
        );
        assert_eq!(
            ConfidenceCalibration::new(vec![[0.0, 0.0], [0.9, 1.0]]),
            Err(CalibrationError::MissingEnd)
        );
        assert_eq!(
            ConfidenceCalibration::new(vec![[0.0, 0.0], [0.5, 0.2], [0.5, 0.3], [1.0, 1.0]]),
            Err(CalibrationError::RawNotIncreasing(2))
        );
        assert_eq!(
            ConfidenceCalibration::new(vec![[0.0, 0.0], [0.5, 0.6], [1.0, 0.5]]),
            Err(CalibrationError::CalibratedDecreasing(2))
        );
    }

    #[test]
    fn errors_display_the_violated_rule() {
        assert_eq!(
            CalibrationError::RawNotIncreasing(2).to_string(),
            "calibration point 2: raw values must be strictly increasing"
        );
        assert!(
            CalibrationError::MissingStart
                .to_string()
                .contains("raw = 0.0")
        );
    }

    #[test]
    fn interpolates_and_hits_knots() {
        let c = curve();
        assert_close(c.calibrate(0.0), 0.0);
        assert!((c.calibrate(0.4) - 0.2).abs() < 1e-6);
        assert!((c.calibrate(0.8) - 0.4).abs() < 1e-6);
        assert!((c.calibrate(0.9) - 0.7).abs() < 1e-6);
        assert!((c.calibrate(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn clamps_and_maps_non_finite_to_zero() {
        let c = curve();
        assert_close(c.calibrate(-3.0), 0.0);
        assert!((c.calibrate(7.0) - 1.0).abs() < 1e-6);
        assert_close(c.calibrate(f32::NAN), 0.0);
        assert_close(c.calibrate(f32::INFINITY), 0.0);
        assert_close(ConfidenceCalibration::identity().calibrate(f32::NAN), 0.0);
    }

    #[test]
    fn calibration_is_monotone() {
        let c = curve();
        let mut prev = -1.0_f32;
        for i in 0..=100_u8 {
            let v = c.calibrate(f32::from(i) / 100.0);
            assert!(v >= prev, "not monotone at {i}");
            prev = v;
        }
    }

    #[test]
    fn toml_round_trip_and_load_error() {
        #[derive(Deserialize, Serialize)]
        struct Wrap {
            c: ConfidenceCalibration,
        }
        let w: Wrap = toml::from_str("c = [[0.0, 0.0], [0.8, 0.4], [1.0, 1.0]]").unwrap();
        assert_eq!(w.c, curve());
        let s = toml::to_string(&w).unwrap();
        let back: Wrap = toml::from_str(&s).unwrap();
        assert_eq!(back.c, curve());
        assert!(toml::from_str::<Wrap>("c = [[0.0, 0.0]]").is_err());
        let id: Wrap = toml::from_str("c = []").unwrap();
        assert!(id.c.is_identity());
    }

    #[test]
    fn from_str_parses_validates_and_blank_is_identity() {
        assert_eq!(
            "[[0.0, 0.0], [0.8, 0.4], [1.0, 1.0]]"
                .parse::<ConfidenceCalibration>()
                .unwrap(),
            curve()
        );
        assert!("  ".parse::<ConfidenceCalibration>().unwrap().is_identity());
        assert!("[[0.0, 0.0]]".parse::<ConfidenceCalibration>().is_err());
        assert!("nonsense".parse::<ConfidenceCalibration>().is_err());
    }

    #[test]
    fn threshold_accepts_bounds() {
        assert!(ConfidenceThreshold::new(0.0).unwrap().get().abs() < f32::EPSILON);
        assert!((ConfidenceThreshold::new(1.0).unwrap().get() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn threshold_rejects_invalid() {
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1, 1.5] {
            assert!(ConfidenceThreshold::new(v).is_err(), "{v}");
        }
    }

    #[test]
    fn threshold_deserialize_rejects_nan_and_out_of_range() {
        #[derive(Deserialize)]
        struct W {
            #[allow(dead_code)]
            t: ConfidenceThreshold,
        }
        assert!(toml::from_str::<W>("t = nan").is_err());
        assert!(toml::from_str::<W>("t = 1.5").is_err());
        assert!(toml::from_str::<W>("t = 0.7").is_ok());
    }
}
