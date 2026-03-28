-- Add fingerprint column to graph_communities for incremental community detection.
-- Existing rows get NULL (treated as "unknown", always re-summarized on next refresh).
ALTER TABLE graph_communities ADD COLUMN fingerprint TEXT;
