-- Add per-session conversation mapping to ACP sessions table.
ALTER TABLE acp_sessions ADD COLUMN conversation_id BIGINT REFERENCES conversations(id);
