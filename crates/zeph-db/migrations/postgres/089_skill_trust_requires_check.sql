-- Migration 089: Add requires_trust_check flag to skill_trust table.
-- Controls per-invocation blake3 re-hash before skill dispatch (#4293).
ALTER TABLE skill_trust
    ADD COLUMN IF NOT EXISTS requires_trust_check INTEGER NOT NULL DEFAULT 0;
