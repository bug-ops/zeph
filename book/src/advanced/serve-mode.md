# zeph serve — Persistent Agent Service

`zeph serve-sessions` runs Zeph as a long-lived process exposing durable [conversation
sessions](session-persistence.md) over an HTTP/SSE API — create sessions, stream responses, and
manage them remotely without an interactive terminal.

> **Naming note**: the command is `zeph serve-sessions`, not the bare `zeph serve` you might
> expect. `zeph serve` already names the [scheduler's foreground daemon](daemon.md) — both
> features can be enabled in the same build, so a second command claiming the same name wasn't
> viable.

```bash
cargo build --release --features session
zeph serve-sessions
```

## REST + SSE API

All routes except `/health` operate on the same durable event log described in [Session
Persistence and Resume](session-persistence.md) — a session created here can be resumed later via
`/conv resume` or `zeph sessions resume` in any other channel, and vice versa.

| Method & Path | Purpose |
|---|---|
| `GET /health` | Unauthenticated liveness check — status, uptime, live session count |
| `POST /sessions` | Create a session |
| `GET /sessions` | List live session ids |
| `GET /sessions/:id` | Durable metadata + live status (works even for a session whose actor has ended — it's still resumable) |
| `DELETE /sessions/:id` | End a live session |
| `POST /sessions/:id/prompt` | Submit a prompt (fire-and-forget; `202 Accepted` once queued) |
| `GET /sessions/:id/events` | Server-Sent Events stream of the session's output (tokens, tool calls, tool results, turn completion) |
| `POST /sessions/:id/fork` | Eager-copy the session into a fresh child, which becomes immediately usable |

```bash
# Create a session and stream its output
SID=$(curl -s -X POST http://127.0.0.1:8420/sessions | jq -r .session_id)
curl -N "http://127.0.0.1:8420/sessions/$SID/events" &
curl -X POST "http://127.0.0.1:8420/sessions/$SID/prompt" \
  -H "Content-Type: application/json" \
  -d '{"text": "Summarize this repository."}'
```

## Session Actors

Each live session runs as an independent `SessionActor` — the agent that owns it runs on a
dedicated OS thread with its own async runtime, isolated from every other session and from the
HTTP server's own request-handling. Multiple SSE clients (or an SSE client and a TUI attaching to
the same session) can subscribe to one session's output concurrently; a subscriber that falls
behind has its own missed events dropped, never the connection — the durable log is always there
to catch up from.

## Idle Eviction

A background task (`serve.evict`) periodically scans for sessions with no attached output
subscribers whose activity has gone stale past `session_idle_ttl_secs`, and ends them —
freeing the OS thread and in-memory state. The session itself isn't deleted: its durable log
remains on disk and it can be resumed again later (via this API, `/conv resume`, or `zeph
sessions resume`), which spins up a fresh `SessionActor` and replays the log.

## Authentication

`/sessions*` routes can execute shell, file, and web tools on behalf of any caller that reaches
the port — treat this API with the same care as a shell. Bearer-token authentication is
controlled by `require_auth` (`true` by default) and `auth_token_vault_key`: the token itself is
never written to `config.toml` — only the *name* of the vault key it's resolved from at startup.

```bash
zeph vault set ZEPH_SERVE_AUTH_TOKEN "$(openssl rand -hex 32)"
```

If `require_auth = true` but no token can be resolved from the configured vault key, `zeph
serve-sessions` refuses to bind any address other than loopback — an unauthenticated API on a
non-loopback address is a real risk, not a warning to skip past. Set `require_auth = false` only
on a network you fully trust.

## `--acp`

The `--acp` flag does **not** run the ACP protocol transport in the same process as
`zeph serve-sessions` — combining them would mean two independent database connection pools
writing the same session-store file concurrently. Run ACP separately instead:

```bash
zeph serve-sessions &
zeph --acp        # or --acp-http, in a second process
```

Both processes share the same durable event logs on disk (via `[session] data_dir`), so sessions
created in one are visible and resumable from the other.

## Configuration

```toml
[serve]
http_addr = "127.0.0.1:8420"              # bind address
require_auth = true                        # require a bearer token on /sessions* (not /health)
auth_token_vault_key = "ZEPH_SERVE_AUTH_TOKEN"  # vault key name to resolve the token from
max_sessions = 50                          # maximum concurrent live sessions
session_idle_ttl_secs = 1800               # idle eviction TTL (30 minutes)
max_queued_prompts = 8                     # per-session prompt mailbox capacity
```

## See Also

- [Session Persistence and Resume](session-persistence.md) — the durable event log every session
  here is backed by.
- [Daemon and Scheduler](daemon.md) — `zeph serve` (the scheduler's foreground daemon, a
  different command).
- [ACP (Agent Client Protocol)](acp.md) — the transport `--acp` runs as a separate process.
