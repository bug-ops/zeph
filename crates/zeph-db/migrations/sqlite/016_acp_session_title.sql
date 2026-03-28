-- Add optional title column for ACP sessions (populated after first agent response).
ALTER TABLE acp_sessions ADD COLUMN title TEXT;
