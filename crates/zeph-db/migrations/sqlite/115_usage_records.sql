-- Per-message usage/cost tracking (issue #6549).
-- Durable per-LLM-call usage ledger, independent of `CostTracker`'s in-memory daily
-- aggregate. `message_id` is set for conversational turns (1:1 with `messages.id`, enforced
-- by the partial unique index below) and NULL for background/orchestration calls (planner,
-- aggregator, ensemble members) that never produce a persisted conversational message.
-- `created_at` is UTC in both dialects (`datetime('now')` here, `NOW()` on Postgres) and is
-- the reconciliation key: SUM(cost_cents) for the current UTC day must equal
-- CostTracker::current_spend().
CREATE TABLE IF NOT EXISTS usage_records (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id         INTEGER REFERENCES messages(id) ON DELETE CASCADE,
    conversation_id    INTEGER REFERENCES conversations(id) ON DELETE CASCADE,
    -- 'conversation' | 'planner' | 'aggregator' | 'ensemble_member'
    source             TEXT    NOT NULL,
    provider_name      TEXT    NOT NULL,
    model_name         TEXT    NOT NULL,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    -- Subset of output_tokens (OpenAI o-series only); NULL when the provider doesn't report it.
    reasoning_tokens   INTEGER,
    cost_cents         REAL    NOT NULL DEFAULT 0,
    -- Full call latency (request send -> response fully received); always populated.
    latency_ms         INTEGER NOT NULL DEFAULT 0,
    -- True TTFT on the one production streaming path (speculative decoding, captured at
    -- SpeculativeStreamDrainer::drive's stream-consumption point in zeph-core) or a TTFB
    -- (time-to-first-byte) proxy measured around the HTTP send otherwise. NULL only for the
    -- in-process Candle backend, which has no network round-trip.
    ttft_ms            INTEGER,
    tokens_per_sec     REAL,
    created_at         TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS uq_usage_records_message ON usage_records(message_id)
    WHERE message_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_usage_records_created_at ON usage_records(created_at);
CREATE INDEX IF NOT EXISTS idx_usage_records_conversation ON usage_records(conversation_id)
    WHERE conversation_id IS NOT NULL;
