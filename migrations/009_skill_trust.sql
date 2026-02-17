CREATE TABLE IF NOT EXISTS skill_trust (
    skill_name TEXT PRIMARY KEY NOT NULL,
    trust_level TEXT NOT NULL DEFAULT 'quarantined',
    source_kind TEXT NOT NULL DEFAULT 'local',
    source_url TEXT,
    source_path TEXT,
    blake3_hash TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
