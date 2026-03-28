-- Add fingerprint column to graph_communities for incremental community detection.
ALTER TABLE graph_communities ADD COLUMN fingerprint TEXT;
