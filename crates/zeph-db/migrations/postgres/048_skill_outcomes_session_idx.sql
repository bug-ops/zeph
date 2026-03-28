CREATE INDEX IF NOT EXISTS idx_skill_outcomes_name_conv
    ON skill_outcomes (skill_name, conversation_id);
