-- Cross-thread store write provenance (issue #6773).
-- Records the identity of the code path that performed the last write to a row so a
-- cross-writer overwrite is detectable and attributable, not silently invisible. NULL
-- means "no writer identity has ever been recorded for this row" — either it was written
-- before this migration, or every write to it so far was anonymous. A write that supplies
-- a writer sets it explicitly; an anonymous write (no writer supplied) preserves whatever
-- value was already there rather than clearing it (see zeph-memory SqliteStore::store_put /
-- StorePutOptions::with_writer, and its COALESCE(?, writer_id) update clauses).
ALTER TABLE cross_thread_store ADD COLUMN IF NOT EXISTS writer_id TEXT;
