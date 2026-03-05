// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod store;
pub mod types;

#[cfg(feature = "graph-memory")]
pub mod extractor;
#[cfg(feature = "graph-memory")]
pub mod resolver;
#[cfg(feature = "graph-memory")]
pub mod retrieval;

pub use store::GraphStore;
pub use types::{Community, Edge, Entity, EntityType, GraphFact};

#[cfg(feature = "graph-memory")]
pub use extractor::{ExtractedEdge, ExtractedEntity, ExtractionResult, GraphExtractor};
#[cfg(feature = "graph-memory")]
pub use resolver::EntityResolver;
#[cfg(feature = "graph-memory")]
pub use retrieval::graph_recall;
