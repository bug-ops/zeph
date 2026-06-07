-- SQLite migration 102; Postgres counterpart is 103. Parity guard matches on logical name 'graph_provenance', not number.
-- Knowledge-ingest Phase 0 (#5015): provenance columns on graph tables so recall can
-- isolate imported knowledge from conversation-derived knowledge (spec-067 INV-2/INV-3).
-- DEFAULT 'conversation' backfills every existing row to the conversation origin.
ALTER TABLE graph_edges    ADD COLUMN origin          TEXT NOT NULL DEFAULT 'conversation';
ALTER TABLE graph_edges    ADD COLUMN import_batch_id TEXT;
ALTER TABLE graph_edges    ADD COLUMN source_uri      TEXT;
ALTER TABLE graph_entities ADD COLUMN origin          TEXT NOT NULL DEFAULT 'conversation';
ALTER TABLE graph_entities ADD COLUMN import_batch_id TEXT;
