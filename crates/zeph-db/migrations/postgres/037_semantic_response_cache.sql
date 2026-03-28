-- Extend existing response_cache table with embedding columns for semantic similarity search.
ALTER TABLE response_cache ADD COLUMN embedding BYTEA;
ALTER TABLE response_cache ADD COLUMN embedding_model TEXT;
ALTER TABLE response_cache ADD COLUMN embedding_ts BIGINT;

-- Partial index excludes rows without embeddings (exact-match only entries).
CREATE INDEX IF NOT EXISTS idx_response_cache_semantic
  ON response_cache(embedding_model, embedding_ts DESC)
  WHERE embedding IS NOT NULL;
