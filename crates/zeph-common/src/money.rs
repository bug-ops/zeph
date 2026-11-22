// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unit-bearing money newtypes: [`Cents`] for amounts and [`CentsPer1k`] for per-1000-token rates.

use std::fmt;
use std::ops::{Add, AddAssign};

use serde::{Deserialize, Serialize};

/// An amount of money in US cents.
///
/// Wraps an `f64` so cent amounts cannot be mixed up with dollars, token counts or rates.
/// Any finite or non-finite `f64` is accepted by [`Cents::new`]; no range is enforced.
/// `Display` prints dollars (`$0.0150` for 1.5 cents, four decimals unless a precision is given).
///
/// # Examples
///
/// ```
/// use zeph_common::money::Cents;
///
/// let total = Cents::new(1.5) + Cents::new(0.5);
/// assert_eq!(total, Cents::new(2.0));
/// assert_eq!(format!("{}", Cents::new(1.5)), "$0.0150");
/// assert_eq!(format!("{:.2}", Cents::new(150.0)), "$1.50");
/// assert_eq!(Cents::new(1.0).saturating_sub(Cents::new(3.0)), Cents::ZERO);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cents(f64);

impl Cents {
    /// Zero cents.
    pub const ZERO: Self = Self(0.0);

    /// Wraps a raw cent amount.
    #[must_use]
    pub const fn new(cents: f64) -> Self {
        Self(cents)
    }

    /// Returns the raw cent amount.
    #[must_use]
    pub const fn as_f64(self) -> f64 {
        self.0
    }

    /// Returns the amount in US dollars.
    #[must_use]
    pub fn usd(self) -> f64 {
        self.0 / 100.0
    }

    /// Subtracts `rhs`, flooring the result at zero.
    #[must_use]
    pub fn saturating_sub(self, rhs: Self) -> Self {
        Self((self.0 - rhs.0).max(0.0))
    }
}

impl Add for Cents {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign for Cents {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl fmt::Display for Cents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let precision = f.precision().unwrap_or(4);
        write!(f, "${:.precision$}", self.usd())
    }
}

/// A price in US cents per 1000 tokens.
///
/// # Examples
///
/// ```
/// use zeph_common::money::{Cents, CentsPer1k};
///
/// assert_eq!(CentsPer1k::new(2.0).cost(500), Cents::new(1.0));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CentsPer1k(f64);

impl CentsPer1k {
    /// Wraps a raw per-1000-token rate.
    #[must_use]
    pub const fn new(rate: f64) -> Self {
        Self(rate)
    }

    /// Returns the raw rate.
    #[must_use]
    pub const fn as_f64(self) -> f64 {
        self.0
    }

    /// Returns the cost of `tokens` tokens at this rate.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts stay far below 2^53; cost is approximate telemetry"
    )]
    pub fn cost(self, tokens: u64) -> Cents {
        Cents(self.0 * tokens as f64 / 1000.0)
    }
}
