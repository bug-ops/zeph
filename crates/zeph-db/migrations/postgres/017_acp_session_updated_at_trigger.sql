-- Automatically update acp_sessions.updated_at whenever an event is inserted.
CREATE OR REPLACE FUNCTION trg_acp_session_updated_at_fn() RETURNS trigger AS $$
BEGIN
    UPDATE acp_sessions SET updated_at = NOW() WHERE id = NEW.session_id;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_acp_session_updated_at
    AFTER INSERT ON acp_session_events
    FOR EACH ROW EXECUTE FUNCTION trg_acp_session_updated_at_fn();
