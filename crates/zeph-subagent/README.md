# zeph-subagent

[![Crates.io](https://img.shields.io/crates/v/zeph-subagent)](https://crates.io/crates/zeph-subagent)
[![docs.rs](https://img.shields.io/docsrs/zeph-subagent)](https://docs.rs/zeph-subagent)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

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
| `transcript` | `TranscriptWriter`, `TranscriptReader` — JSONL-backed history with `.meta.json` sidecars; prefix-based ID lookup; resume-by-ID |
| `memory` | `MemoryScope` — `User`/`Project`/`Local`; memory directory lifecycle; injection into sub-agent system prompt |
| `state` | `SubAgentState` — `Submitted`/`Working`/`Completed`/`Failed`/`Canceled` |
| `resolve` | Definition discovery and 4-level priority resolution (CLI > project > user > config) |
| `command` | `AgentsCommand` enum driving `/agent` and `zeph agents` CLI subcommands |

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
model: claude-sonnet-4-6
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

## Installation

```bash
cargo add zeph-subagent
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
