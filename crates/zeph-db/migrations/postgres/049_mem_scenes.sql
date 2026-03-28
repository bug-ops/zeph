-- MemScene: groups of semantically related MemCells with a stable entity profile.
CREATE TABLE IF NOT EXISTS mem_scenes (
    id           BIGSERIAL PRIMARY KEY,
    label        TEXT NOT NULL,
    profile      TEXT NOT NULL,
    member_count INTEGER NOT NULL DEFAULT 0,
    created_at   BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM NOW())::BIGINT,
    updated_at   BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM NOW())::BIGINT
);

-- Junction table linking messages to scenes (N:M).
CREATE TABLE IF NOT EXISTS mem_scene_members (
    scene_id   BIGINT NOT NULL REFERENCES mem_scenes(id) ON DELETE CASCADE,
    message_id BIGINT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    PRIMARY KEY (scene_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_mem_scene_members_msg ON mem_scene_members(message_id);
