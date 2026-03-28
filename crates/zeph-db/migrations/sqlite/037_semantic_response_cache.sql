-- Extend existing response_cache table with embedding columns for semantic similarity search.
ALTER TABLE response_cache ADD COLUMN embedding BLOB;
ALTER TABLE response_cache ADD COLUMN embedding_model TEXT;
ALTER TABLE response_cache ADD COLUMN embedding_ts INTEGER;

-- Index for efficient semantic search: filter by model, order by recency.
-- Partial index excludes rows without embeddings (exact-match only entries).
CREATE INDEX IF NOT EXISTS idx_response_cache_semantic
  ON response_cache(embedding_model, embedding_ts DESC)
  WHERE embedding IS NOT NULL;
