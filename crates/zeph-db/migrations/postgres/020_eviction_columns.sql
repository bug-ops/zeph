-- Add eviction tracking columns to the messages table.
ALTER TABLE messages ADD COLUMN deleted_at TIMESTAMPTZ DEFAULT NULL;
ALTER TABLE messages ADD COLUMN last_accessed TIMESTAMPTZ DEFAULT NULL;
ALTER TABLE messages ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN qdrant_cleaned BOOLEAN NOT NULL DEFAULT FALSE;
