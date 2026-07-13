-- Out-of-band, step-counter-independent notification claim for durable subagent replay (#6027).
-- A resumed foreground @mention/spawn whose child already resolved replays the journaled result
-- and fires a one-time channel notice + TUI completion event. Those side effects must fire at
-- most once even if the parent restarts AGAIN after the first replay. `notified_at` (Unix epoch
-- millis, NULL until claimed) records the single winning claim: the first
-- `UPDATE ... SET notified_at = ? WHERE promise_id = ? AND notified_at IS NULL` transitions the
-- row (rows_affected = 1) and fires the effects; every later replay sees a non-NULL value and
-- suppresses them. This is deliberately NOT a durable journal step: it consumes no StepId, so it
-- cannot perturb INV-2 step-id determinism the way a replay-only ctx.step would (#6027 C1).
ALTER TABLE durable_promises ADD COLUMN notified_at INTEGER;
