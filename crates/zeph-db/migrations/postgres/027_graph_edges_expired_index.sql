-- Partial index on graph_edges(expired_at) for eviction query.
CREATE INDEX IF NOT EXISTS idx_graph_edges_expired
    ON graph_edges(expired_at)
    WHERE expired_at IS NOT NULL;
