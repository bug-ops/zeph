-- Epoch-based Qdrant invalidation for multi-session consistency (#2478).
-- When a session modifies a graph entity, it increments the epoch so stale
-- Qdrant embeddings can be detected and filtered on read.
--
-- sqlx applies each migration exactly once (tracked by checksum in _sqlx_migrations),
-- so this ALTER TABLE is safe and idempotent within the sqlx migration lifecycle.
ALTER TABLE graph_entities ADD COLUMN embedding_epoch INTEGER NOT NULL DEFAULT 0;
