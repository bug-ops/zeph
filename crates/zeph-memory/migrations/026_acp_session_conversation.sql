-- Add per-session conversation mapping to ACP sessions table.
-- Existing sessions with conversation_id = NULL are handled gracefully:
-- get_acp_session_conversation_id returns None and load_session creates a new conversation.
ALTER TABLE acp_sessions ADD COLUMN conversation_id INTEGER REFERENCES conversations(id);
