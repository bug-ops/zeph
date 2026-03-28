-- Partial indexes for temporal range queries on graph edges.

CREATE INDEX IF NOT EXISTS idx_graph_edges_src_temporal
    ON graph_edges(source_entity_id, valid_from)
    WHERE valid_to IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_graph_edges_tgt_temporal
    ON graph_edges(target_entity_id, valid_from)
    WHERE valid_to IS NOT NULL;
