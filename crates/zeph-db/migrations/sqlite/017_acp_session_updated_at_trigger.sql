-- Automatically update acp_sessions.updated_at whenever an event is inserted.
CREATE TRIGGER IF NOT EXISTS trg_acp_session_updated_at
AFTER INSERT ON acp_session_events
BEGIN
    UPDATE acp_sessions SET updated_at = datetime('now') WHERE id = NEW.session_id;
END;
