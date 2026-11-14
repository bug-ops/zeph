// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Validated non-negative float for unbounded-above configuration fields.
//!
//! [`Fraction`](crate::Fraction) covers values inside the unit interval. Some thresholds, such as a
//! mean negative log-probability, are unbounded above; [`NonNegativeF64`] encodes "finite and
//! `>= 0`" in the type so readers never re-validate.
//!
//! Like `Fraction` it has no `Default`: each field supplies its own named constant.

use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// A value rejected by [`NonNegativeF64::new`] or by deserialization.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("value {value} must be a finite number >= 0.0")]
pub struct NonNegativeF64Error {
    /// The rejected value.
    pub value: f64,
}

/// A finite `f64` guaranteed to be `>= 0.0`.
///
/// Construct with [`NonNegativeF64::new`] (runtime values), [`NonNegativeF64::literal`]
/// (compile-time constants) or by deserializing; read with [`NonNegativeF64::get`].
///
/// # Examples
///
/// ```
/// use zeph_config::NonNegativeF64;
///
/// assert_eq!(NonNegativeF64::new(2.5).unwrap().get(), 2.5);
/// assert!(NonNegativeF64::new(-0.1).is_err());
/// assert!(NonNegativeF64::new(f64::NAN).is_err());
///
/// const THRESHOLD: NonNegativeF64 = NonNegativeF64::literal(0.8);
/// assert_eq!(THRESHOLD.get(), 0.8);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct NonNegativeF64(f64);

impl NonNegativeF64 {
    /// Wraps `value` when it is finite and `>= 0.0`.
    ///
    /// # Errors
    ///
    /// Returns [`NonNegativeF64Error`] for `NaN`, infinite or negative input.
    pub fn new(value: f64) -> Result<Self, NonNegativeF64Error> {
        if value.is_finite() && value >= 0.0 {
            Ok(Self(value))
        } else {
            Err(NonNegativeF64Error { value })
        }
    }

    /// Compile-time constructor for named constants; an invalid literal fails the build when
    /// evaluated in a `const` item.
    ///
    /// # Panics
    ///
    /// Panics (at compile time in `const` contexts) when `value` is `NaN`, infinite or negative.
    #[must_use]
    pub const fn literal(value: f64) -> Self {
        assert!(
            value.is_finite() && value >= 0.0,
            "non-negative literal out of range"
        );
        Self(value)
    }

    /// The wrapped value, guaranteed finite and `>= 0.0`.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl std::fmt::Display for NonNegativeF64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl Serialize for NonNegativeF64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for NonNegativeF64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(f64::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl schemars::JsonSchema for NonNegativeF64 {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("NonNegativeF64")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = schemars::json_schema!({ "type": "number" });
        schema.insert("minimum".to_owned(), 0.0.into());
        schema
    }
}

#[cfg(test)]
mod tests {
    use schemars::JsonSchema as _;

    use super::*;

    #[derive(Deserialize)]
    struct Wrap {
        v: NonNegativeF64,
    }

    fn parse(v: &str) -> Result<f64, toml::de::Error> {
        toml::from_str::<Wrap>(&format!("v = {v}")).map(|w| w.v.get())
    }

    #[test]
    fn accepts_zero_and_values_above_one() {
        assert!(parse("0.0").is_ok());
        assert!(parse("3.5").is_ok());
    }

    #[test]
    fn rejects_negative_and_non_finite() {
        for bad in ["-0.1", "nan", "inf", "-inf"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn round_trips_through_json() {
        let v = NonNegativeF64::new(2.25).unwrap();
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "2.25");
        assert_eq!(serde_json::from_str::<NonNegativeF64>(&json).unwrap(), v);
    }

    #[test]
    fn schema_has_minimum_only() {
        let mut generator = schemars::SchemaGenerator::default();
        let schema = serde_json::to_value(NonNegativeF64::json_schema(&mut generator)).unwrap();
        assert_eq!(schema["type"], "number");
        assert_eq!(schema["minimum"], 0.0);
        assert!(schema.get("maximum").is_none());
    }
}
