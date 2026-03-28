ALTER TABLE messages ADD COLUMN agent_visible BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE messages ADD COLUMN user_visible BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE messages ADD COLUMN compacted_at TIMESTAMPTZ DEFAULT NULL;
CREATE INDEX IF NOT EXISTS idx_messages_conversation_id ON messages(conversation_id);
