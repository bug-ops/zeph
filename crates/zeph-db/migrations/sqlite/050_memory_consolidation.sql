-- Migration 049: All-Mem lifelong memory consolidation (#2270)
--
-- Adds consolidation metadata to messages and a join table linking consolidated
-- entries back to their source originals. Originals are never deleted — they are
-- marked with consolidated=1 and deprioritized in recall over time via temporal decay.

-- Add consolidation metadata to the messages table.
-- consolidated: 0 = original message, 1 = this row is a consolidation product.
ALTER TABLE messages ADD COLUMN consolidated INTEGER NOT NULL DEFAULT 0;
-- NULL for original messages; [0.0, 1.0] confidence for consolidation products.
ALTER TABLE messages ADD COLUMN consolidation_confidence REAL;

-- Join table: maps each consolidated message to the source originals it was derived from.
-- Enables O(1) reverse lookup: "which consolidated entry covers original X?"
CREATE TABLE IF NOT EXISTS memory_consolidation_sources (
    consolidated_id INTEGER NOT NULL REFERENCES messages(id),
    source_id       INTEGER NOT NULL REFERENCES messages(id),
    PRIMARY KEY (consolidated_id, source_id)
);

-- Reverse lookup index: find consolidated entries that reference a given original.
CREATE INDEX IF NOT EXISTS idx_consolidation_sources_source
  ON memory_consolidation_sources (source_id);

-- Sweep index: find non-consolidated originals for the consolidation sweep.
CREATE INDEX IF NOT EXISTS idx_messages_consolidation
  ON messages (conversation_id, consolidated) WHERE consolidated = 0;
