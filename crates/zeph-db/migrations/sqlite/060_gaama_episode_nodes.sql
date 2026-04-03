-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- GAAMA episode nodes: one episode per conversation.
-- Links extracted entities to the conversation context in which they were observed.
-- Enables episode-boundary-aware retrieval (facts from current conversation are
-- preferred over facts from older episodes).

CREATE TABLE IF NOT EXISTS graph_episodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    closed_at TEXT,
    UNIQUE(conversation_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_episodes_conv ON graph_episodes(conversation_id);

-- Entity-episode membership: which entities were observed in which episode.
CREATE TABLE IF NOT EXISTS graph_episode_entities (
    episode_id INTEGER NOT NULL REFERENCES graph_episodes(id) ON DELETE CASCADE,
    entity_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
    first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (episode_id, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_graph_episode_entities_entity
    ON graph_episode_entities(entity_id);

-- Rename graph_edges.episode_id (message-level provenance) to source_message_id
-- to avoid semantic confusion with the new conversation-level graph_episodes table.
-- SQLite does not support DROP COLUMN on tables with foreign keys in older versions,
-- so we use a three-step: add column, copy data, leave old column as NULL.
-- The old episode_id column is kept as a no-op shadow to avoid breaking existing
-- queries that may reference it until a follow-up migration removes it entirely.
ALTER TABLE graph_edges ADD COLUMN source_message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL;
UPDATE graph_edges SET source_message_id = episode_id WHERE episode_id IS NOT NULL;
