-- Migration 043: A-MEM evolving link weights — add retrieval tracking columns.
-- ALTER TABLE ADD COLUMN with DEFAULT is O(1) in SQLite (no table rewrite).

ALTER TABLE graph_edges ADD COLUMN retrieval_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE graph_edges ADD COLUMN last_retrieved_at INTEGER;

-- Index for periodic decay: find edges not retrieved since a cutoff timestamp.
CREATE INDEX IF NOT EXISTS idx_graph_edges_last_retrieved
    ON graph_edges(last_retrieved_at) WHERE valid_to IS NULL;
