-- MemScene: groups of semantically related MemCells with a stable entity profile.
-- Original MemCells are preserved (non-destructive); the scene adds a grouping layer.
CREATE TABLE IF NOT EXISTS mem_scenes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    label TEXT NOT NULL,
    profile TEXT NOT NULL,
    member_count INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Junction table linking messages to scenes (N:M — a message can belong to multiple scenes).
CREATE TABLE IF NOT EXISTS mem_scene_members (
    scene_id INTEGER NOT NULL REFERENCES mem_scenes(id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    PRIMARY KEY (scene_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_mem_scene_members_msg ON mem_scene_members(message_id);
