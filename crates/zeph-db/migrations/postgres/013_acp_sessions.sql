CREATE TABLE IF NOT EXISTS acp_sessions (
    id         TEXT PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS acp_session_events (
    id         BIGSERIAL PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES acp_sessions(id) ON DELETE CASCADE,
    event_type TEXT NOT NULL,
    payload    TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_acp_session_events_session ON acp_session_events(session_id, id);
