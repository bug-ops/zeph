// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod frames;
pub mod gaia;
pub mod locomo;

pub use frames::{FramesEvaluator, FramesLoader};
pub use gaia::{GaiaEvaluator, GaiaLoader};
pub use locomo::{LocomoEvaluator, LocomoLoader};
