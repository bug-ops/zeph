-- Epoch-based Qdrant invalidation for multi-session consistency (#2478).
ALTER TABLE graph_entities ADD COLUMN IF NOT EXISTS embedding_epoch BIGINT NOT NULL DEFAULT 0;
