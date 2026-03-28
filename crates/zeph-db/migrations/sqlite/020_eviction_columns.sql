-- Add eviction tracking columns to the messages table.
-- deleted_at: soft-delete timestamp; NULL means the entry is active.
-- last_accessed: timestamp of the most recent read; NULL means never accessed after creation.
-- access_count: number of times this message has been retrieved.
-- qdrant_cleaned: 1 once the corresponding Qdrant vector has been removed after soft-delete.
--   Enables crash-safe Phase 2 recovery: on next startup/sweep, rows with deleted_at IS NOT NULL
--   and qdrant_cleaned = 0 are retried for Qdrant removal.
ALTER TABLE messages ADD COLUMN deleted_at TEXT DEFAULT NULL;
ALTER TABLE messages ADD COLUMN last_accessed TEXT DEFAULT NULL;
ALTER TABLE messages ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN qdrant_cleaned INTEGER NOT NULL DEFAULT 0;
