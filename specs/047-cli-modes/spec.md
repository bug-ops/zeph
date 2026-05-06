---
aliases:
  - CLI Modes
  - Bare Mode
  - JSON Output Mode
  - Loop Command
tags:
  - sdd
  - spec
  - cli
  - channels
created: 2026-04-19
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[002-agent-loop/spec]]"
  - "[[007-channels/spec]]"
  - "[[042-zeph-commands/spec]]"
---

# Spec: CLI Modes — --bare, --json, -y, /loop, /recap

> [!info]
> Structured CLI execution modes and related slash commands for non-interactive,
> scriptable, and supervised agent operation. Landed primarily in #3170 (--bare,
> --json, -y, /loop) and #3218 (/recap). Fix #3219 closed spurious JSON events
> and /loop error surfacing.

## Sources

### Internal

| File | Contents |
|---|---|
| `crates/zeph-channels/src/json_cli.rs` | `JsonCliChannel` — JSONL event emitter |
| `crates/zeph-channels/src/cli.rs` | `CliChannel` — standard interactive CLI |
| `crates/zeph-core/src/json_event_sink.rs` | `JsonEventSink`, `JsonEvent` enum |
| `src/main.rs` | `--bare`, `--json`, `-y` flag parsing; mode dispatch |
| `crates/zeph-commands/src/commands/loop_cmd.rs` | `/loop` command handler |
| `crates/zeph-commands/src/commands/recap.rs` | `/recap` command handler |

---

## 1. Overview

### Problem Statement

Zeph lacked structured scriptable output. Integrating Zeph in CI pipelines, calling it
from scripts, or testing it programmatically required screen-scraping the interactive
terminal UI. Additionally, the agent started expensive subsystems (code indexer, memory
eviction, scheduler) even in lightweight single-shot invocations.

Two related usability gaps:
1. No supervised loop mode where the user can break out after each turn.
2. No compact session recap for handoff or logging.

### Goal

Provide three complementary CLI execution modes and two slash commands:

- **`--bare`** — minimal startup: skip scheduler, code indexer, and background
  memory eviction. Useful for one-shot queries, tests, and embedded use.
- **`--json`** — machine-readable JSONL event stream on stdout; all logs on stderr.
  Used by CI integrations, wrappers, and the test harness.
- **`-y` / `--auto`** — auto-approve all confirmation prompts. Used with `--json`
  in non-interactive contexts.
- **`/loop`** — interactive prompt loop: after each response, ask the user whether
  to continue; surface errors inline instead of terminating.
- **`/recap`** — summarize the current session into a compact text for handoff,
  logging, or pasting.

### Out of Scope

- TUI mode interaction (separate `011-tui` spec)
- Telegram channel (separate `007-channels` spec)
- MCP elicitation in JSON mode (handled at channel level — see `008-4-elicitation`)
- Persistent session replay from JSONL files

---

## 2. User Stories

### US-001: Scripted single-shot query
AS A CI pipeline developer
I WANT to call `zeph --json --bare -y -p "summarize the diff"` and receive JSONL
SO THAT I can parse agent responses without screen-scraping

**Acceptance criteria:**
```
GIVEN --json --bare -y flags are set
WHEN the agent completes a single turn
THEN stdout contains only valid JSONL events (response_chunk, response_end, cost, tool_call, tool_result)
AND stderr contains all log output
AND no scheduler, code indexer, or memory eviction processes start
```

### US-002: Supervised interactive loop
AS A developer running an extended research session
I WANT /loop to prompt me after each turn
SO THAT I can stop cleanly without Ctrl-C when the session is complete

**Acceptance criteria:**
```
GIVEN /loop is active
WHEN the agent completes a turn
THEN a "Continue? [y/N]" prompt appears
AND entering "n" or pressing Enter exits cleanly
AND entering "y" resumes the conversation
AND any error during a turn is shown inline without terminating the loop
```

### US-003: Session recap for handoff
AS AN agent or operator at the end of a session
I WANT /recap to produce a compact session summary
SO THAT I can paste it into a handoff document or log

**Acceptance criteria:**
```
GIVEN a session with at least 3 turns
WHEN /recap is invoked
THEN a compact summary (≤ 512 tokens) is emitted to the channel
AND the summary covers key decisions, facts discovered, and open questions
AND the command is available in the slash command registry and TUI autocomplete
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `--bare` is set THE SYSTEM SHALL skip startup of: scheduler task loop, code indexer background worker, memory eviction/dream pass | must |
| FR-002 | WHEN `--json` is set THE SYSTEM SHALL construct a `JsonCliChannel` and route all log output to stderr before the channel is active | must |
| FR-003 | WHEN `--json` is set THE SYSTEM SHALL emit only valid JSONL on stdout; each line is a `JsonEvent` with a stable schema | must |
| FR-004 | `JsonCliChannel` SHALL NOT emit `response_end` unless at least one `response_chunk` has been emitted in the current turn (double-emission prevention) | must |
| FR-005 | `JsonCliChannel.send_tool_start`, `send_tool_output`, and `send_usage` SHALL be no-ops; `JsonEventLayer` in `zeph-core` is the canonical emitter for those events | must |
| FR-006 | WHEN `-y` / `--auto` is set THE SYSTEM SHALL auto-approve all `confirm()` calls without reading stdin | must |
| FR-007 | WHEN `/loop` is active THE SYSTEM SHALL present a continuation prompt after each turn and surface errors inline (not terminate) | must |
| FR-008 | WHEN `/loop` receives an error from the agent THE SYSTEM SHALL display the error text and remain in the loop; exit only on explicit "n" or empty input | must |
| FR-009 | WHEN `/recap` is invoked THE SYSTEM SHALL produce a summary of the current session via the configured recap provider and emit it to the channel | must |
| FR-010 | `/recap` SHALL be registered in `COMMANDS` (static list in `zeph-commands`) for `/help` and TUI autocomplete | must |
| FR-011 | WHEN `--bare` is set THE SYSTEM SHALL still load skills, vault, and MCP servers — only background non-interactive workers are skipped | must |
| FR-012 | WHEN `--json` and `--bare` are combined THE SYSTEM SHALL suppress interactive prompts; stdin is treated as a JSONL command stream | should |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | `--bare` startup SHALL complete in < 500 ms on a warm binary (no indexer, no scheduler overhead) |
| NFR-002 | Performance | JSONL event write latency SHALL be < 1 ms per event (unbuffered line-at-a-time write to stdout) |
| NFR-003 | Reliability | `JsonCliChannel` SHALL never emit a `response_end` without a preceding `response_chunk` in the same turn (enforced by `pending_chunks` flag) |
| NFR-004 | Reliability | `/loop` SHALL never terminate the process on a per-turn agent error — errors are display-only |
| NFR-005 | Reliability | `/recap` failure (provider error, timeout) SHALL emit an error message to the channel and not crash |
| NFR-006 | Usability | JSONL schema is versioned (`"schema": 1` field on each event); consumers can version-check before parsing |
| NFR-007 | Usability | `/loop` continuation prompt uses channel-native output (CLI: inline; TUI: status bar; JSON: `elicitation` event) |
| NFR-008 | Security | `-y` / `--auto` applies only to agent-generated confirmation prompts; MCP elicitation with sensitive fields (password/token/key) still emits a warning even in auto mode |
| NFR-009 | Observability | `--json` mode includes a `cost` event after each turn with token usage and estimated cost |

---

## 5. JSON Event Schema

All events are single-line JSON objects:

```jsonc
// Response text chunk
{"type": "response_chunk", "text": "...", "schema": 1}

// Turn complete
{"type": "response_end", "schema": 1}

// Tool invocation
{"type": "tool_call", "tool": "shell", "input": {...}, "id": "...", "schema": 1}

// Tool result
{"type": "tool_result", "id": "...", "output": "...", "exit_code": 0, "schema": 1}

// Token and cost summary
{"type": "cost", "input_tokens": 1234, "output_tokens": 456,
 "cache_read_tokens": 789, "estimated_usd": 0.0042, "schema": 1}

// Error (turn-level)
{"type": "error", "message": "...", "code": "agent_error", "schema": 1}

// Elicitation request (MCP or /loop continuation prompt)
{"type": "elicitation", "prompt": "Continue? [y/N]", "fields": [...], "schema": 1}
```

> [!warning]
> `send_tool_start`, `send_tool_output`, and `send_usage` in `JsonCliChannel` are
> intentionally no-ops. Tool and cost events are emitted by `JsonEventLayer` in
> `zeph-core`, which has direct access to all providers. This avoids double-emission.

---

## 6. Key Invariants

### Always (without asking)
- `--json` forces all log output to stderr before `JsonCliChannel` is constructed
- `JsonCliChannel` tracks `pending_chunks`; `response_end` is suppressed if no chunks have been emitted in the current turn
- `--bare` skips scheduler, code indexer, and memory eviction — never skips vault, skills, or MCP
- `/loop` never terminates on agent error; always presents the continuation prompt
- `-y` never auto-approves sensitive MCP elicitation fields without a warning
- JSONL schema version field (`"schema": 1`) is included on every event line

### Ask First
- Adding new `JsonEvent` variants (breaks downstream parsers that do not handle unknown types)
- Changing the schema version (requires migration guide for consumers)
- Adding subsystems that `--bare` should skip (review cost vs. usability tradeoff)

### Never
- Emit `response_end` without a preceding `response_chunk` in the same turn
- Write log output to stdout when `--json` is active
- Terminate the process inside `/loop` due to a per-turn error
- Bypass vault resolution in `--bare` mode

---

## 7. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `--json` and stdin is a TTY | Read lines interactively; each line triggers a new turn; emit JSONL for each |
| `--json` and stdin is piped | Read until EOF; each non-empty line is a prompt; emit JSONL events per turn |
| `--bare` and a skill requires the code indexer | Skill invocation returns a graceful error; indexer is not started mid-session |
| `/loop` and provider returns 429 | Emit `{"type":"error",...}` event; loop continues; user can retry or exit |
| `/recap` with empty session | Emit a short message ("No turns in session"); no LLM call |
| `-y` and MCP elicitation with type=password | Emit a WARN log; auto-approve the prompt but mark the elicitation response with `warned=true` |
| `response_end` without chunks (MARCH marker race) | Suppressed by `pending_chunks` guard; no duplicate `response_end` emitted |
| `/loop` user enters empty string | Treated as "n" (stop); clean exit |

---

## 8. Success Criteria

- [ ] `--json` integration test: output parses as valid JSONL with expected event types
- [ ] `--json` double-emission test: no `response_end` without preceding `response_chunk` under race condition (#3244)
- [ ] `--bare` startup test: scheduler, indexer, and eviction tasks do not start (verified via tracing spans)
- [ ] `/loop` error test: induced provider error produces inline error message, loop continues
- [ ] `/recap` test: three-turn session produces summary with ≤ 512 tokens
- [ ] `/recap` registered in `COMMANDS` (TUI autocomplete test)
- [ ] `-y` auto-approval test: `confirm()` returns true without stdin read
- [ ] JSONL schema version field present on all emitted events

---

## 9. Acceptance Criteria

```
GIVEN --json --bare -y flags
WHEN the agent processes a single prompt with one tool call
THEN stdout lines parse as JSON objects
AND event types are in {response_chunk, response_end, tool_call, tool_result, cost}
AND no scheduler, indexer, or eviction spans appear in the trace

GIVEN /loop is active
  AND the agent produces an error on turn 2
WHEN the user sees the error
THEN a continuation prompt appears ("Continue? [y/N]")
AND entering "y" sends a new prompt
AND the session continues from turn 3

GIVEN a session with 5 turns
WHEN /recap is invoked
THEN a summary is emitted within 30 s
AND the summary is ≤ 512 tokens as measured by the embedding provider tokenizer
```

---

## 10. `zeph project purge` Command (#3598)

`zeph project purge` performs a full reset of all persisted project state for the
current working directory. It is a destructive, operator-only command with a mandatory
confirmation prompt.

### What Is Purged

| State | Location | Action |
|-------|----------|--------|
| Conversation history | SQLite `messages` table | Deleted for the current project path |
| Memory embeddings | Qdrant collection for the project | Vectors deleted |
| Graph entities and edges | SQLite `entities`, `edges` tables | Deleted for the project |
| Tool audit log | `audit.jsonl` | Deleted |
| Summaries and compactions | SQLite `summaries` table | Deleted for the project |
| Plan history | SQLite `plans` table | Deleted for the project |
| Provider preference | SQLite `channel_preferences` | Deleted for the project |
| Code index | SQLite code-index tables | Deleted for the project |
| Debug dumps | `.local/debug/<session>*` | Deleted if `--include-debug` is passed |

Skills, vault secrets, and config files are **never** purged by this command.

### Invocation

```
zeph project purge [--yes] [--include-debug] [--dry-run]
```

| Flag | Effect |
|------|--------|
| `--yes` / `-y` | Skip the confirmation prompt (matches global `-y` semantics) |
| `--include-debug` | Also delete debug dumps from `.local/debug/` |
| `--dry-run` | Print what would be purged without deleting anything |

### Confirmation Prompt (default)

```
WARNING: This will permanently delete all conversation history, memory, and tool
audit logs for project at /path/to/project. This cannot be undone.

Type the project name to confirm: <project-name>
```

The user must type the project directory name (last path component) exactly to
proceed. This prevents accidental purge from a mis-typed command.

### Key Invariants

- Purge is scoped to the **current project** (resolved from `cwd`). It does NOT purge
  other projects sharing the same SQLite database.
- Skills, vault secrets, and `config.toml` are NEVER touched.
- Qdrant vector deletion is best-effort — if Qdrant is unavailable, the SQLite
  embedding references are still deleted and the command succeeds.
- `--dry-run` must not perform any write operations — read-only inspection only.
- NEVER auto-approve the confirmation prompt in scripts without `--yes` / `-y`.
- The audit log entry for the purge operation itself is written BEFORE the audit log
  is deleted (so forensics can confirm when the purge occurred).
- Exit code 0 on success; exit code 1 on user cancellation; exit code 2 on error.

---

## 11. Open Questions

> [!question]
> - **`/loop` count limit**: should `/loop` support a `--count N` variant that runs N
>   turns automatically before prompting? This would enable supervised batch processing.
>   Not in the current implementation; define before adding.
> - **JSONL schema versioning**: is a single global `schema: 1` version sufficient, or
>   should each event type carry its own version? Decide before adding new event types
>   that break existing consumers.

---

## 12. See Also

- [[constitution]] — project principles
- [[002-agent-loop/spec]] — turn lifecycle
- [[007-channels/spec]] — channel trait and AnyChannel dispatch
- [[042-zeph-commands/spec]] — slash command registry (/recap, /loop registration)
- [[008-mcp/008-4-elicitation]] — MCP elicitation in JSON mode
- [[MOC-specs]] — all specifications
