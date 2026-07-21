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
  - "[[039-background-task-supervisor/spec]]"
  - "[[047-cli-modes/spec]]"
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
GIVEN a Grant with kind=Secret(key) and a TTL of 300s
WHEN the grant expires (TTL elapses)
THEN subsequent calls to grants.check() return Err for that grant
AND memory is zeroized when the grant is dropped
```

`GrantKind::Tool(tool_name)` grants are TTL-tracked identically but are a separate,
narrower mechanism — see Section 17 for its distinct default-permit, time-box-only
enforcement semantics, which differ from `Secret`'s fail-closed check.

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
| FR-008 | WHEN `FilteredToolExecutor` receives a tool call THEN it SHALL check the `ToolPolicy` and denylist before forwarding to the real executor, matching tool IDs case-insensitively after stripping argument suffixes (`"Bash(cargo *)"` → `"bash"`) via `normalize_tool_id` (#3765) | must |
| FR-009 | WHEN a subagent session ends (normally or via cancellation) THEN a JSONL transcript SHALL be written with complete turn history | must |
| FR-010 | WHEN `sweep_old_transcripts()` is called THEN transcripts beyond the retention window SHALL be deleted | must |
| FR-011 | WHEN lifecycle hooks are defined THEN `fire_hooks()` SHALL execute matching hooks for each `HookType` event | must |
| FR-012 | WHEN hook execution fails THEN the failure SHALL be logged at `WARN` and the subagent session SHALL continue | must |
| FR-013 | WHEN `load_memory_content()` is called THEN it SHALL read `MEMORY.md` from the resolved memory directory and return its content | should |
| FR-014 | WHEN `AgentCommand` or `AgentsCommand` is parsed from user input THEN it SHALL map to a typed command variant (`spawn`, `list`, `cancel`, `resume`, `show`) | must |
| FR-015 | WHEN a subagent is spawned with `memory: user` in its definition THEN the system SHALL wrap the tool executor with `MemoryAwareExecutor`, which retries `SandboxViolation` file-tool calls against a `FileExecutor` scoped to `~/.zeph/agent-memory/<agent-name>/` (#3771) | must |
| FR-016 | WHEN `MemoryAwareExecutor` resolves the memory directory THEN it SHALL canonicalize the path and reject any resolved path that escapes `~/.zeph/agent-memory/<agent-name>/` to prevent traversal attacks (#3771) | must |

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
| `SpawnContext` | Parent-derived spawn state | `parent_messages`, `parent_cancel`, `parent_provider_name`, `spawn_depth`, `mcp_tool_names`, `max_trust_level`, `inherited_tool_allowlist` |
| `PermissionGrants` | TTL-bounded permission registry | Map of `GrantKind` → expiry timestamp |
| `Grant` | Single permission grant | `kind: GrantKind`, `ttl_secs`, expiry instant |
| `GrantKind` | Type of permission | Variants: `Secret(String)` (vault key), `Tool(String)` (tool name, #6625) |
| `FilteredToolExecutor` | Tool executor with policy gate | Wraps real executor; enforces `ToolPolicy` and denylist; tool ID comparison is case-insensitive and strips argument suffixes via `normalize_tool_id` |
| `MemoryAwareExecutor` | Sandbox-bypass executor for `memory: user` subagents | Wraps inner executor; retries `SandboxViolation` file-tool calls against a `FileExecutor` scoped to `~/.zeph/agent-memory/<name>/`; path canonicalization delegated to `FileExecutor` to prevent traversal (#3771) |
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
| Subagent with `memory: user` writes to a file outside `~/.zeph/agent-memory/<name>/` | `MemoryAwareExecutor` rejects the call; the canonicalized path does not start with the allowed prefix |
| Subagent name contains path traversal components in memory path construction | `MemoryAwareExecutor` validates the agent name via `is_valid_agent_name()` before constructing the memory path |

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

## 9. Orchestrator Identity Fields (#4032)

`SpawnContext` gains orchestrator identity fields so spawned subagents can identify the
parent orchestrator in their system prompt header.

| Field | Type | Description |
|-------|------|-------------|
| `orchestrator_name` | `Option<String>` | Name of the parent agent or orchestrator |
| `orchestrator_role` | `Option<String>` | Role string (e.g., `"coordinator"`, `"supervisor"`) |

When `orchestrator_role` is `None`, the spawned subagent's orchestrator header omits the
role clause entirely (no dangling pronoun). This fixes a grammar issue where the header
read "from ()" when role was absent (#4070).

### Key Invariants

- Both fields are optional — absent = header omits the field without breaking formatting
- `orchestrator_name` and `orchestrator_role` are injected into the subagent's system prompt only; not stored in the transcript

---

## 10. MCP Config Inheritance on Respawn (#4342)

When a subagent is respawned (e.g., after crash or explicit restart), the parent's MCP
config is re-inherited automatically. Previously, the MCP server list was captured at
initial spawn time and lost on respawn.

### Mechanism

`SpawnContext::mcp_tool_names` is updated from the live parent `McpManager` reference
at respawn time, not from a stale snapshot.

### Key Invariants

- Respawned subagents MUST inherit the parent's current MCP config, not the config at initial spawn
- MCP tool names list is never cached in `SubAgentDef` — always fetched live from parent manager

---

## 11. Worktree Teardown Safety (#4342)

When a subagent operating in a git worktree is stopped, the teardown sequence checks
whether the worktree is still referenced by any other active subagent before removing it.

- Teardown is blocked if another agent has the worktree as its cwd
- Teardown proceeds if no active agents reference the worktree
- `git worktree remove` is invoked only if both conditions hold: worktree is not referenced AND the path matches the `worktrees/` prefix

### Key Invariants

- NEVER remove a worktree that is still in use by an active subagent
- NEVER remove a path that was not created by Zeph (must match known prefix pattern)

---

## 12. Transitive Constraint Propagation (#4681, #4690, #4693, #4694)

Addresses constraint drift (arXiv:2605.10481): safety constraints set at orchestration time
were silently dropped when a subagent spawned its own subagents, allowing trust-level and
tool-allowlist escalation deep in delegation chains.

### New `SpawnContext` Fields

| Field | Type | Description |
|-------|------|-------------|
| `max_trust_level` | `Option<SkillTrustLevel>` | Maximum trust level allowed for skills invoked by this subagent or its children |
| `inherited_tool_allowlist` | `Option<HashSet<String>>` | Tool allowlist inherited from parent; used to restrict what the child may be granted |

### `apply_constraint_propagation(def, ctx)` — `zeph-subagent`

Called during `spawn()` and `resume()` before building the `FilteredToolExecutor`:

1. **Trust clamping**: executor trust is set to `min(def.trust_level, ctx.max_trust_level)` — narrows only, never raises.
2. **Allowlist intersection**: if `ctx.inherited_tool_allowlist` is `Some(parent_set)`:
   - `AllowList(child_set)` → `AllowList(child_set ∩ parent_set)`
   - `InheritAll` → `AllowList(parent_set)` (parent set becomes the effective allowlist)
   - `DenyList(deny_entries)` → `AllowList(parent_set \ deny_entries)` (fail-closed conversion)
3. Constraint narrowing is logged at `info` level for auditability.

The propagated fields are passed transitively: when the newly spawned agent itself spawns
children, it sets `max_trust_level` and `inherited_tool_allowlist` from its own (already-clamped)
constraints.

### Key Invariants

- Constraint propagation MUST run in both `spawn()` and `resume()` — applying it in spawn only is incomplete
- Propagation MUST be transitive: grandchild constraints are bounded by the grandparent's, not just the parent's
- NEVER raise trust level via propagation — `min()` is the only allowed operation on `max_trust_level`
- `InheritAll` tool policy with a non-None parent allowlist MUST be converted to `AllowList(parent_set)` — `InheritAll` must not survive into a constrained delegation chain
- Constraint narrowing MUST be logged — silent narrowing is a security observability gap

### Producers of `inherited_tool_allowlist` (#6526, #6527)

Section 12 above describes the *consumer* (`apply_constraint_propagation`), which existed
before either producer did — until #6526/#6527, `inherited_tool_allowlist` was always `None`
on every production `SpawnContext`. There are now two producers, composed by intersection
via `zeph_subagent::intersect_allowlists` (`None, None → None`; one `Some` → that set; both
`Some` → their intersection — never widens):

1. **Parent-derived floor** (#6527) — `PermissionPolicy::effective_tool_allowlist`
   (`zeph-tools/src/permissions.rs`) derives a narrowed set from the parent session's own
   `[tool.permissions]` rules: a tool is dropped when its first matching rule (first-match-wins,
   mirroring `PermissionPolicy::check`) is a catch-all `Deny` (`""`, `"*"`, or `"**"`). Returns
   `None` (no narrowing) when `autonomy_level` is not `Supervised`, or when no tool is actually
   wholesale-denied — returning `Some(full universe)` in the latter case would incorrectly
   freeze an `InheritAll` child's tool list, hiding dynamically-added MCP tools. Wired into
   `build_spawn_context` (`zeph-core/src/agent/subagent_commands.rs`), so it covers every
   spawn path: interactive `/agent`, `/agent resume`, and orchestrated spawns.
2. **Per-task narrowing** (#6526) — `TaskNode.tool_allowlist: Option<Vec<String>>`
   (`zeph-orchestration/src/graph.rs`), populated by the LLM planner via
   `PlannedTask.tool_allowlist`. Intersected into the spawn context in
   `handle_scheduler_spawn_action` (`zeph-core/src/agent/scheduler_loop.rs`) — orchestrated
   spawns only, since interactive spawns have no `TaskNode`. Planner-emitted names that do not
   match a real tool in the spawn-time tool universe are dropped (with a `tracing::warn!`)
   rather than intersecting to a zero-tool allowlist, so a hallucinated/typo'd name degrades to
   a no-op instead of failing the task.

**Security framing — defense-in-depth, not the primary boundary:** neither producer is the
runtime security boundary. A spawned sub-agent's tool executor is
`Arc::clone(&self.tool_executor)` — the SAME `TrustGateExecutor`-gated tree the parent itself
uses (see `agent_setup.rs`'s `TrustGateExecutor::new(inner, permission_policy.clone())` and
`runner.rs`'s `self.tool_executor` wiring) — wrapped by a `FilteredToolExecutor`. So every
child tool call is re-checked against the parent's own `PermissionPolicy` at call time,
regardless of what `inherited_tool_allowlist` narrows. These two producers only control what
the child's LLM *sees* in its tool catalog: hygiene and wasted-turn avoidance, not a new
access-control layer. This coupling is load-bearing — if a future refactor gives subagents a
fresh/ungated executor, the `None` returns from `effective_tool_allowlist` (for `ReadOnly`
autonomy and for "nothing wholesale-denied") become real escalation holes; see the invariant
comment at the `build_spawn_context` call site.

---

## 13. Open Questions

None.

---

## 14. Live Transcript Forwarding (issue #6359)

### Problem

The pre-existing subagent status surface (TUI sidebar, `--bare` status lines) exposes only a
120-char once-per-turn snippet or the blocking end-of-run result — an operator watching a
long-running sub-agent cannot see its actual per-turn text/thinking output as it is produced.

### Mechanism

Opt-in (`agents.forward_transcript = true`, env `ZEPH_AGENTS_FORWARD_TRANSCRIPT`, CLI
`--forward-subagent-text`; default `false`, zero behavioral change when disabled or when no
consumer surface — `--tui` and/or `--bare` — is active). When enabled, each running sub-agent's
full, untruncated per-turn text/thinking output is forwarded the moment a turn's LLM response
arrives:

1. Each sub-agent turn loop (`crates/zeph-subagent/src/agent_loop.rs`) pushes a `RawChunk` into a
   per-task `mpsc` ingress channel — one channel per spawned task, never a shared broadcast.
2. A manager-owned drain (`crates/zeph-subagent/src/forward.rs`) consumes each task's channel and
   sanitizes every chunk through the same layers already applied to the analogous sub-agent
   debug-dump/outbound-LLM egress paths: the baseline `ContentSanitizer` pass plus, when
   configured, `SecretMaskRegistry` and `PiiFilter`. A `RawChunk`→`SanitizedChunk` typestate
   structurally enforces that no unsanitized chunk can reach a consumer.
3. Sanitized chunks dispatch to whichever consumer surface is active:
   - TUI: a bounded ring buffer per subagent, surfaced in the runtime subagent detail view
     (`SubAgentMetrics::live_transcript`) — see `[[026-tui-subagent-management/spec]]`.
   - `--bare`: JSON lines on stdout — see `[[047-cli-modes/spec]]`.

### Key Invariants

- Structurally non-blocking on the sub-agent's own turn loop: the per-task ingress uses
  `try_send`, dropping on a full channel with a per-task drop counter, never blocking the
  producing turn.
- Routed through `zeph_common::TaskSupervisor` (per CLAUDE.md's Async & Background Tasks
  contract) — never an untracked `tokio::spawn`.
- Forwarded content passes through the same sanitization chain as debug-dump/egress paths before
  reaching any consumer — no unsanitized path exists.
- Default `false`; enabling it changes only what is forwarded, never the underlying turn loop's
  control flow or result.

### Token-Level Intra-Turn Streaming (issue #6456)

Superseding the once-per-turn-only limitation above: when the active provider's native
streaming-with-tools path is available (`AnyProvider::chat_with_tools_stream`, driven from
`agent_loop.rs`), text/thinking chunks are forwarded as partial deltas *within* a turn rather
than only once the turn completes. Non-streaming backends, or a streaming attempt that fails,
keep the prior once-per-turn forwarding unchanged — both delivery modes share the same
`ForwardSender::send_text`/`send_thinking` API and are tail-dropped identically under
backpressure. A caller must never send both the streamed deltas and the final whole-turn text
for the same turn (that would double-forward the same content).

**Split-pattern masking gap and its fix**: sanitizing each streamed delta in isolation let a
secret or PII pattern split across two delta boundaries evade both fragments' individual checks.
The forwarding drain now holds back the trailing `SANITIZE_HOLDBACK_BYTES` (256) bytes of each
pending `Text`/`Thinking` buffer — rounded down to the nearest UTF-8 char boundary — before
sanitizing and releasing the safe prefix; the full remaining buffer is flushed with zero
holdback immediately before an explicit `Terminal` chunk or the hard-abort backstop, so buffered
content is only ever delayed, never silently dropped. A secret/PII pattern whose split halves
are separated by more than 256 bytes of other already-flushed content remains a residual,
practically-unreachable limitation of any bounded-window approach.

**Design contract — deltas are ephemeral, display-only**: no consumer (TUI ring buffer, `--bare`
sink, or any future sink) may reconstruct the subagent's conversational state from concatenated
forwarded chunks, let alone feed it back into the parent's LLM context. The loop's own
accumulated response text (returned from `run_agent_loop` and pushed into `messages`) is
assembled independently of whether any given delta was actually forwarded; only the guaranteed
terminal chunk marks the point at which a consumer may treat the run as having reached a
terminal state.

### Known Limitation

Combining `--bare` with `--json` interleaves two different JSON schemas on stdout — unsupported
for now; use `--bare` without `--json` for scripted-pipeline forwarding.

---

## 15. Delegation Mode Gate (issue #5857)

> [!note] Broken cross-reference — 2026-07 audit
> This section previously cited a companion spec `042-subagent-delegation-mode-parity`. No such
> spec exists in `/specs/` — `042` is already assigned to
> [[042-zeph-commands/spec|Slash Command Registry]], and no directory matching
> `*-subagent-delegation-mode-parity` exists anywhere under `/specs/`. Either the companion spec
> was never created or was misnumbered at authoring time. The motivation is summarized inline
> below instead of via a dangling link; if a dedicated spec for delegation-mode parity across
> subsystems is still wanted, it should be filed as a new numbered spec (follow-up, not created in
> this corrective pass).

### Problem

Prior to this addition, `SubAgentConfig.enabled: bool` was the only lever governing sub-agent
spawning, and it was **inert** — no code path actually read it. An operator had no way to keep
the sub-agent subsystem enabled and useful while forbidding the main agent from autonomously
deciding to spawn one (e.g. in a semi-trusted channel where prompt-injected input could reach
the agent).

### Mechanism

`SubAgentConfig` gains `delegation_mode: DelegationMode` (`disabled` / `explicit_request_only` /
`proactive`, `#[serde(default)]` → `Proactive`, preserving prior unconstrained behavior). Every
spawn attempt carries a `SpawnOrigin` (`Explicit` / `Autonomous`) on `SpawnContext`, enforced at
the top of `SubAgentManager::spawn` (the single chokepoint — `spawn_for_task` delegates to it,
so both share the gate automatically):

- `disabled` rejects every spawn regardless of origin (read-only ops — `/agent list`, status
  queries — are unaffected, since they never call `spawn`).
- `explicit_request_only` rejects `Autonomous`-origin spawns, permits `Explicit`.
- `proactive` permits both, unchanged from pre-existing behavior.

**Fail-closed default**: `SpawnOrigin::default() = Autonomous` — the *restrictive* value, not
the permissive one. An untagged or forgotten `SpawnContext` is therefore denied under the
restrictive modes rather than silently allowed. Every real spawn call site was audited and
explicitly tagged:

- `build_spawn_context` (`crates/zeph-core/src/agent/subagent_commands.rs`, shared by
  `/agent spawn` foreground/background and `/agent resume`) sets `Explicit` — all three of its
  callers are dispatched from the explicit `/agent` slash command.
- `handle_scheduler_spawn_action` (`crates/zeph-core/src/agent/scheduler_loop.rs`) overrides
  `spawn_ctx.origin = Autonomous` immediately after calling `build_spawn_context`, mirroring the
  existing post-construction override pattern already used for `network_denied`/`progress_at`.
  This is the orchestration scheduler's autonomous DAG dispatch — the concrete threat this gate
  protects against, since it spawns without any turn-level user confirmation.

`SubAgentConfig.enabled` becomes the outer kill switch via
`SubAgentConfig::effective_delegation_mode()`: `enabled = false` always resolves to `Disabled`
regardless of `delegation_mode`'s configured value. `src/runner.rs` bootstrap calls
`mgr.set_delegation_mode(agents_config.effective_delegation_mode())` once, before any spawn can
occur; the manager itself never re-reads `enabled`.

A rejected spawn returns `SubAgentError::DelegationDenied { mode, origin, def_name }` before any
resource is allocated (no worktree, no transcript file, no consumed concurrency slot), and logs
a `tracing::warn!` distinguishable from `ConcurrencyLimit`/`MaxDepthExceeded` rejections.

### ACP `/subagent spawn` — a separate gate, not a `SpawnContext` tag

`/subagent spawn <cmd>` (`crates/zeph-core/src/agent/slash_commands.rs::handle_subagent_slash`)
launches an **external ACP process** via `zeph_acp::run_session` and never constructs a
`SpawnContext` or touches `SubAgentManager` at all — the gate above does not see it. Spec 042
FR-003 requires `disabled` mode to reject every spawn path, so `handle_subagent_slash` carries
its own explicit check against `SubAgentConfig::effective_delegation_mode()` before invoking the
spawn callback, rejecting only under `Disabled` (the command is itself an explicit user action,
so it remains permitted under `explicit_request_only` and `proactive`).

### Key Invariants

- The gate check runs before any resource allocation (NFR-002) — no partial worktree, transcript,
  or concurrency-slot side effect on a rejected spawn.
- `SpawnOrigin`'s fail-closed default means a forgotten call site is a visible, safe denial under
  restrictive modes, never a silent bypass.
- `delegation_mode` is orthogonal to `default_permission_mode`/`PermissionMode` — the former
  governs *whether* a spawn may happen and *who* may trigger it; the latter governs what a
  spawned sub-agent is allowed to *do* once running. Never merge the two.
- Out of scope for this addition (deferred, see spec 042 Open Questions): per-turn/per-session
  override of `delegation_mode` (FR-011); a dedicated TUI status-bar/sidebar indicator beyond
  the `/agent list` header line (NFR-004 partial).

---

## 16. Secret-Leak, Trust-Cap, and Transcript-Finalize Wiring (#6503: issues #6492, #6493, #6494)

Three trust-boundary gaps in the grant/spawn/transcript machinery, each closed by wiring
already-existing enforcement code to an actual call site — the enforcement logic itself
predated this fix in every case; only the connection was missing.

### 1. Secret-leak on tool echo (#6492)

A granted vault secret echoed back verbatim by a tool (e.g. `shell: echo $SOME_VAULT_KEY`)
could previously reach the transcript, the next turn's LLM context, and the debug dump
unmasked. `SubAgentManager::deliver_secret` now registers the delivered value into the shared
`SecretMaskRegistry` (the same registry instance already used for the parent's outbound-LLM
masking and the forward-transcript drain's `SanitizeLayers`, Section 14) at delivery time, not
only on eventual read. The subagent loop's `handle_tool_step` masks every tool-result `content`
against this registry (`would_mask` pre-check, then `mask`) immediately before it is pushed
into `messages` — the single chokepoint feeding the transcript, the following turn's LLM
context, and the debug dump. Contingent on `secret_masking.enabled` (default `true`; the
registry is `None` when disabled) and on the secret being at least `MIN_SECRET_LEN` (8) bytes.

### 2. Trust-cap wiring (#6493)

Section 12's `apply_constraint_propagation` clamps `SpawnContext::max_trust_level` correctly,
but `build_spawn_context` — the only production call site constructing a top-level
`SpawnContext` — left the field `None` unconditionally, so the clamp had nothing to clamp
against and a spawned sub-agent's trust was never actually capped despite the enforcement
machinery existing. `build_spawn_context` now sets `max_trust_level` from a new
`parent_effective_trust_level()`, which folds `min_trust` across the trust levels of every
currently-active skill this turn (yielding `Trusted` when no skill is active) — mirroring
exactly the fold `apply_skill_trust_and_gating` applies to the parent's own tool gate, so the
cap handed to a spawned sub-agent is always consistent with what the parent itself enforces on
its own turn. Because `build_spawn_context` is the single top-level construction site, every
spawn path reached through it (foreground, background, `/agent resume`) is covered.

### 3. Finalize-skip on turn error (#6494)

`run_agent_loop`'s transcript anchor `finalize()` call (issue #6449) was reached only on the
loop's normal-completion or explicit-`break` exit paths — a `run_turn` error propagated via an
early `?`-return skipped it entirely, leaving that transcript chained-but-unanchored and
reopening the whole-strip downgrade issues #6449/#6461 had closed. A `run_turn` error is now
captured into a `pending_error` local and the loop `break`s instead of early-returning;
`finalize()` then runs unconditionally on every loop-exit path (including the LLM-error path),
and the captured error is only re-raised to the caller after `finalize()` has run.
`publish_completed_status` is skipped whenever `pending_error.is_some()`, since
`call_provider_with_status` already published the terminal `Failed` status (and its matching
forwarded terminal chunk, Section 14) before returning the error — publishing `Completed`
afterward would overwrite that status and double-forward a terminal chunk.

### Key Invariants

- A delivered secret MUST be registered into the shared mask registry before it can reach any
  tool-result content — masking a registry that was never populated with the secret it is
  supposed to catch is equivalent to no masking at all.
- `max_trust_level` MUST be populated from the parent's own current effective trust at every
  top-level spawn call site — an unpopulated (`None`) field makes Section 12's clamp a no-op,
  not an error, so this is a silent-bypass class of gap distinct from a panic or rejected spawn.
- Transcript `finalize()` MUST run on every `run_agent_loop` exit path, including an LLM/tool
  error — skipping it on any path reopens the anchor-chain downgrade #6449/#6461 closed.

---

## 17. Tool Grant TTL Enforcement Before Dispatch (issue #6625)

### Problem

`GrantKind::Tool` grants were TTL-tracked in `PermissionGrants` identically to
`GrantKind::Secret` grants (Section 3, US-003), but prior to this fix no production call site
ever checked a `Tool` grant before dispatching the corresponding tool call — the type implied
enforcement that did not exist. A `Tool` grant could expire or be revoked with no observable
effect on subsequent dispatch.

### Mechanism

`PermissionGrants` is now shared via `Arc<Mutex<_>>` between `SubAgentHandle` and the spawned
agent-loop task. This differs from how `Secret` grants are delivered (per-key over a channel):
`Tool` grants need live shared state instead, since `revoke_all()` must be visible to the loop
immediately, without a delivery round-trip. `PermissionGrants::check_tool_grant(tool_name)`
returns a `ToolGrantCheck` (`NoGrant` / `Active` / `Expired`) and is enforced as the *first*
check in `handle_tool_step` — before any hook fires or the executor runs. A loop-local
`active_tool_grants_seen` set distinguishes "revoked" from "never granted" after
`sweep_expired()` evicts a stale entry — without it, a rejected dispatch would silently permit
the very next call for the same tool, since the evicted grant would then read as `NoGrant`.

### Confirmed Semantics — default-permit, time-box only, NOT an allow-list

- Absence of any `Tool` grant for a given tool name always permits the call — this is the
  universal state for every production caller today, since none yet creates `GrantKind::Tool`
  grants, so this fix causes no observable behavior change until a caller starts issuing them.
- A grant existing for one tool name never restricts any other tool name, even for the same
  sub-agent — there is no implicit "some grants exist, so everything else is denied" mode.
- Matching is exact tool-name string equality only — no prefix/glob support for tool families
  (e.g. an MCP server-scoped grant does not cover sub-tool names under that server).

### Key Invariants

- `check_tool_grant` MUST run before any hook fires or the executor runs — enforcing after
  either would let a revoked/expired grant's side effects occur regardless of the check result.
- `Tool` grants MUST NEVER be reinterpreted as an allow-list — `NoGrant` always means
  "unrestricted by this mechanism," never "denied." Extending this into allow-list semantics
  requires a deliberate, separately-specified design change, not an incidental generalization.
- A grant evicted by `sweep_expired()` MUST still read as `Expired` (fail-closed) rather than
  `NoGrant` (fail-open) on the same tool for the remainder of the run — the loop-local
  `active_tool_grants_seen` set is what makes this distinction possible.

---

## 18. Strict Nested Frontmatter Parsing (issue #6626) — BREAKING CHANGE

### Problem

`RawSubAgentDef` carries `#[serde(deny_unknown_fields)]`, but serde does not propagate that
attribute into nested struct types — `RawToolPolicy` (the `tools:` section) and
`RawPermissions` (the `permissions:` section) each need their own independent
`deny_unknown_fields` declaration. Without it, a typo inside either nested section (e.g.
`pemission_mode:`, `alow:`) was silently ignored by serde's normal unknown-field handling and
the section fell back to its fail-safe default, instead of the definition failing to load —
masking a misconfigured sub-agent definition as a silently-permissive default.

### Mechanism

Both `RawToolPolicy` and `RawPermissions` now carry `#[serde(deny_unknown_fields)]`. Every
field on both structs already carries an explicit `#[serde(default = ...)]`, so this addition
only rejects genuinely unknown keys (typos) — it has no effect on omission of optional fields,
so existing valid frontmatter that omits optional keys continues to parse unchanged.

**BREAKING CHANGE**: `SubAgentDef::parse()` now returns `Err(SubAgentError::Parse)` for any
sub-agent frontmatter file with a misspelled or otherwise unrecognized key inside its nested
`tools:` or `permissions:` section — previously such a file loaded successfully with that
field's section silently reset to its default.

### Key Invariants

- `#[serde(deny_unknown_fields)]` on a parent struct does NOT extend to nested struct types —
  each nested struct requiring strict rejection must declare the attribute independently.
- Strict rejection must never conflict with optional-field omission — adding
  `deny_unknown_fields` to a struct without first ensuring every field has an explicit default
  would reject currently-valid frontmatter that merely omits an optional key.

---

## 19. See Also

- [[constitution]] — project principles
- [[002-agent-loop/spec]] — parent agent loop that uses `SubAgentManager`
- [[010-security/spec]] — security model; agent name validation and grant TTLs are security controls
- [[026-tui-subagent-management/spec]] — TUI sidebar that displays running subagents
- [[033-subagent-context-propagation/spec]] — context propagation spec (shipped v0.18+)
- [[MOC-specs]] — all specifications
