-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- ReasoningBank: distilled reasoning strategy memory (#3342).
-- Stores per-agent generalizable reasoning-strategy summaries extracted from
-- completed turns via a self-judge + distillation pipeline.
CREATE TABLE IF NOT EXISTS reasoning_strategies (
    id           TEXT    PRIMARY KEY NOT NULL,
    summary      TEXT    NOT NULL,
    outcome      TEXT    NOT NULL,
    task_hint    TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    use_count    INTEGER NOT NULL DEFAULT 0,
    embedded_at  INTEGER             -- nullable: set when Qdrant upsert succeeds
);

-- LRU eviction scan: most-recently-used first, then by use_count
CREATE INDEX IF NOT EXISTS idx_reasoning_strategies_last_used_at
    ON reasoning_strategies (last_used_at);

-- Hot-row protection: filter by use_count when evicting
CREATE INDEX IF NOT EXISTS idx_reasoning_strategies_use_count
    ON reasoning_strategies (use_count);
