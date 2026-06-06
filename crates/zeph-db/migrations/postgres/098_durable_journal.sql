-- Durable execution layer (spec-064, #4944): append-only journal entries.
-- payload holds AEAD-sealed bytes (nonce || ciphertext || tag); control entries leave it NULL.
CREATE TABLE durable_journal (
    seq             BIGSERIAL PRIMARY KEY,                 -- global append order (durability anchor)
    execution_id    TEXT      NOT NULL REFERENCES durable_executions(execution_id),
    step_id         BIGINT    NOT NULL,
    entry_kind      TEXT      NOT NULL,
    idem_key        BYTEA,                                 -- IdempotencyKey (32B); NULL for non-step entries
    effect_class    TEXT,
    payload         BYTEA,                                 -- AEAD-sealed; NULL for control entries
    payload_version INTEGER,
    hmac            BYTEA,                                 -- row-level HMAC for shared-DB / Restate
    created_at      BIGINT    NOT NULL
);

CREATE INDEX idx_durable_journal_exec_step
    ON durable_journal(execution_id, step_id, seq);

-- Enforce at most one committed result per step (defense in depth alongside the writer).
CREATE UNIQUE INDEX idx_durable_journal_result
    ON durable_journal(execution_id, step_id)
    WHERE entry_kind = 'step_result';

-- Efficient exactly-once intent lookup ("does this intent already exist?").
CREATE INDEX idx_durable_journal_idem_key
    ON durable_journal(execution_id, idem_key)
    WHERE idem_key IS NOT NULL;
