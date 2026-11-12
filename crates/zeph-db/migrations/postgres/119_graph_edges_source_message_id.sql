-- Make graph_edges.source_message_id (migration 060) the single provenance column (#7015).
-- Code now reads and writes it; `episode_id` stays as a dead column.
UPDATE graph_edges SET source_message_id = episode_id WHERE episode_id IS NOT NULL;
DROP INDEX IF EXISTS idx_graph_edges_episode_active;
CREATE INDEX IF NOT EXISTS idx_graph_edges_source_message_active ON graph_edges(source_message_id) WHERE valid_to IS NULL AND source_message_id IS NOT NULL;
