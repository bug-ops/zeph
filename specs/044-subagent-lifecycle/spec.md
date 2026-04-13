---
aliases:
  - Subagent Management
  - Subagent Lifecycle
  - Agent Spawning
tags:
  - sdd
  - spec
  - subagent
  - delegation
  - lifecycle
created: 2026-04-13
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[002-agent-loop/spec]]"
  - "[[010-security/spec]]"
  - "[[026-tui-subagent-management/spec]]"
  - "[[033-subagent-context-propagation/spec]]"
---

# Spec: Subagent Lifecycle (`zeph-subagent`)

> [!info]
> Full lifecycle of sub-agent tasks within the Zeph agent framework: definition parsing,
> spawning, concurrency management, permission grants, hooks, tool filtering, transcript
> persistence, and memory injection. Implements the `/agent` and `/agents` slash commands.

## 1. Overview

### Problem Statement

Delegating sub-tasks to isolated LLM sessions requires strict lifecycle management:
controlled spawning, bounded concurrency, cancellation propagation, TTL-bounded permission
grants, per-agent tool policies, and persistent transcripts for resume and audit. Without
a dedicated crate, this logic would accumulate in `zeph-core` and couple unrelated concerns.

### Goal

Provide `zeph-subagent` as the single source of truth for everything relating to sub-agent
lifecycle: parsing `SubAgentDef` files, spawning isolated agent loops, enforcing permission
grants and tool policies, firing lifecycle hooks, and persisting transcripts.

### Out of Scope

- TUI sidebar rendering for subagents (owned by `zeph-tui`, spec `026`)
- Context propagation details (spec `033` covers the gap analysis; this spec covers the full lifecycle)
- MCP server lifecycle (owned by `zeph-mcp`)
- A2A protocol (owned by `zeph-a2a`)

---

## 2. User Stories

### US-001: Spawning a Subagent

AS A parent agent loop
I WANT to spawn a named subagent definition with an initial prompt and parent context
SO THAT isolated work is delegated without blocking the parent.

**Acceptance criteria:**

```
GIVEN a valid SubAgentDef file and a SpawnContext with parent messages
WHEN SubAgentManager::spawn() is called
THEN a SubAgentHandle is returned with a unique task ID
AND the subagent runs in an isolated tokio task
AND parent_cancel propagation cancels the child when the parent is cancelled (foreground mode)
AND spawn_depth is incremented by 1
```

### US-002: Concurrency Limit

AS A system operator
I WANT to cap the number of concurrently running subagents
SO THAT runaway `/agent spawn` chains cannot exhaust system resources.

**Acceptance criteria:**

```
GIVEN the concurrency limit is set to N
WHEN N subagents are already running
AND another spawn is attempted
THEN spawn() returns Err(SubAgentError::ConcurrencyLimitExceeded)
```

### US-003: Permission Grants

AS A parent agent giving a subagent access to vault secrets or tools
I WANT TTL-bounded permission grants
SO THAT secrets are not exposed beyond the subagent's session lifetime.

**Acceptance criteria:**

```
GIVEN a Grant with kind=VaultSecret and a TTL of 300s
WHEN the grant expires (TTL elapses)
THEN subsequent calls to grants.check() return Err for that grant
AND memory is zeroized when the grant is dropped
```

### US-004: Tool Policy Enforcement

AS A subagent spawned with a restricted tool policy
I WANT the FilteredToolExecutor to block disallowed tool calls
SO THAT the subagent cannot exceed its declared permissions.

**Acceptance criteria:**

```
GIVEN a SubAgentDef with tool_policy = "readonly"
WHEN the subagent calls the "shell" write tool
THEN FilteredToolExecutor rejects the call with a permission-denied error
AND the rejection is logged at WARN level
```

### US-005: Transcript Persistence

AS A user or operator reviewing past subagent sessions
I WANT subagent conversations persisted to JSONL transcript files
SO THAT I can inspect what the subagent did and resume interrupted sessions.

**Acceptance criteria:**

```
GIVEN a subagent session that completes or is cancelled
WHEN the session ends
THEN a JSONL transcript file exists with one JSON line per turn
AND TranscriptMeta records start time, end time, and exit reason
AND sweep_old_transcripts() removes transcripts beyond the retention limit
```

### US-006: Lifecycle Hooks

AS A developer configuring subagent behavior
I WANT to define shell commands that run at PreToolUse, PostToolUse, SubagentStart, and SubagentStop
SO THAT external integrations can react to subagent lifecycle events.

**Acceptance criteria:**

```
GIVEN a hook definition with type = "SubagentStart" and a shell command
WHEN a subagent starts
THEN the shell command is executed with the subagent name in the environment
AND hook execution failures are logged at WARN but do not abort the session
```

### US-007: Memory Injection

AS A subagent starting a new session
I WANT persistent `MEMORY.md` content injected into my system prompt
SO THAT cross-session knowledge is available without explicit retrieval.

**Acceptance criteria:**

```
GIVEN a MEMORY.md file in the subagent's memory directory
WHEN the subagent's system prompt is assembled
THEN the memory content is prepended to the system prompt
AND memory content exceeding the token budget is truncated, not omitted entirely
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `SubAgentDef::parse()` receives a Markdown file with YAML frontmatter THEN the system SHALL extract name, description, system prompt, permissions, and hooks | must |
| FR-002 | WHEN an agent name fails the regex `^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$` THEN the system SHALL reject the definition with `SubAgentError::InvalidName` | must |
| FR-003 | WHEN `SubAgentDef::load()` is called THEN the system SHALL enforce the 256 KiB file size limit and reject oversized files | must |
| FR-004 | WHEN `SubAgentDef::load_all()` scans directories THEN the system SHALL process files in priority order and cap per-directory scans at the configured limit | must |
| FR-005 | WHEN `SubAgentManager::spawn()` is called at the concurrency limit THEN the system SHALL return `Err(ConcurrencyLimitExceeded)` | must |
| FR-006 | WHEN a subagent is spawned with `parent_cancel` THEN cancelling the parent token SHALL cancel the child's `CancellationToken` | must |
| FR-007 | WHEN a `Grant` TTL expires THEN `PermissionGrants::check()` SHALL return an error for that grant | must |
| FR-008 | WHEN `FilteredToolExecutor` receives a tool call THEN it SHALL check the `ToolPolicy` and denylist before forwarding to the real executor | must |
| FR-009 | WHEN a subagent session ends (normally or via cancellation) THEN a JSONL transcript SHALL be written with complete turn history | must |
| FR-010 | WHEN `sweep_old_transcripts()` is called THEN transcripts beyond the retention window SHALL be deleted | must |
| FR-011 | WHEN lifecycle hooks are defined THEN `fire_hooks()` SHALL execute matching hooks for each `HookType` event | must |
| FR-012 | WHEN hook execution fails THEN the failure SHALL be logged at `WARN` and the subagent session SHALL continue | must |
| FR-013 | WHEN `load_memory_content()` is called THEN it SHALL read `MEMORY.md` from the resolved memory directory and return its content | should |
| FR-014 | WHEN `AgentCommand` or `AgentsCommand` is parsed from user input THEN it SHALL map to a typed command variant (`spawn`, `list`, `cancel`, `resume`, `show`) | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Security | Agent names must be ASCII-only with the validated regex; path traversal characters are rejected |
| NFR-002 | Security | Definition files are size-capped at 256 KiB before parsing |
| NFR-003 | Security | `PermissionGrants` must be TTL-bounded; no grant may outlive its declared expiry |
| NFR-004 | Concurrency | `SubAgentManager` must enforce the concurrency cap atomically (no TOCTOU) |
| NFR-005 | Isolation | Subagent tool executors are independent instances; they do not share state with the parent |
| NFR-006 | Persistence | Transcripts must be written atomically (write to temp file, then rename) |
| NFR-007 | Safety | No `unsafe` code |

---

## 5. Data Model

| Entity | Description | Key Attributes |
|--------|-------------|----------------|
| `SubAgentDef` | Parsed subagent definition | `name`, `description`, system prompt body, `permissions: SubAgentPermissions`, `hooks: SubagentHooks` |
| `SubAgentPermissions` | Permission set for a subagent | `tool_policy: ToolPolicy`, `skill_filter: SkillFilter`, `memory_scope: MemoryScope`, `permission_mode: PermissionMode` |
| `SubAgentManager` | Lifecycle manager | Concurrency limit, active handles map, cancellation registry |
| `SubAgentHandle` | Reference to a running task | Task ID (UUID), status channel, cancellation token |
| `SubAgentStatus` | Current state of a task | Variants: `Running`, `Completed`, `Failed`, `Cancelled` |
| `SpawnContext` | Parent-derived spawn state | `parent_messages`, `parent_cancel`, `parent_provider_name`, `spawn_depth`, `mcp_tool_names` |
| `PermissionGrants` | TTL-bounded permission registry | Map of `GrantKind` → expiry timestamp |
| `Grant` | Single permission grant | `kind: GrantKind`, `ttl_secs`, expiry instant |
| `GrantKind` | Type of permission | Variants: `VaultSecret`, `Tool` |
| `FilteredToolExecutor` | Tool executor with policy gate | Wraps real executor; enforces `ToolPolicy` and denylist |
| `PlanModeExecutor` | Executor for plan mode | Wraps real executor; disables write operations |
| `HookDef` | Lifecycle hook definition | `hook_type: HookType`, shell command template |
| `HookType` | Lifecycle event | `PreToolUse`, `PostToolUse`, `SubagentStart`, `SubagentStop` |
| `HookMatcher` | Pattern for hook selection | Glob or regex pattern on tool name / agent name |
| `TranscriptWriter` | Append-only JSONL writer | Session ID, file path, turn counter |
| `TranscriptReader` | Replay reader for transcripts | Iterates JSONL lines as `Message` |
| `TranscriptMeta` | Session metadata record | Start time, end time, exit reason, turn count |
| `SubAgentState` | Mutable runtime state | Active provider, current conversation, tool executor reference |

---

## 6. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Definition file contains TOML frontmatter (deprecated) | Parsed with a deprecation warning in logs |
| Agent name contains path traversal (e.g., `../etc`) | `is_valid_agent_name()` returns false; load rejected |
| Spawn depth exceeds configured maximum | `spawn()` returns `Err(MaxDepthExceeded)` |
| Transcript write fails (disk full) | Error logged at `ERROR`; session continues; partial transcript preserved |
| Hook shell command not found | Hook fails; logged at `WARN`; session continues |
| Grant checked after TTL expiry | Returns `Err`; no panic |
| Subagent cancelled mid-turn | Tool in progress receives cancellation signal; transcript records `Cancelled` exit reason |
| `load_all()` encounters symlink outside allowed boundary | File is skipped with a security warning in logs |

---

## 7. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | Name validation | Unit tests cover valid names, path traversal, unicode homoglyphs, empty, too-long |
| SC-002 | Concurrency cap | Integration test spawns N+1 agents and confirms Nth+1 is rejected |
| SC-003 | TTL expiry | Unit test advances mock clock past TTL and confirms grant check fails |
| SC-004 | Tool policy | Unit test confirms disallowed tool is rejected by `FilteredToolExecutor` |
| SC-005 | Transcript completeness | Integration test verifies all turns appear in JSONL after session ends |

---

## 8. Agent Boundaries

### Always (without asking)
- Run `cargo nextest run -p zeph-subagent` after changes
- Validate agent names with `is_valid_agent_name()` before any file I/O

### Ask First
- Adding new `HookType` variants (affects all callers that pattern-match)
- Changing `SubAgentPermissions` fields (affects definition file format)
- Raising the default concurrency limit (resource impact)
- Adding new dependencies to `zeph-subagent`

### Never
- Allow agent names containing path separators without validation
- Grant permissions beyond the declaring `SubAgentDef`'s declared policy
- Bypass the size limit on definition file loading
- Use `unsafe` blocks

---

## 9. Open Questions

None.

---

## 10. See Also

- [[constitution]] — project principles
- [[002-agent-loop/spec]] — parent agent loop that uses `SubAgentManager`
- [[010-security/spec]] — security model; agent name validation and grant TTLs are security controls
- [[026-tui-subagent-management/spec]] — TUI sidebar that displays running subagents
- [[033-subagent-context-propagation/spec]] — context propagation spec (shipped v0.18+)
- [[MOC-specs]] — all specifications
