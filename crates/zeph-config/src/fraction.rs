// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Validated unit-interval floats for configuration fields.
//!
//! Many thresholds, rates and ratios are floats constrained to an interval inside `0..=1`, but
//! the interval ends differ field by field: `(0, 1]` for a decay, `[0, 1]` for a margin, `(0, 1)`
//! for a compaction watermark. [`Fraction`] encodes the interval in its type, so a value that is
//! out of range or non-finite cannot be deserialized or constructed, and readers receive the raw
//! float through [`Fraction::get`] without re-validating.
//!
//! The interval is a [`UnitBounds`] marker (`Closed`, `Open`, `LowerClosed`, `UpperClosed`) and
//! the float width is `f32` (default) or `f64`; the `Unit*` aliases name the combinations used by
//! the configuration structs.
//!
//! `Fraction` deliberately has no `Default`: a default is a property of the field, not of the
//! interval, so each field supplies its own `default = "fn"` returning a named constant.

use std::borrow::Cow;
use std::marker::PhantomData;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

mod sealed {
    pub trait Sealed {}
}

/// Float widths a [`Fraction`] can wrap: [`f32`] and [`f64`].
///
/// Sealed; it exposes only what interval checking and schema generation need.
pub trait UnitFloat:
    sealed::Sealed
    + Copy
    + PartialOrd
    + std::fmt::Debug
    + std::fmt::Display
    + std::str::FromStr<Err = std::num::ParseFloatError>
    + Serialize
    + for<'de> Deserialize<'de>
    + 'static
{
    /// The lower bound of the unit interval (`0.0`).
    const ZERO: Self;
    /// The upper bound of the unit interval (`1.0`).
    const ONE: Self;
    /// Returns `true` when the value is neither `NaN` nor infinite.
    fn is_finite(self) -> bool;
    /// The value widened to `f64`, for error messages.
    fn to_f64(self) -> f64;
}

macro_rules! impl_unit_float {
    ($t:ty) => {
        impl sealed::Sealed for $t {}
        impl UnitFloat for $t {
            const ZERO: Self = 0.0;
            const ONE: Self = 1.0;
            fn is_finite(self) -> bool {
                <$t>::is_finite(self)
            }
            fn to_f64(self) -> f64 {
                f64::from(self)
            }
        }
    };
}
impl_unit_float!(f32);
impl_unit_float!(f64);

/// Which ends of the unit interval a [`Fraction`] includes.
///
/// Sealed; implemented by [`Closed`], [`Open`], [`LowerClosed`] and [`UpperClosed`].
pub trait UnitBounds: sealed::Sealed + 'static {
    /// `true` when `0.0` is a valid value.
    const LOWER_INCLUSIVE: bool;
    /// `true` when `1.0` is a valid value.
    const UPPER_INCLUSIVE: bool;
    /// The interval in mathematical notation, for error messages.
    const INTERVAL: &'static str;
}

macro_rules! bounds_marker {
    ($(#[$doc:meta])* $name:ident, $lower:expr, $upper:expr, $interval:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd)]
        pub struct $name;
        impl sealed::Sealed for $name {}
        impl UnitBounds for $name {
            const LOWER_INCLUSIVE: bool = $lower;
            const UPPER_INCLUSIVE: bool = $upper;
            const INTERVAL: &'static str = $interval;
        }
    };
}

bounds_marker!(
    /// The closed interval `[0, 1]`.
    Closed,
    true,
    true,
    "[0.0, 1.0]"
);
bounds_marker!(
    /// The open interval `(0, 1)`.
    Open,
    false,
    false,
    "(0.0, 1.0)"
);
bounds_marker!(
    /// The interval `[0, 1)`: zero is valid, one is not.
    LowerClosed,
    true,
    false,
    "[0.0, 1.0)"
);
bounds_marker!(
    /// The interval `(0, 1]`: one is valid, zero is not.
    UpperClosed,
    false,
    true,
    "(0.0, 1.0]"
);

/// A value rejected by [`Fraction::new`] or by deserialization.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("value {value} must be a finite number in {interval}")]
pub struct FractionError {
    /// The rejected value, widened to `f64`.
    pub value: f64,
    /// The interval that was required, e.g. `(0.0, 1.0]`.
    pub interval: &'static str,
}

/// A string rejected by `Fraction::from_str`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ParseFractionError {
    /// The text is not a floating-point number.
    #[error("not a number: {0}")]
    NotANumber(#[from] std::num::ParseFloatError),
    /// The number is outside the interval.
    #[error(transparent)]
    OutOfRange(#[from] FractionError),
}

fn in_bounds<B: UnitBounds, F: UnitFloat>(value: F) -> bool {
    let lower = if B::LOWER_INCLUSIVE {
        value >= F::ZERO
    } else {
        value > F::ZERO
    };
    let upper = if B::UPPER_INCLUSIVE {
        value <= F::ONE
    } else {
        value < F::ONE
    };
    value.is_finite() && lower && upper
}

/// A finite float guaranteed to lie in the interval described by `B`.
///
/// Construct with [`Fraction::new`] (runtime values), [`Fraction::literal`] (compile-time
/// constants) or by deserializing; read with [`Fraction::get`].
///
/// # Examples
///
/// ```
/// use zeph_config::{UnitClosed, UnitUpperClosed};
///
/// assert_eq!(UnitClosed::new(0.0).unwrap().get(), 0.0);
/// assert!(UnitUpperClosed::new(0.0).is_err());
/// assert!(UnitClosed::new(f32::NAN).is_err());
///
/// const HALF: UnitClosed = UnitClosed::literal(0.5);
/// assert_eq!(HALF.get(), 0.5);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Fraction<B: UnitBounds, F: UnitFloat = f32> {
    value: F,
    bounds: PhantomData<B>,
}

impl<B: UnitBounds, F: UnitFloat> Fraction<B, F> {
    /// Wraps `value` when it is finite and inside the interval.
    ///
    /// # Errors
    ///
    /// Returns [`FractionError`] for `NaN`, infinite or out-of-interval input.
    pub fn new(value: F) -> Result<Self, FractionError> {
        if in_bounds::<B, F>(value) {
            Ok(Self {
                value,
                bounds: PhantomData,
            })
        } else {
            Err(FractionError {
                value: value.to_f64(),
                interval: B::INTERVAL,
            })
        }
    }

    /// The wrapped value, guaranteed finite and inside the interval.
    #[must_use]
    pub const fn get(self) -> F {
        self.value
    }
}

impl<B: UnitBounds, F: UnitFloat> std::fmt::Display for Fraction<B, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.value, f)
    }
}

impl<B: UnitBounds, F: UnitFloat> std::str::FromStr for Fraction<B, F> {
    type Err = ParseFractionError;

    /// Parses decimal text and validates it, so interactive prompts can use a `Fraction` as the
    /// input type and get range checking for free.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s.trim().parse::<F>()?)?)
    }
}

macro_rules! impl_literal {
    ($t:ty) => {
        impl<B: UnitBounds> Fraction<B, $t> {
            /// Compile-time constructor for named constants; an invalid literal fails the build
            /// when evaluated in a `const` item.
            ///
            /// # Panics
            ///
            /// Panics (at compile time in `const` contexts) when `value` is `NaN` or outside the
            /// interval.
            #[must_use]
            pub const fn literal(value: $t) -> Self {
                let lower = if B::LOWER_INCLUSIVE {
                    value >= 0.0
                } else {
                    value > 0.0
                };
                let upper = if B::UPPER_INCLUSIVE {
                    value <= 1.0
                } else {
                    value < 1.0
                };
                assert!(lower && upper, "fraction literal outside its interval");
                Self {
                    value,
                    bounds: PhantomData,
                }
            }
        }
    };
}
impl_literal!(f32);
impl_literal!(f64);

impl<B: UnitBounds, F: UnitFloat> Serialize for Fraction<B, F> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de, B: UnitBounds, F: UnitFloat> Deserialize<'de> for Fraction<B, F> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(F::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl<B: UnitBounds, F: UnitFloat> schemars::JsonSchema for Fraction<B, F> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Fraction")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let lower_key = if B::LOWER_INCLUSIVE {
            "minimum"
        } else {
            "exclusiveMinimum"
        };
        let upper_key = if B::UPPER_INCLUSIVE {
            "maximum"
        } else {
            "exclusiveMaximum"
        };
        let mut schema = schemars::json_schema!({ "type": "number" });
        schema.insert(lower_key.to_owned(), 0.0.into());
        schema.insert(upper_key.to_owned(), 1.0.into());
        schema
    }
}

/// A float in `[0, 1]`.
pub type UnitClosed = Fraction<Closed>;
/// A float in `(0, 1)`.
pub type UnitOpen = Fraction<Open>;
/// A float in `[0, 1)`.
pub type UnitLowerClosed = Fraction<LowerClosed>;
/// A float in `(0, 1]`.
pub type UnitUpperClosed = Fraction<UpperClosed>;
/// An `f64` in `[0, 1]`.
pub type UnitClosed64 = Fraction<Closed, f64>;
/// An `f64` in `(0, 1]`.
pub type UnitUpperClosed64 = Fraction<UpperClosed, f64>;

#[cfg(test)]
mod tests {
    use schemars::JsonSchema as _;

    use super::*;

    #[derive(Deserialize)]
    #[serde(bound = "")]
    struct Wrap<B: UnitBounds> {
        v: Fraction<B>,
    }

    fn parse<B: UnitBounds>(v: &str) -> Result<f32, toml::de::Error> {
        toml::from_str::<Wrap<B>>(&format!("v = {v}")).map(|w| w.v.get())
    }

    #[test]
    fn bounds_match_their_notation() {
        assert!(parse::<Closed>("0.0").is_ok() && parse::<Closed>("1.0").is_ok());
        assert!(parse::<Open>("0.0").is_err() && parse::<Open>("1.0").is_err());
        assert!(parse::<Open>("0.5").is_ok());
        assert!(parse::<LowerClosed>("0.0").is_ok() && parse::<LowerClosed>("1.0").is_err());
        assert!(parse::<UpperClosed>("0.0").is_err() && parse::<UpperClosed>("1.0").is_ok());
    }

    #[test]
    fn rejects_out_of_range_and_non_finite() {
        for bad in ["1.5", "-0.1", "nan", "inf", "-inf"] {
            assert!(parse::<Closed>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn f64_width_round_trips() {
        let f = UnitClosed64::new(0.25).unwrap();
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, "0.25");
        assert_eq!(serde_json::from_str::<UnitClosed64>(&json).unwrap(), f);
    }

    #[test]
    fn error_names_the_interval() {
        let err = UnitUpperClosed::new(0.0).unwrap_err();
        assert!(err.to_string().contains("(0.0, 1.0]"), "{err}");
    }

    #[test]
    fn schema_carries_inclusive_and_exclusive_bounds() {
        let mut generator = schemars::SchemaGenerator::default();
        let closed = serde_json::to_value(UnitClosed::json_schema(&mut generator)).unwrap();
        assert_eq!(closed["minimum"], 0.0);
        assert_eq!(closed["maximum"], 1.0);
        let open = serde_json::to_value(UnitOpen::json_schema(&mut generator)).unwrap();
        assert_eq!(open["exclusiveMinimum"], 0.0);
        assert_eq!(open["exclusiveMaximum"], 1.0);
        assert_eq!(open["type"], "number");
    }

    #[test]
    fn from_str_parses_and_validates() {
        assert_eq!("0.25".parse::<UnitOpen>().unwrap().get(), 0.25);
        assert!(matches!(
            "1.0".parse::<UnitOpen>(),
            Err(ParseFractionError::OutOfRange(_))
        ));
        assert!(matches!(
            "abc".parse::<UnitOpen>(),
            Err(ParseFractionError::NotANumber(_))
        ));
        assert_eq!(UnitOpen::literal(0.5).to_string(), "0.5");
    }

    #[test]
    fn literal_in_const_context() {
        const V: UnitUpperClosed = UnitUpperClosed::literal(1.0);
        assert_eq!(V.get(), 1.0);
    }
}
