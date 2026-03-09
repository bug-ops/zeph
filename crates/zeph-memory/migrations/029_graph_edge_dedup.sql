-- Deduplicate active edges and add a partial unique index to prevent future duplicates.
--
-- Active edges share (source_entity_id, target_entity_id, relation, valid_to IS NULL).
-- Keep the row with the smallest id (earliest insertion) and remove the rest.

DELETE FROM graph_edges
WHERE valid_to IS NULL
  AND id NOT IN (
      SELECT MIN(id)
      FROM graph_edges
      WHERE valid_to IS NULL
      GROUP BY source_entity_id, target_entity_id, relation
  );

CREATE UNIQUE INDEX IF NOT EXISTS uq_graph_edges_active
    ON graph_edges(source_entity_id, target_entity_id, relation)
    WHERE valid_to IS NULL;
