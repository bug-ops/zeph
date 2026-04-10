// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Concrete [`DatasetLoader`] and [`Evaluator`] implementations for each built-in dataset.
//!
//! | Dataset | Loader | Evaluator | Scoring |
//! |---------|--------|-----------|---------|
//! | LOCOMO | [`LocomoLoader`] | [`LocomoEvaluator`] | Token F1, threshold 0.5 |
//! | FRAMES | [`FramesLoader`] | [`FramesEvaluator`] | Exact match (case-insensitive) |
//! | GAIA | [`GaiaLoader`] | [`GaiaEvaluator`] | GAIA-normalized exact match |
//!
//! [`DatasetLoader`]: crate::DatasetLoader
//! [`Evaluator`]: crate::Evaluator

pub mod frames;
pub mod gaia;
pub mod locomo;

pub use frames::{FramesEvaluator, FramesLoader};
pub use gaia::{GaiaEvaluator, GaiaLoader};
pub use locomo::{LocomoEvaluator, LocomoLoader};
