-- Migration 027: Partial index on graph_edges(expired_at) for eviction query.
-- Accelerates delete_expired_edges which filters WHERE expired_at IS NOT NULL AND expired_at < datetime(...).
-- Only invalidated edges have expired_at set (a small fraction of total edges), so the partial index
-- stays compact and does not inflate write overhead for active edges.
CREATE INDEX IF NOT EXISTS idx_graph_edges_expired
    ON graph_edges(expired_at)
    WHERE expired_at IS NOT NULL;
