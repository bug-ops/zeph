ALTER TABLE messages ADD COLUMN agent_visible INTEGER NOT NULL DEFAULT 1;
ALTER TABLE messages ADD COLUMN user_visible INTEGER NOT NULL DEFAULT 1;
ALTER TABLE messages ADD COLUMN compacted_at TEXT DEFAULT NULL;
CREATE INDEX IF NOT EXISTS idx_messages_conversation_id ON messages(conversation_id);
