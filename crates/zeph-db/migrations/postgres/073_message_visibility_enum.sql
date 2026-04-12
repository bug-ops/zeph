-- Migrate messages.agent_visible + messages.user_visible to messages.visibility TEXT.
-- Maps:
--   (true, true)   -> 'both'
--   (true, false)  -> 'agent_only'
--   (false, true)  -> 'user_only'
--   (false, false) -> 'both' (invalid state; safest default)

ALTER TABLE messages ADD COLUMN visibility TEXT NOT NULL DEFAULT 'both';

UPDATE messages SET visibility =
    CASE
        WHEN agent_visible = TRUE AND user_visible = FALSE THEN 'agent_only'
        WHEN agent_visible = FALSE AND user_visible = TRUE THEN 'user_only'
        ELSE 'both'
    END;

ALTER TABLE messages DROP COLUMN agent_visible;
ALTER TABLE messages DROP COLUMN user_visible;
