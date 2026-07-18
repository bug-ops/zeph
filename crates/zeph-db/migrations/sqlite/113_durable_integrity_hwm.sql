-- Durable transcript-integrity (issue #6360): authenticated per-execution high-water-mark.
--
-- The existing row HMAC (`durable_journal.hmac`) binds a control entry's own identity but not its
-- *presence* -- deleting a committed StepResult row is invisible to it. This table adds a signed
-- `{key_epoch, max_committed_step_id, committed_result_count}` tuple per execution, updated
-- in-transaction on every committed StepResult and checked once (O(1)) on resume: a mismatch means
-- a committed result was deleted or the count was tampered with outside the write path.
CREATE TABLE durable_execution_integrity (
    execution_id            TEXT    PRIMARY KEY REFERENCES durable_executions(execution_id),
    key_epoch               INTEGER NOT NULL,
    max_committed_step_id   INTEGER NOT NULL,
    committed_result_count  INTEGER NOT NULL,
    hwm_hmac                BLOB    NOT NULL,
    updated_at              INTEGER NOT NULL
);

-- Persists how many idempotent StepResult rows a checkpoint fold deleted, co-located with the
-- snapshot it describes. `committed_result_count` is invariant across a fold (the same results
-- move from live rows into the checkpoint, net-zero), so this column is read back only for the
-- resume-time cross-check: count(surviving StepResult rows) + SUM(folded_count over checkpoints)
-- must equal the signed `committed_result_count`. NULL for every non-checkpoint entry_kind.
ALTER TABLE durable_journal ADD COLUMN folded_count INTEGER;
