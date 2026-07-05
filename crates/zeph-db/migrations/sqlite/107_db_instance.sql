-- Stable per-database identity (#5742). A random UUID generated once per physical
-- database, used to disambiguate autoincrementing IDs (e.g. conversation_id) when
-- multiple independent databases share one Qdrant instance.
CREATE TABLE IF NOT EXISTS db_instance (
    id          INTEGER PRIMARY KEY CHECK(id = 1),
    instance_id TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
