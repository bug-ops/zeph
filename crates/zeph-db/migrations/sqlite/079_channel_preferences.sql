-- SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Channel preferences: per-channel UX state persisted across restarts (#3308).
-- Identity is a composite (channel_type, channel_id) pair:
--   channel_type — one of "cli", "tui", "telegram", "discord"
--   channel_id   — chat/user scope within the type (Telegram chat_id as decimal string;
--                  empty string "" for single-user CLI/TUI channels)
CREATE TABLE IF NOT EXISTS channel_preferences (
    channel_type TEXT    NOT NULL,
    channel_id   TEXT    NOT NULL DEFAULT '',
    pref_key     TEXT    NOT NULL,
    pref_value   TEXT    NOT NULL,
    updated_at   INTEGER NOT NULL, -- unix epoch milliseconds
    PRIMARY KEY (channel_type, channel_id, pref_key)
);
