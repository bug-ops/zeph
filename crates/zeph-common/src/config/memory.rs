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

/// Runtime config for Hebbian background consolidation (HL-F3/F4, #3345).
///
/// Passed to `spawn_consolidation_loop` after being constructed from `HebbianConfig` in the
/// top-level runner.
#[derive(Debug, Clone)]
pub struct HebbianConsolidationConfig {
    /// How often the consolidation sweep runs, in seconds. `0` disables the loop.
    pub consolidation_interval_secs: u64,
    /// Minimum `degree × avg_weight` score for a candidate to qualify.
    pub consolidation_threshold: f64,
    /// Maximum candidates processed per sweep.
    pub max_candidates_per_sweep: usize,
    /// Re-consolidation cooldown per entity, in seconds.
    pub consolidation_cooldown_secs: u64,
    /// LLM prompt timeout per distillation call, in seconds.
    pub consolidation_prompt_timeout_secs: u64,
    /// Maximum neighbour summaries passed to the LLM per candidate.
    pub consolidation_max_neighbors: usize,
}

impl Default for HebbianConsolidationConfig {
    fn default() -> Self {
        Self {
            consolidation_interval_secs: 3600,
            consolidation_threshold: 5.0,
            max_candidates_per_sweep: 10,
            consolidation_cooldown_secs: 86_400,
            consolidation_prompt_timeout_secs: 30,
            consolidation_max_neighbors: 20,
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

/// Runtime config for the forgetting sweep (#2397).
#[derive(Debug, Clone)]
pub struct ForgettingConfig {
    /// Enable the forgetting sweep.
    pub enabled: bool,
    /// Per-sweep decay rate applied to importance scores. Range: (0.0, 1.0).
    pub decay_rate: f32,
    /// Importance floor below which memories are pruned. Range: [0.0, 1.0].
    pub forgetting_floor: f32,
    /// How often the forgetting sweep runs, in seconds.
    pub sweep_interval_secs: u64,
    /// Maximum messages to process per sweep.
    pub sweep_batch_size: usize,
    /// Hours: messages accessed within this window get replay protection.
    pub replay_window_hours: u32,
    /// Messages with `access_count` >= this get replay protection.
    pub replay_min_access_count: u32,
    /// Hours: never prune messages accessed within this window.
    pub protect_recent_hours: u32,
    /// Never prune messages with `access_count` >= this.
    pub protect_min_access_count: u32,
}

impl Default for ForgettingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            decay_rate: 0.1,
            forgetting_floor: 0.05,
            sweep_interval_secs: 7200,
            sweep_batch_size: 500,
            replay_window_hours: 24,
            replay_min_access_count: 3,
            protect_recent_hours: 24,
            protect_min_access_count: 3,
        }
    }
}
