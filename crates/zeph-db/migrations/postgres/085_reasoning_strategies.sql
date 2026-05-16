-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Migration 085: ReasoningBank distilled strategy memory (PostgreSQL).
-- Adapted from SQLite migration 077.

CREATE TABLE IF NOT EXISTS reasoning_strategies (
    id           TEXT   PRIMARY KEY NOT NULL,
    summary      TEXT   NOT NULL,
    outcome      TEXT   NOT NULL,
    task_hint    TEXT   NOT NULL,
    created_at   BIGINT NOT NULL,
    last_used_at BIGINT NOT NULL,
    use_count    BIGINT NOT NULL DEFAULT 0,
    embedded_at  BIGINT
);

CREATE INDEX IF NOT EXISTS idx_reasoning_strategies_last_used_at
    ON reasoning_strategies (last_used_at);

CREATE INDEX IF NOT EXISTS idx_reasoning_strategies_use_count
    ON reasoning_strategies (use_count);
