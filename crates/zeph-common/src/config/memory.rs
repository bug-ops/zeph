// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared runtime configuration structs for memory subsystems.
//!
//! These are plain (no serde) structs used as runtime parameters. They are separate from the
//! serde-annotated config types in `zeph-config` which own the deserialization concerns.

/// Runtime config for Kumiho belief revision passed into resolver methods.
#[derive(Debug, Clone)]
pub struct BeliefRevisionConfig {
    pub similarity_threshold: f32,
}

/// Runtime config for A-MEM dynamic note linking.
#[derive(Debug, Clone)]
pub struct NoteLinkingConfig {
    pub enabled: bool,
    pub similarity_threshold: f32,
    pub top_k: usize,
    pub timeout_secs: u64,
}

impl Default for NoteLinkingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            similarity_threshold: 0.85,
            top_k: 10,
            timeout_secs: 5,
        }
    }
}

/// Runtime config for the consolidation sweep loop.
#[derive(Debug, Clone)]
pub struct ConsolidationConfig {
    pub enabled: bool,
    pub confidence_threshold: f32,
    pub sweep_interval_secs: u64,
    pub sweep_batch_size: usize,
    pub similarity_threshold: f32,
}
