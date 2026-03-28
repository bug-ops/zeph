-- Migration 041: MAGMA multi-graph memory — add edge_type column to graph_edges.
--
-- All existing edges receive edge_type='semantic' via the DEFAULT clause.
-- SQLite ALTER TABLE ADD COLUMN with a constant DEFAULT is O(1) — no table rewrite.

ALTER TABLE graph_edges ADD COLUMN edge_type TEXT NOT NULL DEFAULT 'semantic'
    CHECK(edge_type IN ('semantic', 'temporal', 'causal', 'entity'));

-- Replace the active-edge uniqueness constraint to include edge_type.
-- The same (source, target, relation) pair can now coexist as Semantic and Causal edges.
DROP INDEX IF EXISTS uq_graph_edges_active;

CREATE UNIQUE INDEX IF NOT EXISTS uq_graph_edges_active
    ON graph_edges(source_entity_id, target_entity_id, relation, edge_type)
    WHERE valid_to IS NULL;

CREATE INDEX IF NOT EXISTS idx_graph_edges_type
    ON graph_edges(edge_type);

-- Partial index covering the BFS hot path: active edges filtered by type.
CREATE INDEX IF NOT EXISTS idx_graph_edges_type_valid
    ON graph_edges(edge_type, valid_to) WHERE valid_to IS NULL;
