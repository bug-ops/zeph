# Session Persistence and Resume

Every conversation-session in Zeph (CLI, TUI, Telegram, Discord, Slack, ACP, or `zeph
serve-sessions`) can maintain a durable, replayable event log — a crash-safe, forkable record of
everything that happened in that session, independent of the `SQLite` `messages` table used for
search and recall.

## Why a Separate Event Log?

The existing `SQLite` `messages` projection is optimized for semantic search and context
assembly, not for exact replay. It doesn't capture tool-call/result pairing precisely enough to
reconstruct a conversation byte-for-byte after a crash, and it has no concept of "this session was
forked from that one at this exact point."

The event log (`zeph-session` crate) exists specifically to answer: *what exactly happened in this
session, in order, and can I replay it deterministically?*

## The Event Log

Each session writes to its own append-only JSONL file:

```text
<data_dir>/sessions/<session_id>/events.jsonl
```

Every line is one `SessionEvent` — `SessionStarted`, `UserMessage`, `AssistantMessage`,
`ToolCall`, `ToolResult`, `Condensation`, `Compaction`, or `ForkPoint` — tagged with a sequence
number (`seq`), a timestamp, and (for tool events) a turn ID. The log is the source of truth;
the `SQLite` projection is reconciled from it, not the other way around.

**Crash safety**: an event is durably appended and flushed *before* the corresponding `SQLite`
write happens, never after. If the process crashes between the two, the log is ahead — on next
open, a torn trailing write (partial JSON from a crash mid-append) is detected and truncated
cleanly rather than causing a parse error, and the `SQLite` projection is reconciled forward from
the log to catch up.

## `SessionId` and `ConversationId`

Every session has a `SessionId` (a UUID string) linked one-to-one with a `ConversationId` (the
existing `SQLite` conversation primary key). Looking up a session by id resolves its linked
conversation, and vice versa — this bijection is what lets `/conv resume`/`/conv fork` swap the
active conversation on a running agent without losing the connection between the durable log and
the searchable `SQLite` history.

## Browsing and Resuming Sessions

### `/conv` — In-Conversation Slash Command

```text
/conv list                  # table of all sessions: id, title, status, event count, updated
/conv show <id>             # one session's metadata
/conv resume <id>           # swap the CURRENT conversation onto <id>, replaying its history
/conv fork <id>             # copy <id> into a fresh session, then swap onto the copy
```

`/conv` is a channel-agnostic slash command — it works identically whether typed in the CLI, the
TUI, or a chat channel, because it's implemented once against the shared `AgentAccess` interface
rather than per-channel.

`/conv resume` and `/conv fork` perform a **live, mid-session swap**: the agent's message history,
active `ConversationId`, and durable-log target all switch to the resumed/forked session between
turns — no restart required. In the TUI, a "Replaying conversation…" status indicator shows while
the swap is in progress.

### `zeph sessions` — CLI Subcommand

```bash
zeph sessions list                      # table of all sessions
zeph sessions show <id> [--events]      # metadata, or full event-log dump with --events
zeph sessions resume <id>               # launch a live interactive agent bound to <id>
zeph sessions resume <id> --print       # dump events to stdout instead (no agent started)
zeph sessions fork <id> [--at N]        # eager-copy <id> into a fresh session
zeph sessions export <id> <path>        # write the event log to a file
zeph sessions import <path>             # load an exported event log as a new session
```

## Forking

Forking eager-copies a session's event log up to a cut point (`--at N`, or the full log by
default) into a fresh session with its own id:

- The child's log opens with a `SessionStarted` event recording `forked_from: (parent_id, seq)`.
- The parent's log gets a non-destructive `ForkPoint` provenance record — forking never mutates
  the parent's own history.
- The child gets its own `ConversationId`, independent of the parent.

A session can only be forked at a point *within its own logged history* — old sessions predating
event-log persistence (see [Legacy Sessions](#legacy-sessions) below) can only be forked at the
import boundary, not at an arbitrary historical point, since granular tool-call replay isn't
available before that boundary.

## Condensation

Distinct from live, in-memory context compaction (see [Context
Budgets](../concepts/context-budgets.md)), *condensation* operates at the event-log level: when a
session's durable history grows large, a `Condenser` (the default implementation,
`LlmCondenser`, reuses the same summarization pipeline as in-memory compaction) replaces a range
of events with a single `Condensation` event carrying a structured summary. Replay folds a
`Condensation` event into one system message representing everything it replaced — a later
condensation is guaranteed not to overlap an earlier one's replaced range.

## Legacy Sessions

Installs upgrading from before session persistence existed have `SQLite` message history for
conversations with no event log at all. That history is **not** retroactively synthesized into a
full log — tool-call/result pairing isn't recoverable from the old projection, so any attempt
would be lossy and potentially misleading.

Instead, the first time such a session is resumed (via `/conv resume`, `zeph sessions resume`, or
an ACP client reconnecting), Zeph writes a `SessionStarted` header plus a single event
summarizing "this session had N prior messages before durable logging existed," then proceeds
normally — every turn after that point is logged in full detail. A session bootstrapped this way
can be forked and replayed from that import boundary onward, just not from before it.

## Configuration

```toml
[session]
enabled = true               # maintain a durable event log per session (default: true)
data_dir = ".zeph/sessions"  # directory under which event logs are stored
encrypt = false               # opt-in AEAD encryption — reserved, not yet implemented
max_event_log_mb = 256        # size guard that triggers condensation

[session.condense]
condense_provider = "fast"    # provider name from [[llm.providers]]; empty = primary provider
threshold = 0.85               # fraction of context budget that triggers condensation
keep_recent = 20               # minimum recent events preserved after condensation
```

When `[session] enabled = false`, only the `SQLite` `messages` projection is written — the
pre-session-persistence behavior. `encrypt` is a placeholder for a future AEAD implementation; the
current protection for event-log files is OS file permissions (`0o600` on Unix).

## See Also

- [`zeph serve` — Persistent Agent Service](serve-mode.md) — exposes durable sessions over
  HTTP/SSE, backed by the same event log described here.
- [Migrate Config](../guides/migrate-config.md) — `--migrate-config` adds `[session]`/`[serve]`
  sections to existing config files.
- [CLI Reference](../reference/cli.md) — full `zeph sessions` and `/conv` syntax.
