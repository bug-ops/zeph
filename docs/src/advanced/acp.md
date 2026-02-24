# ACP (Agent Client Protocol)

Zeph implements the [Agent Client Protocol](https://agentclientprotocol.com) for IDE integration. The ACP server manages concurrent sessions, proxies tool execution to the IDE, and exposes a set of custom extension methods for session management and agent introspection.

## Custom Methods

Zeph extends the base ACP protocol with 7 custom methods dispatched via `ext_method`. All method names use a leading underscore prefix to avoid collision with the standard protocol.

| Method | Description |
|---|---|
| `_session/list` | List all sessions (in-memory + persisted) |
| `_session/get` | Get session details and event history |
| `_session/delete` | Delete a session from memory and persistent store |
| `_session/export` | Export session events for backup or migration |
| `_session/import` | Import events into a new session |
| `_agent/tools` | List available agent tools |
| `_agent/working_dir/update` | Update the working directory for a session |

### _session/list

Returns all known sessions, merging in-memory (live) sessions with persisted sessions from SQLite. In-memory sessions override persisted entries when both exist for the same ID.

**Params:** `{}`

**Response:**

```json
{
  "sessions": [
    { "session_id": "abc-123", "created_at": "2026-01-15T10:30:00Z", "busy": false }
  ]
}
```

The `busy` field is `true` when the session is actively processing a prompt (no output available yet).

### _session/get

Retrieves details for a single session, including its full event history from the persistent store.

**Params:** `{ "session_id": "abc-123" }`

**Response:**

```json
{
  "session_id": "abc-123",
  "created_at": "2026-01-15T10:30:00Z",
  "busy": false,
  "events": [
    { "event_type": "user_message", "payload": "..." }
  ]
}
```

Returns an error if the session is not found in memory or in the persistent store.

### _session/delete

Removes a session from both in-memory state and SQLite. Returns `{ "deleted": true }` if the session existed in either location.

**Params:** `{ "session_id": "abc-123" }`

### _session/export

Exports all persisted events for a session. Useful for backup, migration, or debugging.

**Params:** `{ "session_id": "abc-123" }`

**Response:**

```json
{
  "session_id": "abc-123",
  "events": [ ... ],
  "exported_at": "2026-01-15T10:35:00Z"
}
```

### _session/import

Creates a new session and replays imported events into SQLite using an atomic transaction. Returns the newly generated session ID (UUID v4).

**Params:**

```json
{
  "events": [
    { "event_type": "user_message", "payload": "..." }
  ]
}
```

**Response:** `{ "session_id": "<new-uuid>" }`

The event count is capped at 10,000 (`MAX_IMPORT_EVENTS`). Requests exceeding this limit are rejected.

### _agent/tools

Returns the list of tools available to the agent within a session context.

**Params:** `{ "session_id": "abc-123" }`

**Response:**

```json
{
  "tools": [
    { "id": "bash", "description": "Execute shell commands" },
    { "id": "read_file", "description": "Read file contents" },
    { "id": "write_file", "description": "Write or update file contents" },
    { "id": "search", "description": "Search file content with regex" },
    { "id": "web_scrape", "description": "Fetch and extract content from a URL" }
  ]
}
```

### _agent/working_dir/update

Updates the working directory for an active in-memory session. Only succeeds if the session exists in memory.

**Params:** `{ "session_id": "abc-123", "path": "/workspace/project" }`

**Response:** `{ "updated": true }`

## Auth Hints

The `initialize` response includes an `auth_hint` key in its `meta` field, signaling to the IDE client that authentication is required. This allows clients to present appropriate credential prompts before sending prompts.

## Security

### Session ID Validation

All methods that accept a `session_id` parameter enforce strict validation:

- Maximum length: 128 characters
- Allowed characters: `[a-zA-Z0-9_-]`

Requests with invalid session IDs are rejected with an `invalid_request` error.

### Path Traversal Protection

The `_agent/working_dir/update` method rejects any path containing `..` (parent directory) components, preventing path traversal attacks that could escape the intended workspace boundary.

### Import Size Cap

Session import is limited to 10,000 events per request (`MAX_IMPORT_EVENTS`), preventing denial-of-service through oversized payloads. The import is executed as an atomic SQLite transaction — either all events are written or none are.

## Configuration

ACP is configured in `config.toml` under the `[acp]` section:

```toml
[acp]
max_sessions = 4
session_idle_timeout_secs = 1800
# permission_file = "~/.config/zeph/acp-permissions.toml"
```

See [Configuration Reference](../reference/configuration.md) for environment variable overrides.
