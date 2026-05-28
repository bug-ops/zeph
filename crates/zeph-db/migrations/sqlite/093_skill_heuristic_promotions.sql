-- AutoSkill A6 (spec 061): track evaluated heuristic batches per skill.
-- Prevents re-evaluating the same heuristic batch (idempotency via batch_hash).
CREATE TABLE IF NOT EXISTS skill_heuristic_promotions (
    skill_name         TEXT    NOT NULL,
    batch_hash         TEXT    NOT NULL,   -- BLAKE3 hex of sorted heuristic texts
    evaluated_at       INTEGER NOT NULL,   -- Unix timestamp (seconds)
    recommendation     TEXT    NOT NULL,   -- "body_enrichment" | "new_skill" | "none"
    draft_skill_name   TEXT,               -- NULL when recommendation = "none"
    PRIMARY KEY (skill_name, batch_hash)
);
