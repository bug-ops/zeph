---
aliases:
  - Subagent Context
  - Subagent Spawning
  - Agent Spawning & Delegation
tags:
  - sdd
  - spec
  - subagent
  - context
  - delegation
created: 2026-04-03
status: approved
related:
  - "[[MOC-specs]]"
  - "[[002-agent-loop/spec]]"
  - "[[026-tui-subagent-management/spec]]"
  - "[[032-handoff-skill-system/spec]]"
---

# Spec: Subagent Context Propagation

> [!info]
> Implementation spec for `/agent spawn` command. Documents how parent context is
> passed to spawned subagents, the YAML frontmatter format, and which gaps remain open.
> All core features shipped in PR #2575–#2579 (v0.18.0–v0.20.0).

**Scope**: `/agent spawn <name> <prompt>` command — spawning isolated subagent sessions with parent-derived state propagation  
**Status**: Approved (shipped v0.18.0+)  
**Related PRs**: #2575, #2576, #2577, #2578, #2579

---

## 1. Overview

Subagents are isolated LLM sessions spawned from a parent agent to delegate or parallelize work.
The parent passes rich context (conversation history, provider info, cancellation signals) to the subagent
so it can understand why it was spawned and what the parent has already tried.

The spawn interface is implemented in `SubAgentManager::spawn()` and configured via `SubAgentConfig`.

---

## 2. SubAgentDef YAML Format

Subagent definitions are Markdown files with YAML frontmatter. Example:

```markdown
---
name: code-reviewer
description: Comprehensive code review with security focus
model: claude-opus-4
permissions:
  max_turns: 15
  timeout_secs: 300
  background: false
skills:
  include:
    - "git-*"
    - "review"
  exclude:
    - "deploy"
tools:
  allow:
    - shell
    - Read
    - Edit
  except:
    - "rm"
    - "sudo"
memory: project
hooks:
  - type: command
    when: pre_tool_use
    if_tool: "shell"
    command: "pre-tool-hook.sh"
---

You are an expert code reviewer. Focus on:
- Security vulnerabilities (injection, XSS, SSRF)
- Performance issues (N+1 queries, unbounded loops)
- Style consistency with the project
- Test coverage gaps

Do not approve code until all issues are addressed.
```

### 2.1 Frontmatter Fields

| Field | Type | Required | Default | Notes |
|-------|------|----------|---------|-------|
| `name` | string | ✓ | — | ASCII alphanumeric + hyphen/underscore, max 64 chars. Used in `/agent spawn <name>` |
| `description` | string | ✓ | — | Shown in `/agent list` output |
| `model` | string \| "inherit" | ✗ | None | See §2.2. Special value `"inherit"` uses parent's model |
| `permissions.max_turns` | u32 | ✗ | config default | Max LLM turns before auto-stop |
| `permissions.timeout_secs` | u32 | ✗ | 600 | Timeout in seconds |
| `permissions.background` | bool | ✗ | false | If `true`, spawn runs without blocking parent (background task) |
| `permissions.permission_mode` | enum | ✗ | config default | `default` \| `accept_edits` \| `dont_ask` \| `bypass_permissions` \| `plan` |
| `skills.include` | [string] | ✗ | [] | Glob patterns of skills to enable; empty = inherit all |
| `skills.exclude` | [string] | ✗ | [] | Glob patterns to remove from inherited set |
| `tools.allow` \| `tools.deny` | [string] | ✗ | — | Allowlist or denylist of tool IDs |
| `tools.except` | [string] | ✗ | [] | Additional denylist (deny wins) |
| `memory` | enum | ✗ | None | `user` \| `project` \| `local` — persistent memory scope |
| `hooks` | [hook] | ✗ | [] | Per-agent lifecycle hooks (PreToolUse, PostToolUse) |

### 2.2 Model Field: Inherit Semantics

The `model` field supports three modes:

1. **Absent or `None`**: Use the subagent config's default provider (usually the main provider)
2. **Named model** (e.g., `"gpt-4o-mini"`): Use that specific model by name
3. **`"inherit"`**: Copy the parent's current model name at spawn time

Example:

```yaml
# Use fast/cheap model for simple reviews
model: gpt-4o-mini

# Use same model as parent for consistency
model: inherit

# Use strongest model for complex analysis
model: claude-opus-4
```

**Implementation**: At spawn time, if `def.model == Some("inherit")`, resolve to `parent_provider_name` from `SpawnContext`.
If resolution fails (parent name not found), fall back to the main provider's default model.

---

## 3. Context Propagation: SpawnContext

The `SpawnContext` struct carries parent-derived state to the spawned subagent:

```rust
pub struct SpawnContext {
    /// Recent parent conversation messages (last N turns).
    pub parent_messages: Vec<Message>,
    
    /// Parent's cancellation token for linked cancellation (foreground spawns).
    pub parent_cancel: Option<CancellationToken>,
    
    /// Parent's active provider name (for model:inherit resolution).
    pub parent_provider_name: Option<String>,
    
    /// Current spawn depth (0 = top-level agent).
    pub spawn_depth: u32,
    
    /// MCP tool names available in the parent's tool executor (for diagnostics).
    pub mcp_tool_names: Vec<String>,
}
```

### 3.1 Message History Propagation

**Configuration**: `[agents] context_window_turns` (default: 10)

When spawning, the last N turns from `parent.context.messages` are extracted and passed as `parent_messages`.
These are prepended to the subagent's message history before the task prompt.

**Example**: If the parent has 20 turns and `context_window_turns = 5`, only the last 5 turns are included.
This bounds memory overhead and focus the subagent on recent context.

Set `context_window_turns = 0` to disable history propagation entirely.

### 3.2 Cancellation Linkage (Foreground Spawns)

When `permissions.background = false` (foreground spawn):
- `parent_cancel` is set to the parent's `CancellationToken`
- The subagent's cancel token is linked via `parent_cancel.child_token()`
- If the parent is cancelled (Ctrl+C, session abort), the subagent is cancelled too

When `permissions.background = true`:
- `parent_cancel` is `None`
- The subagent gets an independent `CancellationToken`
- Subagent continues running even if the parent is cancelled
- Parent polls via `SubAgentManager::statuses()` and `collect()`

### 3.3 Spawn Depth Tracking

Each spawn increments the `spawn_depth` counter.

- **Depth 0**: Top-level agent (CLI, Telegram, TUI)
- **Depth 1**: First-level subagent spawned by depth-0
- **Depth 2**: Subagent spawned by a depth-1 subagent
- ...
- **Depth N**: Blocked if N > `config.max_spawn_depth` (default: 3)

**Depth guard**: When `spawn_depth >= max_spawn_depth`, `SubAgentManager::spawn()` returns `SubAgentError::TooDeep`.

---

## 4. Context Injection: Prepending Parent Context

**Configuration**: `[agents] context_injection_mode` (default: `LastAssistantTurn`)

The parent's recent context is injected into the subagent's task prompt to answer "why was I spawned?".

### 4.1 Injection Modes

| Mode | Behavior |
|------|----------|
| `none` | No context injected; subagent receives only the literal task prompt |
| `last_assistant_turn` | Prepend the last assistant message from parent history as a preamble |
| `summary` | LLM-summarized parent context (Phase 2, not yet implemented; falls back to `last_assistant_turn`) |

### 4.2 LastAssistantTurn Mode (Implemented)

When `context_injection_mode = last_assistant_turn`:

1. Extract the last `Role::Assistant` message from `parent_messages`
2. Prepend it to `task_prompt` as a structured preamble:

```
Context from parent agent:
---
[last assistant message body]
---

Now, [original task_prompt]
```

Example:

```
Context from parent agent:
---
I analyzed the PR and found three issues:
1. SQL injection in user input handling
2. Missing rate limit on API endpoint
3. Incorrect error logging
---

Now, write a detailed security report on these findings.
```

**Impact**: The subagent understands what the parent has already discovered, avoiding duplicate analysis.

---

## 5. Configuration: SubAgentConfig TOML Section

The `[agents]` section controls subagent behavior:

```toml
[agents]
enabled = true
max_concurrent = 5                       # max simultaneous subagents
max_spawn_depth = 3                      # depth guard (0 = prevent nesting)
context_window_turns = 10                # parent turns to pass (0 = none)
context_injection_mode = "last_assistant_turn"  # or "none" / "summary"
default_permission_mode = "dont_ask"
allow_bypass_permissions = false
transcript_enabled = true
transcript_max_files = 50

[agents.default_memory_scope]
# ...
```

**Key defaults**:
- `max_concurrent`: 5 — prevent runaway spawns
- `max_spawn_depth`: 3 — limit nesting levels
- `context_window_turns`: 10 — recent context window
- `context_injection_mode`: `last_assistant_turn` — provide parent context

---

## 6. Call Sites and Integration

### 6.1 Spawning from the Agent Loop

In `crates/zeph-core/src/agent/mod.rs`, `handle_agent_command()` calls:

```rust
AgentCommand::Spawn { name, prompt } => {
    let ctx = SpawnContext {
        parent_messages: self.context.messages
            .iter()
            .rev()
            .take(config.agents.context_window_turns)
            .cloned()
            .collect(),
        parent_cancel: Some(self.cancel.clone()),
        parent_provider_name: Some(self.provider.name().to_owned()),
        spawn_depth: self.spawn_depth + 1,
        mcp_tool_names: self.tool_executor.mcp_tools(),  // if available
    };
    
    let task_id = mgr.spawn(&name, &prompt, provider, executor, skills, config, ctx)?;
    // ...
}
```

### 6.2 Command Syntax

```
/agent spawn <name> <prompt...>
```

- `<name>`: Definition name (e.g., `code-reviewer`)
- `<prompt>`: Task description (e.g., `"Review this PR for security issues"`)

The `prompt` is placed after context injection and sent to the subagent's LLM.

---

## 7. Foreground vs Background Spawns

### 7.1 Foreground (Blocking, Linked Cancellation)

When `permissions.background = false`:

```yaml
permissions:
  background: false  # or omitted (default)
```

- Parent **blocks** until the subagent completes (turns == `max_turns` or timeout)
- Subagent's cancel token is linked to parent's — cancelling parent cancels subagent
- Result is returned directly in the turn
- Used for: sequential delegation ("do X, then I'll continue"), code review, validation

### 7.2 Background (Non-Blocking, Independent)

When `permissions.background = true`:

```yaml
permissions:
  background: true
```

- Parent **returns immediately** with a task ID
- Subagent runs independently in a background task
- No cancellation linkage — subagent continues if parent is cancelled
- Parent can poll status via `/agents status <task_id>` or `/agents collect`
- Used for: parallel work (multiple reviews in parallel), long-running analysis, batch processing

**Implementation**: The subagent's `JoinHandle` is stored in `SubAgentManager`'s task set;
parent can poll via `statuses()` and retrieve transcripts via `collect()`.

---

## 8. Transcript Persistence

When `[agents] transcript_enabled = true`, subagent conversations are persisted to JSONL:

```
.zeph/subagent-transcripts/
├── <task_id>.jsonl        # conversation history
└── <task_id>.meta.json    # metadata (status, turns, duration)
```

Transcripts enable:
- Session resume on restart
- Auditing and debugging
- Training data collection

See `TranscriptWriter` and `TranscriptReader` in `zeph-subagent`.

---

## 9. Gap Analysis: Resolved and Open

| Gap | Priority | Resolved? | Status | Notes |
|---|---|---|---|---|
| GAP-01: Conversation history | P1 | ✅ | Shipped #2575, #3760, #3761 | `parent_messages` in `SpawnContext`; orphaned tool pairs pruned by `trim_parent_messages`; budget estimated by `estimate_parts_size` |
| GAP-02: Parent context injection | P2 | ✅ | Shipped #2576 | `context_injection_mode` with `last_assistant_turn` |
| GAP-03: Model inheritance | P2 | ✅ | Shipped #2577 | `model: "inherit"` in frontmatter |
| GAP-04: MCP context propagation | P2 | ⚠️ Partial | Shipped #2578 | Tool executor shared; MCP server lifecycle not accessible |
| GAP-05: Cancellation propagation | P2 | ✅ | Shipped #2579 | `parent_cancel.child_token()` for foreground spawns |
| GAP-06: Recursion depth guard | P3 | ✅ | Shipped #2575 | `max_spawn_depth` config with depth tracking |
| GAP-07: CWD propagation | P3 | ✅ | Shipped #2582 | CWD appended to system prompt explicitly |
| GAP-08: Nested spawn from subagent | P3 | ❌ | Open | Subagent has no access to `SubAgentManager`; requires architecture change |
| GAP-08b: Early loop exit on text-only | P2 | ✅ | Shipped #2585 | Nudge message on first turn if no tools called |
| GAP-09: Serde asymmetry | P3 | ❌ | Documented | `disallowed_tools` serde mismatch; tracked as IMP-CRIT-04 |
| GAP-10: Inter-agent communication | P4 | ❌ | Open | No mailbox/team model; requires design phase |
| GAP-11: Prompt cache optimization | P4 | ❌ | Open | No fork path reuse; each spawn rebuilds system prompt |

---

## 10. Key Invariants

### Always
- Parent context is passed as-is (no filtering or scrubbing)
- Foreground spawns block the parent until completion
- Depth guard is enforced before concurrency guard to fail fast
- Cancellation is propagated from parent to foreground children
- Background spawns are tracked and can be collected or cancelled independently
- All spawns include `spawn_depth` and are gated by `max_spawn_depth`
- Parent message history is pruned by `trim_parent_messages()` before passing to the subagent: any `ToolResult` without a paired `ToolUse` is removed first (pass 1), then any `ToolUse` without a paired `ToolResult` is removed (pass 2), except the trailing assistant message which is exempt from pass 2 to preserve pending tool calls at the conversation boundary (#3760)
- Token budget for the sliced parent history is estimated by `estimate_parts_size()`, which uses a 50-byte JSON overhead for `ToolUse`/`ToolResult` blocks, base64 expansion ratio for images, and `content.len()` fallback; `flatten_parts()` byte counting is NOT used for budget estimation because it underestimates structured JSON size (#3761)

### Ask First
- Enabling `background = true` for long-running subagents (risk of orphaned tasks)
- Setting `max_spawn_depth = 0` to disable nesting entirely
- Using `context_injection_mode = "none"` (loses parent context awareness)

### Never
- Spawn subagents with `bypass_permissions = true` unless explicitly configured
- Pass secrets or sensitive data in the `task_prompt` (use memory scope + MEMORY.md instead)
- Spawn more than `max_concurrent` subagents (enforced at call site)
- Link a subagent's cancel token to more than one parent (use `.child_token()` for one-to-one linkage)

---

## 11. Open Questions & Future Work

1. **GAP-08 (Nested spawn)**: How should a subagent request spawning another subagent?
   - Option A: Pass `Arc<SubAgentManager>` in `AgentLoopArgs` (requires mutation handling)
   - Option B: Subagent sends message to parent via channel (async, requires orchestration)
   - Option C: Subagent cannot spawn — multi-level delegation goes through parent (simplest)
   
2. **GAP-10 (Team model)**: Should subagents be able to send messages to each other (sibling coordination)?
   - Requires mailbox/queue per subagent
   - Requires heartbeat or polling mechanism to check for messages
   - Would enable multi-agent collaboration patterns (reviewer + executor + merger)

3. **GAP-11 (Prompt cache)**: Should fork-path spawns (inherit all parent state) reuse the parent's system prompt bytes?
   - Would require a "fork spawn" subcommand or frontmatter flag
   - Higher cache hit rate for parallel subagent spawns
   - Phase 2 optimization

4. **GAP-09 (Serde asymmetry)**: Should round-trip serialization of `SubAgentDef` work?
   - Currently `disallowed_tools` deserializes from `tools.except` but serializes as top-level
   - Fix: migrate on-disk format or add custom serde impl
   - Pre-v1.0.0 cleanup

---

## See Also

- [[MOC-specs]] — all specifications
- [[002-agent-loop/spec]] — agent main loop and context assembly
- [[026-tui-subagent-management/spec]] — TUI subagent sidebar
- [[032-handoff-skill-system/spec]] — inter-agent handoff protocol
- [[001-system-invariants/spec]] — system contracts
