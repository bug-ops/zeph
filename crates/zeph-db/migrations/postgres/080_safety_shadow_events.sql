-- Migration: 080_safety_shadow_events.sql
-- Persistent safety memory stream for ShadowSentinel Phase 2 (spec 050).
-- Stores all safety-relevant tool events across sessions for cross-session
-- pattern detection and LLM probe context assembly.
CREATE TABLE IF NOT EXISTS safety_shadow_events (
    id              BIGSERIAL PRIMARY KEY,
    session_id      TEXT NOT NULL,
    turn_number     BIGINT NOT NULL,
    event_type      TEXT NOT NULL,
    tool_id         TEXT,
    risk_signal     TEXT,
    risk_level      TEXT NOT NULL,
    probe_verdict   TEXT,
    context_summary TEXT,
    created_at      BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM NOW())::BIGINT
);

-- Session replay: retrieve full trajectory ordered by time
CREATE INDEX IF NOT EXISTS idx_shadow_events_session
    ON safety_shadow_events(session_id, created_at);

-- Cross-session pattern detection: find similar tool sequences across sessions
CREATE INDEX IF NOT EXISTS idx_shadow_events_tool
    ON safety_shadow_events(tool_id, created_at DESC)
    WHERE tool_id IS NOT NULL;

-- Probe audit: find all probe verdicts for a session efficiently
CREATE INDEX IF NOT EXISTS idx_shadow_events_probe
    ON safety_shadow_events(session_id, event_type)
    WHERE event_type = 'probe_result';
