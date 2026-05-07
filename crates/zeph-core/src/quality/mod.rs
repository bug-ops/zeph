// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MARCH self-check quality pipeline.
//!
//! Provides post-response factual consistency checking via a two-stage
//! Proposer → Checker pipeline.
//!
//! See [`pipeline::SelfCheckPipeline`] for the entry point.

pub mod checker;
pub mod config;
pub mod parser;
pub mod pipeline;
pub mod prompts;
pub mod proposer;
pub mod types;

#[cfg(test)]
mod tests;

pub use config::QualityConfig;
pub use pipeline::{RetrievedContext, SelfCheckPipeline};
pub use types::SelfCheckReport;
