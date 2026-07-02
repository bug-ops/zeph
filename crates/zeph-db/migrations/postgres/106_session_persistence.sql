-- Promotes `acp_sessions` (migration 013) from ACP-only to the channel-agnostic
-- conversation-session index backing the JSONL event log (spec-068 §5.1, #5343). Non-ACP
-- channels (CLI, TUI, Telegram) mint an ACP-style SessionId and share this same table
-- (Decision D1 — no new `sessions` table).
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS last_seq            BIGINT NOT NULL DEFAULT 0;
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS event_count         BIGINT NOT NULL DEFAULT 0;
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS forked_from         TEXT;
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS forked_at_seq       BIGINT;
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS status              TEXT NOT NULL DEFAULT 'idle'
                                              CHECK(status IN ('active', 'idle', 'archived'));
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS last_condensed_seq  BIGINT NOT NULL DEFAULT 0;

-- Enforces the SessionId <-> ConversationId bijection (spec §5.2). Permits multiple NULLs
-- (legacy rows without a conversation link, or rows never associated with one).
CREATE UNIQUE INDEX IF NOT EXISTS idx_acp_sessions_conversation_id
    ON acp_sessions(conversation_id)
    WHERE conversation_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_acp_sessions_status      ON acp_sessions(status);
CREATE INDEX IF NOT EXISTS idx_acp_sessions_updated     ON acp_sessions(updated_at);
CREATE INDEX IF NOT EXISTS idx_acp_sessions_forked_from ON acp_sessions(forked_from);
