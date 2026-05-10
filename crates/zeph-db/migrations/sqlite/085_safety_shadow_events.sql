-- Migration: 085_safety_shadow_events.sql
-- Persistent safety memory stream for ShadowSentinel Phase 2 (spec 050).
-- Stores all safety-relevant tool events across sessions for cross-session
-- pattern detection and LLM probe context assembly.
CREATE TABLE IF NOT EXISTS safety_shadow_events (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL,
    turn_number     INTEGER NOT NULL,
    event_type      TEXT NOT NULL,     -- 'tool_call', 'tool_result', 'risk_signal', 'probe_result'
    tool_id         TEXT,              -- qualified tool id (nullable for non-tool events)
    risk_signal     TEXT,              -- serialized RiskSignal variant (nullable)
    risk_level      TEXT NOT NULL,     -- 'calm', 'elevated', 'high', 'critical'
    probe_verdict   TEXT,              -- 'allow', 'deny', 'skip' (nullable, set for probe_result)
    context_summary TEXT,              -- short text summary for LLM probe context
    created_at      INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Session replay: retrieve full trajectory ordered by time
CREATE INDEX IF NOT EXISTS idx_shadow_events_session
    ON safety_shadow_events(session_id, created_at ASC);

-- Cross-session pattern detection: find similar tool sequences across sessions
CREATE INDEX IF NOT EXISTS idx_shadow_events_tool
    ON safety_shadow_events(tool_id, created_at DESC)
    WHERE tool_id IS NOT NULL;

-- Probe audit: find all probe verdicts for a session efficiently
CREATE INDEX IF NOT EXISTS idx_shadow_events_probe
    ON safety_shadow_events(session_id, event_type)
    WHERE event_type = 'probe_result';
