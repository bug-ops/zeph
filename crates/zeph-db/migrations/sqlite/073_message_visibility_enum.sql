-- Migrate messages.agent_visible + messages.user_visible to messages.visibility TEXT.
-- Maps:
--   (1, 1) -> 'both'       (visible to agent and user)
--   (1, 0) -> 'agent_only' (visible to agent only)
--   (0, 1) -> 'user_only'  (visible to user only)
--   (0, 0) -> 'both'       (invalid state; safest default)

ALTER TABLE messages ADD COLUMN visibility TEXT NOT NULL DEFAULT 'both';

UPDATE messages SET visibility =
    CASE
        WHEN agent_visible = 1 AND user_visible = 0 THEN 'agent_only'
        WHEN agent_visible = 0 AND user_visible = 1 THEN 'user_only'
        ELSE 'both'
    END;

ALTER TABLE messages DROP COLUMN agent_visible;
ALTER TABLE messages DROP COLUMN user_visible;
