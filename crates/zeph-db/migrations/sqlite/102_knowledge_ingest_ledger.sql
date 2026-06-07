-- Knowledge-ingest idempotency ledger (spec-067 §8, #5016).
--
-- Re-read / cost guard only (INV-5): records (source_uri, content_hash) of inputs already
-- ingested so an unchanged file is not re-embedded on the next run. It does NOT reconcile
-- LLM-extraction drift across model versions.
--
-- ingested_at is TEXT (ISO-8601) in both dialects to avoid TIMESTAMPTZ→String decode mismatch.
CREATE TABLE IF NOT EXISTS knowledge_ingest_ledger (
    source_uri      TEXT    NOT NULL,
    content_hash    TEXT    NOT NULL,
    import_batch_id TEXT    NOT NULL,
    ingested_at     TEXT    NOT NULL DEFAULT (datetime('now')),
    entities        INTEGER NOT NULL DEFAULT 0,
    edges           INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (source_uri, content_hash)
);
