// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod store;
pub mod types;

pub mod activation;
pub mod belief;
pub mod belief_revision;
pub mod community;
pub mod conflict;
pub mod entity_lock;
pub mod experience;
pub mod extractor;
pub mod ontology;
pub mod resolver;
pub mod retrieval;
pub mod retrieval_astar;
pub mod retrieval_beam;
pub mod retrieval_watercircles;
pub mod rpe;
pub mod strategy_classifier;

pub use store::GraphStore;
pub use types::{Community, Edge, EdgeType, Entity, EntityAlias, EntityType, Episode, GraphFact};

pub use activation::{
    ActivatedFact, ActivatedNode, HelaFact, HelaSpreadParams, SpreadingActivation,
    SpreadingActivationParams, hela_spreading_recall,
};
pub use belief::{BeliefMemConfig, BeliefStore, PendingBelief, noisy_or, time_decayed_prob};
pub use belief_revision::{BeliefRevisionConfig, find_superseded_edges};
pub use community::{
    GraphEvictionStats, assign_to_community, cleanup_stale_entity_embeddings, detect_communities,
    run_graph_eviction,
};
pub use entity_lock::EntityLockManager;
pub use extractor::{ExtractedEdge, ExtractedEntity, ExtractionResult, GraphExtractor};
pub use resolver::{EntityResolver, ResolutionOutcome};
pub use retrieval::{graph_recall, graph_recall_activated};
pub use rpe::{RpeRouter, RpeSignal, extract_candidate_entities};
