-- Migration 050: All-Mem lifelong memory consolidation.
-- consolidated: FALSE = original message, TRUE = this row is a consolidation product.
ALTER TABLE messages ADD COLUMN consolidated BOOLEAN NOT NULL DEFAULT FALSE;
-- NULL for original messages; [0.0, 1.0] confidence for consolidation products.
ALTER TABLE messages ADD COLUMN consolidation_confidence DOUBLE PRECISION;

-- Join table: maps each consolidated message to the source originals it was derived from.
CREATE TABLE IF NOT EXISTS memory_consolidation_sources (
    consolidated_id BIGINT NOT NULL REFERENCES messages(id),
    source_id       BIGINT NOT NULL REFERENCES messages(id),
    PRIMARY KEY (consolidated_id, source_id)
);

-- Reverse lookup index.
CREATE INDEX IF NOT EXISTS idx_consolidation_sources_source
  ON memory_consolidation_sources (source_id);

-- Sweep index: find non-consolidated originals for the consolidation sweep.
CREATE INDEX IF NOT EXISTS idx_messages_consolidation
  ON messages (conversation_id, consolidated) WHERE consolidated = FALSE;
