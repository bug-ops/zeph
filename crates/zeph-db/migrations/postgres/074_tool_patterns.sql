-- Migration 074: PASTE (Pattern-Aware Speculative Tool Execution) tables.
--
-- tool_pattern_transitions: one row per observed (skill, prev_tool, next_tool, args_fingerprint)
-- tuple. Updated in-place with exponential decay on each observation.
--
-- tool_pattern_predictions: materialized top-K predictions per (skill, prev_tool) pair.
-- Rebuilt lazily by PatternStore::refresh(); stale rows vacuumed after 30 days.

CREATE TABLE IF NOT EXISTS tool_pattern_transitions (
    id                     BIGSERIAL PRIMARY KEY,
    skill_name             TEXT    NOT NULL,
    skill_hash             TEXT    NOT NULL,
    prev_tool              TEXT,
    next_tool              TEXT    NOT NULL,
    args_fingerprint       TEXT    NOT NULL,
    args_template          TEXT    NOT NULL DEFAULT '{}',
    count_raw              BIGINT  NOT NULL DEFAULT 1,
    success_raw            BIGINT  NOT NULL DEFAULT 0,
    count_decayed          DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    last_seen_at           BIGINT  NOT NULL,
    avg_latency_ms         BIGINT  NOT NULL DEFAULT 0,
    UNIQUE(skill_name, skill_hash, prev_tool, next_tool, args_fingerprint)
);

CREATE INDEX IF NOT EXISTS idx_tool_pattern_skill_prev
    ON tool_pattern_transitions(skill_name, skill_hash, prev_tool);

CREATE TABLE IF NOT EXISTS tool_pattern_predictions (
    skill_name             TEXT    NOT NULL,
    skill_hash             TEXT    NOT NULL,
    prev_tool              TEXT,
    next_tool              TEXT    NOT NULL,
    args_fingerprint       TEXT    NOT NULL,
    args_template          TEXT    NOT NULL,
    score                  DOUBLE PRECISION NOT NULL,
    wilson_lower_bound     DOUBLE PRECISION NOT NULL,
    rank                   BIGINT  NOT NULL,
    PRIMARY KEY (skill_name, skill_hash, prev_tool, rank)
);
