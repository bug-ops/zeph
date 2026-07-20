-- Write-time memory-consent gate (issue #6490, MemGhost).
-- Adds nullable source_kind/trust_level provenance columns to messages. NULL means
-- "written before this migration" (legacy row, provenance unknown) — new writes always
-- set an explicit value (see zeph-agent-persistence PersistenceService::persist_message).
ALTER TABLE messages ADD COLUMN IF NOT EXISTS source_kind TEXT;
ALTER TABLE messages ADD COLUMN IF NOT EXISTS trust_level TEXT;

CREATE INDEX IF NOT EXISTS idx_messages_trust_level ON messages(trust_level)
    WHERE trust_level IS NOT NULL;
