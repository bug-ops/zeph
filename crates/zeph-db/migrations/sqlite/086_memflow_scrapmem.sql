-- Migration: 086_memflow_scrapmem.sql
-- MemFlow tiered retrieval (issue #3712) and ScrapMem optical forgetting + EM-Graph (issue #3713).

-- ScrapMem optical forgetting: progressive content-fidelity decay on stored messages.
ALTER TABLE messages ADD COLUMN content_fidelity TEXT NOT NULL DEFAULT 'Full';
ALTER TABLE messages ADD COLUMN compressed_content TEXT;

-- Episodic Memory Graph (EM-Graph): causal-temporal event extraction.
CREATE TABLE IF NOT EXISTS episodic_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    -- No CASCADE: messages are never deleted (spec 001-6); events outlive content compression.
    message_id  INTEGER NOT NULL REFERENCES messages(id),
    event_type  TEXT NOT NULL,
    summary     TEXT NOT NULL,
    embedding   BLOB,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS causal_links (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    cause_event_id  INTEGER NOT NULL REFERENCES episodic_events(id),
    effect_event_id INTEGER NOT NULL REFERENCES episodic_events(id),
    strength        REAL NOT NULL DEFAULT 1.0,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(cause_event_id, effect_event_id)
);

CREATE INDEX IF NOT EXISTS idx_episodic_events_session ON episodic_events(session_id);
CREATE INDEX IF NOT EXISTS idx_episodic_events_message ON episodic_events(message_id);
CREATE INDEX IF NOT EXISTS idx_causal_links_cause ON causal_links(cause_event_id);
CREATE INDEX IF NOT EXISTS idx_causal_links_effect ON causal_links(effect_event_id);
