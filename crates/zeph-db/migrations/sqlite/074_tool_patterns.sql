-- Migration 074: PASTE (Pattern-Aware Speculative Tool Execution) tables.
--
-- tool_pattern_transitions: one row per observed (skill, prev_tool, next_tool, args_fingerprint)
-- tuple. Updated in-place with exponential decay on each observation.
--
-- tool_pattern_predictions: materialized top-K predictions per (skill, prev_tool) pair.
-- Rebuilt lazily by PatternStore::refresh(); stale rows vacuumed after 30 days.

CREATE TABLE IF NOT EXISTS tool_pattern_transitions (
    id                     INTEGER PRIMARY KEY,
    skill_name             TEXT    NOT NULL,
    skill_hash             TEXT    NOT NULL,   -- BLAKE3 hex of SKILL.md content
    prev_tool              TEXT,               -- NULL means skill activation
    next_tool              TEXT    NOT NULL,
    args_fingerprint       TEXT    NOT NULL,   -- BLAKE3 hex over normalized args
    args_template          TEXT    NOT NULL DEFAULT '{}',  -- type-placeholder JSON template (H1)
    count_raw              INTEGER NOT NULL DEFAULT 1,
    success_raw            INTEGER NOT NULL DEFAULT 0,
    count_decayed          REAL    NOT NULL DEFAULT 1.0,
    last_seen_at           INTEGER NOT NULL,   -- unix epoch seconds
    avg_latency_ms         INTEGER NOT NULL DEFAULT 0,
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
    args_template          TEXT    NOT NULL,   -- JSON object template for prediction
    score                  REAL    NOT NULL,
    wilson_lower_bound     REAL    NOT NULL,
    rank                   INTEGER NOT NULL,
    PRIMARY KEY (skill_name, skill_hash, prev_tool, rank)
);
