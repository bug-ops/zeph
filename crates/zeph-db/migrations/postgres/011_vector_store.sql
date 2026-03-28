CREATE TABLE IF NOT EXISTS vector_collections (
    name TEXT PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS vector_points (
    id         TEXT NOT NULL,
    collection TEXT NOT NULL REFERENCES vector_collections(name),
    vector     BYTEA NOT NULL,
    payload    JSONB NOT NULL DEFAULT '{}'::jsonb,
    PRIMARY KEY (collection, id)
);

CREATE INDEX IF NOT EXISTS idx_vector_points_collection ON vector_points(collection);
