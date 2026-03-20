// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod store;
pub mod types;

pub mod community;
pub mod extractor;
pub mod resolver;
pub mod retrieval;

pub use store::GraphStore;
pub use types::{Community, Edge, EdgeType, Entity, EntityAlias, EntityType, GraphFact};

pub use community::{
    GraphEvictionStats, assign_to_community, cleanup_stale_entity_embeddings, detect_communities,
    run_graph_eviction,
};
pub use extractor::{ExtractedEdge, ExtractedEntity, ExtractionResult, GraphExtractor};
pub use resolver::{EntityResolver, ResolutionOutcome};
pub use retrieval::graph_recall;
