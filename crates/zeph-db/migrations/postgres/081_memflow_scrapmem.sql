-- Migration: 081_memflow_scrapmem.sql
-- MemFlow tiered retrieval (issue #3712) and ScrapMem optical forgetting + EM-Graph (issue #3713).

-- ScrapMem optical forgetting: progressive content-fidelity decay on stored messages.
ALTER TABLE messages ADD COLUMN IF NOT EXISTS content_fidelity TEXT NOT NULL DEFAULT 'Full';
ALTER TABLE messages ADD COLUMN IF NOT EXISTS compressed_content TEXT;

-- Episodic Memory Graph (EM-Graph): causal-temporal event extraction.
-- No CASCADE: messages are never deleted (spec 001-6).
CREATE TABLE IF NOT EXISTS episodic_events (
    id          BIGSERIAL PRIMARY KEY,
    session_id  TEXT NOT NULL,
    message_id  BIGINT NOT NULL REFERENCES messages(id),
    event_type  TEXT NOT NULL,
    summary     TEXT NOT NULL,
    embedding   BYTEA,
    created_at  BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM NOW())::BIGINT
);

CREATE TABLE IF NOT EXISTS causal_links (
    id              BIGSERIAL PRIMARY KEY,
    cause_event_id  BIGINT NOT NULL REFERENCES episodic_events(id),
    effect_event_id BIGINT NOT NULL REFERENCES episodic_events(id),
    strength        REAL NOT NULL DEFAULT 1.0,
    created_at      BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM NOW())::BIGINT,
    UNIQUE(cause_event_id, effect_event_id)
);

CREATE INDEX IF NOT EXISTS idx_episodic_events_session ON episodic_events(session_id);
CREATE INDEX IF NOT EXISTS idx_episodic_events_message ON episodic_events(message_id);
CREATE INDEX IF NOT EXISTS idx_causal_links_cause ON causal_links(cause_event_id);
CREATE INDEX IF NOT EXISTS idx_causal_links_effect ON causal_links(effect_event_id);
