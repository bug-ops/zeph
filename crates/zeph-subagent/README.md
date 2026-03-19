# zeph-subagent

Subagent management crate for Zeph: spawning, grants, transcripts, filtering, and lifecycle hooks.

Extracted from `zeph-core` as part of epic #1973 (Phase 1f).

## Modules

- `command` — CLI commands for subagent management (`/agent`, `/agents`)
- `def` — `SubAgentDef` type (agent definition and metadata)
- `error` — `SubAgentError` (typed errors via thiserror)
- `filter` — `FilteredToolExecutor`, `PlanModeExecutor`, skill filtering
- `grants` — `Grant`, `GrantKind`, `PermissionGrants`, `SecretRequest`
- `hooks` — lifecycle hooks (`PreToolUse`, `PostToolUse`, `SubagentStart`, `SubagentStop`)
- `manager` — `SubAgentManager` (spawn, monitor, communicate)
- `memory` — memory bindings for subagent context
- `resolve` — agent definition discovery and path resolution
- `state` — `SubAgentState` (persistent state tracking)
- `transcript` — `TranscriptWriter`, `TranscriptReader` (JSONL-backed history)
