-- Remove any existing self-loop edges.
DELETE FROM graph_edges WHERE source_entity_id = target_entity_id;

-- Prevent future self-loop edges via CHECK constraint (simpler than trigger in PostgreSQL).
ALTER TABLE graph_edges ADD CONSTRAINT graph_edges_no_self_loops
    CHECK (source_entity_id <> target_entity_id);
