// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared serde deserializers for unit-interval float configuration fields.
//!
//! Many configuration thresholds are constrained to the unit interval, either
//! the half-open `(0.0, 1.0]` or the fully closed `[0.0, 1.0]`. Both shapes
//! require the same guard: reject non-finite input (`NaN`, `±inf`) and reject
//! values outside the interval. Centralizing that guard here keeps individual
//! config structs free of copy-pasted validators — they reference
//! [`de_unit_open`] or [`de_unit_closed`] via `#[serde(deserialize_with = "...")]`.
//!
//! The helpers are generic over the float width through the crate-private
//! [`UnitFloat`] trait, so the same two functions serve both `f32` and `f64`
//! fields.

use serde::{Deserialize, Deserializer, de::Error as _};

/// Float types that can be range-checked against the unit interval.
///
/// Crate-private and implemented only for [`f32`] and [`f64`]. It exposes the
/// two interval bounds as associated constants plus a finiteness check so the
/// deserializers can stay width-agnostic.
pub(crate) trait UnitFloat: Copy + PartialOrd {
    /// The lower interval bound (`0.0`).
    const ZERO: Self;
    /// The upper interval bound (`1.0`).
    const ONE: Self;
    /// Returns `true` when the value is neither `NaN` nor infinite.
    fn is_finite(self) -> bool;
}

impl UnitFloat for f32 {
    const ZERO: Self = 0.0;
    const ONE: Self = 1.0;
    fn is_finite(self) -> bool {
        f32::is_finite(self)
    }
}

impl UnitFloat for f64 {
    const ZERO: Self = 0.0;
    const ONE: Self = 1.0;
    fn is_finite(self) -> bool {
        f64::is_finite(self)
    }
}

/// Deserialize a float and require it to lie in the half-open interval `(0.0, 1.0]`.
///
/// Rejects non-finite values and anything `<= 0.0` or `> 1.0`. Intended for
/// decay/threshold fields where a zero value is degenerate.
///
/// # Errors
///
/// Returns a deserialization error if the value is `NaN`, infinite, or outside
/// `(0.0, 1.0]`.
pub(crate) fn de_unit_open<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: UnitFloat + Deserialize<'de>,
{
    let value = T::deserialize(deserializer)?;
    if !value.is_finite() {
        return Err(D::Error::custom("value must be a finite number"));
    }
    if !(value > T::ZERO && value <= T::ONE) {
        return Err(D::Error::custom("value must be in (0.0, 1.0]"));
    }
    Ok(value)
}

/// Deserialize a float and require it to lie in the closed interval `[0.0, 1.0]`.
///
/// Rejects non-finite values and anything `< 0.0` or `> 1.0`. Intended for
/// threshold/margin fields where `0.0` is a valid boundary.
///
/// # Errors
///
/// Returns a deserialization error if the value is `NaN`, infinite, or outside
/// `[0.0, 1.0]`.
pub(crate) fn de_unit_closed<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: UnitFloat + Deserialize<'de>,
{
    let value = T::deserialize(deserializer)?;
    if !value.is_finite() {
        return Err(D::Error::custom("value must be a finite number"));
    }
    if !(value >= T::ZERO && value <= T::ONE) {
        return Err(D::Error::custom("value must be in [0.0, 1.0]"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct OpenF32 {
        #[serde(deserialize_with = "de_unit_open")]
        v: f32,
    }

    #[derive(serde::Deserialize)]
    struct OpenF64 {
        #[serde(deserialize_with = "de_unit_open")]
        v: f64,
    }

    #[derive(serde::Deserialize)]
    struct ClosedF32 {
        #[serde(deserialize_with = "de_unit_closed")]
        v: f32,
    }

    #[test]
    fn open_rejects_zero_accepts_one() {
        assert!(toml::from_str::<OpenF32>("v = 0.0").is_err());
        assert!(toml::from_str::<OpenF32>("v = 1.0").is_ok());
        assert!((toml::from_str::<OpenF32>("v = 0.5").unwrap().v - 0.5).abs() < 1e-6);
    }

    #[test]
    fn open_rejects_out_of_range_and_non_finite() {
        assert!(toml::from_str::<OpenF32>("v = 1.5").is_err());
        assert!(toml::from_str::<OpenF32>("v = -0.1").is_err());
        assert!(toml::from_str::<OpenF32>("v = nan").is_err());
        assert!(toml::from_str::<OpenF32>("v = inf").is_err());
    }

    #[test]
    fn closed_accepts_zero_and_one() {
        assert!(toml::from_str::<ClosedF32>("v = 0.0").unwrap().v.abs() < 1e-6);
        assert!((toml::from_str::<ClosedF32>("v = 1.0").unwrap().v - 1.0).abs() < 1e-6);
        assert!(toml::from_str::<ClosedF32>("v = 1.0001").is_err());
        assert!(toml::from_str::<ClosedF32>("v = -0.0001").is_err());
    }

    #[test]
    fn open_works_for_f64() {
        assert!(toml::from_str::<OpenF64>("v = 0.0").is_err());
        assert!((toml::from_str::<OpenF64>("v = 0.25").unwrap().v - 0.25).abs() < 1e-12);
    }
}
