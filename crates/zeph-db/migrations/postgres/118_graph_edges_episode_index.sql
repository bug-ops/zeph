-- Index-backed lookup for GraphStore::entity_ids_for_messages (#7012): it filters
-- graph_edges by `episode_id IN (...) AND valid_to IS NULL`. The column name is tracked by #7015.
CREATE INDEX IF NOT EXISTS idx_graph_edges_episode_active ON graph_edges(episode_id) WHERE valid_to IS NULL AND episode_id IS NOT NULL;
