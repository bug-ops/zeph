// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Memory subsystem configuration types.
//!
//! Split from the former 4800-line `memory.rs` monolith into focused submodules,
//! one per memory concern. All public types are re-exported here so the historical
//! `zeph_config::memory::*` paths remain stable:
//!
//! - `root` — root [`MemoryConfig`] and [`VectorBackend`]
//! - `graph` — knowledge-graph extraction, activation, and quality gating
//! - `hebbian` — synaptic plasticity and reward-prediction-error gating
//! - `retrieval` — hybrid retrieval, admission control, and store routing
//! - `fidelity` — compression, forgetting, and eviction
//! - `consolidation` — episodic and five-signal consolidation daemons
//! - `session` — session, document, and semantic-memory config
//! - `persona` — persona inference and trajectory risk accumulation
//! - `reasoning` — memory-augmented reasoning, probes, and `MemCoT`

mod consolidation;
mod fidelity;
mod graph;
mod hebbian;
mod persona;
mod reasoning;
mod retrieval;
mod root;
mod session;
#[cfg(test)]
mod tests;

pub use consolidation::*;
pub use fidelity::*;
pub use graph::*;
pub use hebbian::*;
pub use persona::*;
pub use reasoning::*;
pub use retrieval::*;
pub use root::*;
pub use session::*;

// Shared helpers referenced by serde attributes in more than one submodule.
pub(crate) fn validate_similarity_threshold<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "similarity_threshold must be a finite number",
        ));
    }
    if !(0.0..=1.0).contains(&value) {
        return Err(serde::de::Error::custom(
            "similarity_threshold must be in [0.0, 1.0]",
        ));
    }
    Ok(value)
}

pub(crate) fn default_embed_timeout_secs() -> u64 {
    5
}
