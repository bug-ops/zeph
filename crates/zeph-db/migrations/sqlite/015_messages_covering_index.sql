-- Covering index for queries that filter by conversation_id and order/limit by id.
-- Replaces the single-column idx_messages_conversation_id for these access patterns.
CREATE INDEX IF NOT EXISTS idx_messages_conversation_id_id ON messages(conversation_id, id);
