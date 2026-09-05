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
//! - `store` — cross-thread key-value store (spec-080, #6363)
//! - `consent_gate` — write-time memory-consent gate (issue #6490)

mod consent_gate;
mod consolidation;
mod fidelity;
mod graph;
mod hebbian;
mod persona;
mod reasoning;
mod retrieval;
mod root;
mod session;
mod store;
#[cfg(test)]
mod tests;

pub use consent_gate::ConsentGateConfig;
pub use consolidation::{
    EpisodicConsolidationConfig, FiveSignalConfig, FiveSignalConsolidationConfig,
};
pub use fidelity::{
    AconConfig, ArcCompactionConfig, CompressionConfig, CompressionGuidelinesConfig,
    CompressionStrategy, DigestConfig, EvictionConfig, EvictionPolicy, ForgettingConfig,
    MicrocompactConfig, OpticalForgettingConfig, PruningStrategy, TypedPagesConfig,
    TypedPagesEnforcement,
};
pub use graph::{
    ApexMemConfig, BeamSearchConfig, ConflictResolutionStrategy, ConsolidationDaemonConfig,
    EmGraphConfig, ExperienceConfig, GraphConfig, GraphRetrievalStrategy, ImplicitConflictConfig,
    NoteLinkingConfig, SimilarityMethod, SpreadingActivationConfig, WaterCirclesConfig,
    WriteQualityGateConfig,
};
pub use hebbian::{
    BeliefRevisionConfig, ConflictRecencyConfig, HebbianConfig, RpeConfig, WriteGateConfig,
};
pub use persona::{
    CategoryConfig, PersonaConfig, SidequestConfig, TrajectoryConfig,
    TrajectoryRiskAccumulatorConfig, TrajectorySeverityMultipliers, TrajectorySignalWeights,
    TreeConfig,
};
pub use reasoning::{
    AutoDreamConfig, CompactionProbeConfig, MagicDocsConfig, MemCotConfig, ProbeCategory,
    ReasoningConfig, RecallViewConfig,
};
pub use retrieval::{
    AdmissionConfig, AdmissionStrategy, AdmissionWeights, ConsolidationConfig, ContextFormat,
    RetrievalConfig, RetrievalFailuresConfig, StoreRoutingConfig, StoreRoutingStrategy, TierConfig,
    TieredRetrievalConfig, TypeAwareComposeConfig,
};
pub use root::{MemoryConfig, VectorBackend};
pub use session::{ContextStrategy, DocumentConfig, SemanticConfig, SessionsConfig};
pub use store::CrossThreadStoreConfig;

pub(crate) fn default_embed_timeout_secs() -> u64 {
    5
}
