-- Add optional conversation_id column for per-conversation scoping.
-- Existing rows get NULL (global scope) — no NOT NULL constraint.
ALTER TABLE compression_guidelines
    ADD COLUMN conversation_id INTEGER REFERENCES conversations(id) ON DELETE CASCADE;

-- Index for efficient per-conversation lookups with global fallback.
CREATE INDEX IF NOT EXISTS idx_compression_guidelines_conversation
    ON compression_guidelines(conversation_id);
