-- Partial indexes to accelerate temporal range queries on graph edges.
--
-- Temporal recall queries split into two UNIONed branches:
--   1. Active edges (valid_to IS NULL AND valid_from <= ?ts)
--   2. Historically valid edges (valid_to IS NOT NULL AND valid_from <= ?ts AND valid_to > ?ts)
--
-- Active edges are already covered by idx_graph_edges_valid (migration 021) and
-- uq_graph_edges_active (migration 029). These two partial indexes cover the
-- historical branch for each BFS direction (source → target, target → source).
--
-- Semantic contract:
--   valid_to  — end of the temporal validity window; NULL = open-ended [valid_from, +∞)
--   expired_at — administrative tombstone timestamp used for eviction/cleanup only
--   For superseded edges both are set to the same value (current behavior in invalidate_edge).

CREATE INDEX IF NOT EXISTS idx_graph_edges_src_temporal
    ON graph_edges(source_entity_id, valid_from)
    WHERE valid_to IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_graph_edges_tgt_temporal
    ON graph_edges(target_entity_id, valid_from)
    WHERE valid_to IS NOT NULL;
