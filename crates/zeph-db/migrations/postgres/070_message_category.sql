-- Category-aware memory (#2428).
-- Adds nullable category column to messages for skill/tool-based auto-tagging.
ALTER TABLE messages ADD COLUMN IF NOT EXISTS category TEXT;

CREATE INDEX IF NOT EXISTS idx_messages_category ON messages(category)
    WHERE category IS NOT NULL;
