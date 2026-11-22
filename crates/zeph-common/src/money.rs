// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unit-bearing money newtypes: [`Cents`] for amounts and [`CentsPer1k`] for per-1000-token rates.

use std::fmt;
use std::ops::{Add, AddAssign};

use serde::{Deserialize, Serialize};

/// An amount of money in US cents.
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cents(f64);

impl Cents {
    /// Zero cents.
    pub const ZERO: Self = Self(0.0);

    /// Wraps a raw cent amount.
    #[must_use]
    pub fn new(_cents: f64) -> Self {
        todo!("#7176")
    }

    /// Returns the raw cent amount.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        todo!("#7176")
    }

    /// Returns the amount in US dollars.
    #[must_use]
    pub fn usd(self) -> f64 {
        todo!("#7176")
    }

    /// Subtracts `rhs`, flooring the result at zero.
    #[must_use]
    pub fn saturating_sub(self, _rhs: Self) -> Self {
        todo!("#7176")
    }
}

impl Add for Cents {
    type Output = Self;

    fn add(self, _rhs: Self) -> Self {
        todo!("#7176")
    }
}

impl AddAssign for Cents {
    fn add_assign(&mut self, _rhs: Self) {
        todo!("#7176")
    }
}

impl fmt::Display for Cents {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        todo!("#7176")
    }
}

/// A price in US cents per 1000 tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CentsPer1k(f64);

impl CentsPer1k {
    /// Wraps a raw per-1000-token rate.
    #[must_use]
    pub fn new(_rate: f64) -> Self {
        todo!("#7176")
    }

    /// Returns the raw rate.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        todo!("#7176")
    }

    /// Returns the cost of `tokens` tokens at this rate.
    #[must_use]
    pub fn cost(self, _tokens: u64) -> Cents {
        todo!("#7176")
    }
}
