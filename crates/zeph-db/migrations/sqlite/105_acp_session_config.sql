-- Snapshot of per-session config fields (#5373), persisted on graceful `session/close` so a
-- later `session/resume` or `session/fork` of a session no longer resident in memory can inherit
-- its last-known values instead of resetting to configured defaults. NULL means no snapshot
-- exists yet (session was never closed gracefully, or predates this migration) — callers fall
-- back to config defaults in that case.
ALTER TABLE acp_sessions ADD COLUMN current_model TEXT;
ALTER TABLE acp_sessions ADD COLUMN temperature_preset TEXT;
ALTER TABLE acp_sessions ADD COLUMN thinking_enabled INTEGER;
ALTER TABLE acp_sessions ADD COLUMN auto_approve_level TEXT;
