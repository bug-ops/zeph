-- Persists the TurnContextTrust of ingested graph knowledge (#6953, spec-067).
-- trust_level: 0/1/2 = TurnContextTrust; NULL = never ingest-classified, trust unassessed
-- (a reader must treat it as untrusted).
-- Backfill uses the stored GraphOrigin::as_str literals. `origin` keeps the FIRST writer, so a
-- row may have been re-written by a less trusted source since: 'ingest' (tier 0) is therefore
-- backfilled only for entities never touched after creation (last_seen_at = first_seen_at) and
-- never for edges, which carry no last-write signal. Everything else stays NULL (untrusted);
-- 'subagent' (1) and 'external-agent' (2) are already untrusted tiers and are kept.
ALTER TABLE graph_entities ADD COLUMN IF NOT EXISTS trust_level SMALLINT;
ALTER TABLE graph_edges    ADD COLUMN IF NOT EXISTS trust_level SMALLINT;
UPDATE graph_entities SET trust_level = CASE
    WHEN origin = 'ingest' AND last_seen_at = first_seen_at THEN 0
    WHEN origin = 'subagent' THEN 1
    WHEN origin = 'external-agent' THEN 2
END
WHERE origin IN ('ingest', 'subagent', 'external-agent');
UPDATE graph_edges SET trust_level = CASE origin WHEN 'subagent' THEN 1 WHEN 'external-agent' THEN 2 END
WHERE origin IN ('subagent', 'external-agent');
