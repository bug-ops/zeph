-- Durable execution layer (spec-064, #4944): external-completion handles.
-- The 32-byte resolver token is never stored; only its BLAKE3 hash (INV-9).
CREATE TABLE durable_promises (
    promise_id          TEXT    PRIMARY KEY,
    execution_id        TEXT    NOT NULL REFERENCES durable_executions(execution_id),
    resolver_token_hash BYTEA   NOT NULL,
    resolved            INTEGER NOT NULL DEFAULT 0,
    payload             BYTEA,
    created_at          BIGINT  NOT NULL,
    resolved_at         BIGINT
);
