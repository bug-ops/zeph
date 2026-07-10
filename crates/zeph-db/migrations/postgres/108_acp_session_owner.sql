-- Adds connection/tenant-scoped ownership to `acp_sessions` (#5868): ACP session listing and
-- loading was previously global across every connection sharing one SQLite store. `owner_key`
-- is the authenticated ACP client identity (bearer-token client id, or "acp-local" for stdio /
-- unauthenticated HTTP). NULL means either a legacy pre-fix row or a non-ACP-channel row
-- (CLI/TUI/Telegram via `zeph_session::SessionStore::create`, spec-068 Decision D1 — no second
-- sessions table) — those rows are never scoped/filtered by ACP handlers.
ALTER TABLE acp_sessions ADD COLUMN IF NOT EXISTS owner_key TEXT;

-- Serves the scoped list query `WHERE owner_key = ? ORDER BY updated_at DESC`.
CREATE INDEX IF NOT EXISTS idx_acp_sessions_owner_updated ON acp_sessions(owner_key, updated_at);
