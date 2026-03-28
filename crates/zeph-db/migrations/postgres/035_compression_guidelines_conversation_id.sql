-- Add optional conversation_id column for per-conversation scoping.
ALTER TABLE compression_guidelines
    ADD COLUMN conversation_id BIGINT REFERENCES conversations(id) ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS idx_compression_guidelines_conversation
    ON compression_guidelines(conversation_id);
