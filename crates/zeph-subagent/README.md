# zeph-subagent

[![Crates.io](https://img.shields.io/crates/v/zeph-subagent)](https://crates.io/crates/zeph-subagent)
[![docs.rs](https://img.shields.io/docsrs/zeph-subagent)](https://docs.rs/zeph-subagent)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

Subagent management for Zeph — spawning, zero-trust grants, JSONL transcripts, scoped tool access, and lifecycle hooks.

## Overview

Manages the full lifecycle of sub-agents: loading YAML definitions from disk, spawning isolated tokio tasks with their own LLM provider and filtered tool executor, tracking state, persisting JSONL transcripts for session resumption, and firing lifecycle hooks around tool calls. All capability grants follow a zero-trust model — sub-agents receive only explicitly granted tools, skills, and secrets.

## Key modules

| Module | Description |
|--------|-------------|
| `def` | `SubAgentDef` — YAML definition with frontmatter (model, tools, skills, grants, hooks, max_turns) and system prompt body |
| `manager` | `SubAgentManager` — spawn, cancel, status tracking, and communication channels |
| `grants` | `PermissionGrants`, `Grant`, `GrantKind`, `SecretRequest` — zero-trust delegation |
| `filter` | `FilteredToolExecutor` — scoped tool access with `tools.except` additional denylist; `PlanModeExecutor` — restricts to read-only tools |
| `hooks` | `HookDef`, `HookMatcher`, `SubagentHooks` — `PreToolUse`/`PostToolUse` per-agent hooks; `SubagentStart`/`SubagentStop` config-level hooks |
| `transcript` | `TranscriptWriter`, `TranscriptReader` — JSONL-backed history with `.meta.json` sidecars; prefix-based ID lookup; resume-by-ID; keyed-BLAKE3 hash-chained entries with vault-anchor downgrade resistance (opt-in, see [Transcript integrity](#transcript-integrity)) |
| `forward` | `ForwardSurfaces` — opt-in, per-turn forwarding of a running sub-agent's full text/thinking output to the TUI detail view and/or `--bare` stdout via a per-task `mpsc` ingress and a manager-owned sanitizing drain; disabled by default (`forward_transcript`) |
| `memory` | `MemoryScope` — `User`/`Project`/`Local`; memory directory lifecycle; injection into sub-agent system prompt |
| `state` | `SubAgentState` — `Submitted`/`Working`/`Completed`/`Failed`/`Canceled` |
| `resolve` | Definition discovery and 4-level priority resolution (CLI > project > user > config) |
| `command` | `AgentsCommand` enum driving `/agent` and `zeph agents` CLI subcommands |
| `fleet` | `FleetRegistry`, `SharedFleetRegistry`, `FleetSessionInfo`, `FleetSessionStatus` — live registry of running sub-agent sessions |
| `durable` | `DurableResolverSeat`, `SubagentResult`, `make_durable_promise`/`resolve_durable_promise`/`await_durable_subagent` — durable promises for crash-resumable spawns via `zeph-durable` |
| `cwd_guard` | `CwdLock` — process-wide working-directory lock for sub-agents that run without a dedicated worktree |

## Usage

Sub-agents are managed via chat commands and the `zeph agents` CLI:

```text
/agent list                    # list available definitions
/agent spawn researcher "summarize this PR"
/agent bg worker "run tests"   # background execution
/agent status                  # show active agents
/agent cancel <id>             # cancel by ID prefix
/agent resume <id> "continue"  # resume session with transcript
@researcher "what is Rust?"    # mention shorthand
```

CLI management outside a session:

```bash
zeph agents list
zeph agents create researcher --description "Web researcher"
zeph agents show researcher
zeph agents edit researcher
zeph agents delete researcher
```

## Sub-agent definition format

```yaml
---
name: researcher
description: Performs web research tasks
model: claude-sonnet-5
max_turns: 20
tools: [web_scrape, read_file]
skills: [research]
permission_mode: default   # default | accept_edits | dont_ask | bypass | plan

grants:
  tools: [web_scrape]
  secrets: [ZEPH_SEARCH_API_KEY]

hooks:
  pre_tool_use:
    - pattern: "web_scrape|fetch_url"
      command: "echo 'scraping: $TOOL_NAME'"
      timeout_secs: 5
      on_error: continue   # continue | abort
---

You are a research assistant. Use web_scrape to gather information.
Always cite your sources.
```

> [!IMPORTANT]
> The top-level frontmatter and its nested `tools:` and `permissions:` sections all reject unknown keys. A misspelled key (e.g. `pemission_mode:`, `alow:`) fails the definition load instead of silently falling back to defaults, so security-relevant `permission_mode`/`worktree` typos surface immediately.

## Zero-trust grants

Sub-agents receive only what is explicitly granted:

```rust
use zeph_subagent::{PermissionGrants, Grant, GrantKind};

let grants = PermissionGrants::builder()
    .tools(["web_scrape", "read_file"])
    .skills(["research"])
    .secrets(["ZEPH_SEARCH_API_KEY"])
    .build();
```

**Important:** Tools not in the grant list are inaccessible to the sub-agent even if they are globally available. Use `tools.except` in the definition to additionally deny specific tools from an inherited grant set.

Both `GrantKind::Tool` and `GrantKind::Secret` grants carry TTL and revocation state. `handle_tool_step` re-checks `PermissionGrants::check_tool_grant` immediately before every tool dispatch, so an expired or revoked tool grant is rejected with an actionable error at call time — grants are shared between `SubAgentManager` and the spawned agent-loop task via `Arc<Mutex<..>>`.

## Context propagation

Sub-agents inherit context from their parent agent to reduce cold-start latency:

- **History propagation** — the parent's recent conversation history is injected into the sub-agent's system prompt, giving it awareness of the ongoing task without requiring explicit re-briefing.
- **Cancellation propagation** — the parent's cancel signal is forwarded so that cancelling the parent also cancels running sub-agents.
- **Model inheritance** — when a sub-agent definition does not specify a model, it inherits the parent's active provider, avoiding unnecessary provider resolution overhead.

## Transcript persistence

Every sub-agent session is persisted as a JSONL transcript:

```bash
~/.local/share/zeph/transcripts/
    {agent-name}-{timestamp}-{id}.jsonl
    {agent-name}-{timestamp}-{id}.meta.json
```

Resume a previous session:

```text
/agent resume abc123 "continue where we left off"
```

`TranscriptReader` performs prefix-based lookup — partial IDs are resolved to the most recent matching session.

## Transcript integrity

Each appended `TranscriptEntry` carries a keyed-BLAKE3 hash-chain link (`chain`) binding its content to every prior entry in the file, so an in-place edit or partial field strip breaks verification on the next read (fail-closed). Chain verification is opt-in and off by default — it activates once a process configures a history-integrity key ring (`ZEPH_HISTORY_KEY` in the vault) via `configure_history_integrity`. Pre-feature transcripts with no `chain` field anywhere are treated as legacy and auto-trusted.

A per-file vault anchor (`{epoch, count, head, written_at}`, mirrored to the age vault on finalize) closes the residual gap where an attacker with file-write access strips every `chain` field, which would otherwise be indistinguishable from genuine legacy content. Controlled by the root `[integrity]` config section (`anchor = "vault"` by default, `"none"` to opt out); see `zeph-core`'s `anchor_store` module for the sweep that bounds vault growth.

> [!NOTE]
> Transcript integrity is process-global, configured once at bootstrap by the `zeph` binary — this crate never resolves vault keys itself (`zeph-subagent` has no `zeph-vault` dependency).

## Live transcript forwarding

Opt-in, per-turn forwarding of a running sub-agent's full text/thinking output to the TUI runtime detail view and/or a `--bare` stdout JSON sink, instead of only the once-per-turn status snippet or the blocking end-of-run result:

```toml
[agents]
forward_transcript = false   # default: false; also settable via --forward-subagent-text or ZEPH_AGENTS_FORWARD_TRANSCRIPT
```

Forwarding is structurally non-blocking on the sub-agent's own turn loop: `agent_loop.rs` does a non-blocking `try_send` of a `RawChunk` into a bounded per-task `mpsc` (capacity 128, tail-drop on full), and a manager-owned drain performs the one sanitize step before dispatching to whichever consumer surfaces (`ForwardSurfaces`) are active for the session.

## Delegation mode

Tri-state control over whether the main agent may spawn sub-agents, and who may trigger it (spec `042-subagent-delegation-mode-parity`, issue #5857):

```toml
[agents]
enabled = true               # outer kill switch: false always resolves to "disabled" below
delegation_mode = "proactive"  # "disabled" | "explicit_request_only" | "proactive" (default)
```

- `disabled` — no spawn from any code path (slash command, orchestration scheduler, `/subagent spawn`); read-only operations (`/agent list`) still work.
- `explicit_request_only` — only spawns attributable to a direct user action (`/agent spawn`, `/agent resume`, `/subagent spawn`) are permitted; the orchestration scheduler's autonomous DAG dispatch is rejected.
- `proactive` — both explicit and autonomous spawns are permitted, subject to the pre-existing `max_concurrent`/`max_spawn_depth`/`max_spawns_per_session`/permission-grant constraints. Matches the subsystem's behavior prior to this field's introduction.

Enforcement is fail-closed: every spawn is tagged with a `SpawnOrigin` (`Explicit` or `Autonomous`) on `SpawnContext`, and an untagged context defaults to `Autonomous` — the restrictive value — so a forgotten call site is denied under the restrictive modes rather than silently allowed. `SubAgentManager::spawn` (and `spawn_for_task`, which delegates to it) is the single chokepoint; a rejected spawn returns `SubAgentError::DelegationDenied` before any resource is allocated. Overridable via `ZEPH_AGENTS_DELEGATION_MODE` or `--delegation-mode`.

## Session-wide spawn cap

Independent of `max_concurrent` (in-flight limit) and `max_spawn_depth` (recursion limit), `max_spawns_per_session` bounds the *cumulative* number of sub-agents spawned over a session's lifetime — catching a shallow, low-concurrency but high-frequency sequential delegation loop that neither of those limits would (issue #6545):

```toml
[agents]
max_spawns_per_session = 100   # default: 100; 0 = unlimited
```

`SessionSpawnBudget` is a plain, uncloneable `AtomicUsize` counter — `SubAgentManager` owns the origin instance, `zeph-core`'s `OrchestrationState` owns an independent fallback used only when no manager is wired. `SubAgentManager::spawn`/`resume` and `zeph-core`'s ACP `/subagent spawn` chokepoint (which never touches `SubAgentManager` at all) reach whichever instance applies through an `Agent::session_budget()` accessor that hands out a `&SessionSpawnBudget` reference, so both paths enforce the exact same session-wide counter without ever copying it. Checked before `max_spawn_depth`/`max_concurrent`, but only *consumed* at each path's true commit point — a spawn rejected for any other reason (`NotFound`, a transient `ConcurrencyLimit` the orchestration scheduler will retry, a failed ACP process launch) never burns budget it never used. Reaching the cap returns `SubAgentError::SessionSpawnLimit`, whose `Display` names the config key directly. Resets at session start; never persisted.

## Features

| Feature | Description |
|---------|-------------|
| `sqlite` | SQLite backend forwarded to `zeph-durable`/`zeph-sanitizer`/`zeph-skills`/`zeph-tools` (enabled by `default`) |
| `postgres` | PostgreSQL backend forwarded to the same crates |

## Installation

```bash
cargo add zeph-subagent
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
