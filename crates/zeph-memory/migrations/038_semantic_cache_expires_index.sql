-- Drop the old partial index that lacks expires_at coverage.
DROP INDEX IF EXISTS idx_response_cache_semantic;

-- Recreate with expires_at included so get_semantic() can filter expired rows
-- within the index scan instead of post-filtering on the heap.
-- Column order: embedding_model (equality), expires_at (range), embedding_ts (sort).
CREATE INDEX IF NOT EXISTS idx_response_cache_semantic
  ON response_cache(embedding_model, expires_at, embedding_ts DESC)
  WHERE embedding IS NOT NULL;
