# Subagent Context Propagation: Gap Analysis

**Date**: 2026-04-03  
**Status**: Draft  
**Scope**: `/agent spawn` command — context passed to subagents at launch

---

## 1. Reference: Claude Code Implementation

### Architecture overview

Claude Code spawns subagents through `AgentTool.tsx`. Context is represented by
`ToolUseContext` — a rich runtime object that is cloned and partially shared on spawn.
The key entry point is `createSubagentContext()` in `src/utils/forkedAgent.ts`.

### What is passed to the subagent

| Context element | Mechanism |
|---|---|
| LLM model | `AgentDefinition.model` field (`'inherit'` copies parent's model) |
| System prompt | Fresh build from `AgentDefinition.getSystemPrompt()`, or frozen parent bytes on fork path |
| Conversation history | Empty by default; full parent history on fork path via `forkContextMessages` |
| Tools | Filtered pool built from `AgentDefinition.tools`; exact parent tools on fork path |
| MCP clients | Shared from parent — `parentContext.options.mcpClients` passed through |
| MCP resources | Shared — `parentContext.options.mcpResources` passed through |
| Agent definitions | Shared — enables nested agent spawns |
| App state (read) | Wrapped: `getAppState` isolates UI prompts for background agents |
| App state (write) | Shared for sync agents; no-op for async agents |
| Abort controller | Child controller linked to parent (cancel propagates) |
| File state cache | Cloned — each agent gets its own LRU cache |
| Query depth | Incremented (`depth + 1`) — prevents infinite recursion |
| Working directory | `worktreePath` propagated explicitly |

### Fork path (implicit spawn)

When `subagent_type` is not specified, Claude Code uses a **fork path** that:

1. Inherits the parent's system prompt as frozen bytes (byte-exact for prompt cache)
2. Passes the full parent conversation history as a prefix
3. Uses `useExactTools: true` — inherits parent's exact tool pool
4. Achieves high prompt cache hit rates via byte-identical API prefixes

### Isolation model

- Background (async) agents: fully isolated from UI mutations, independent abort
- Sync (foreground) agents: share `setAppState` for live UI updates
- All agents: isolated file state cache, new abort controller (child-linked), new query tracking

---

## 2. Current Zeph Implementation

### Architecture overview

Zeph spawns subagents via `SubAgentManager::spawn()` in `crates/zeph-subagent/src/manager.rs`.
The entry point from the agent loop is `handle_agent_command()` in
`crates/zeph-core/src/agent/mod.rs:4505`.

### `spawn()` signature

```rust
pub fn spawn(
    def_name: &str,
    task_prompt: &str,
    provider: AnyProvider,
    tool_executor: Arc<dyn ErasedToolExecutor>,
    skills: Option<Vec<String>>,
    config: &SubAgentConfig,
) -> Result<String, SubAgentError>
```

### What is actually passed

| Context element | Status | Notes |
|---|---|---|
| LLM provider | ✅ Passed | Parent's `AnyProvider` cloned |
| Model override | ✅ Partial | `def.model` from frontmatter; no `'inherit'` option |
| System prompt | ✅ Passed | Built from `def.system_prompt` + optional memory injection |
| Task prompt | ✅ Passed | User's `/agent spawn <name> <prompt>` argument |
| Tool executor | ✅ Passed | Parent's `Arc<dyn ErasedToolExecutor>` |
| Skills | ✅ Passed | Filtered by `SkillFilter` from `def.skills` |
| Permission mode | ✅ Passed | From `def.permissions.permission_mode` |
| Memory scope | ✅ Passed | MEMORY.md injected into system prompt |
| Hooks | ✅ Passed | `def.hooks` (PreToolUse / PostToolUse) |
| Conversation history | ❌ **NOT passed** | `initial_messages: vec![]` — always empty |
| Parent system prompt | ❌ **NOT passed** | Subagent uses its own `def.system_prompt` only |
| MCP context | ❌ **NOT passed** | Only base `tool_executor` (no MCP layer) |
| Abort/cancel linkage | ❌ **NOT linked** | Independent `CancellationToken` per agent |
| Working directory | ⚠️ Implicit | `std::env::current_dir()` in memory — not explicitly propagated |
| Agent depth tracking | ❌ Missing | No recursion depth counter |
| Agent definitions | ❌ **NOT passed** | Nested agent spawn not possible from within subagent |

### Call site in core (mod.rs:4524)

```rust
AgentCommand::Spawn { name, prompt } => {
    let provider = self.provider.clone();
    let tool_executor = Arc::clone(&self.tool_executor);
    let skills = self.filtered_skills_for(&name);
    let mgr = self.orchestration.subagent_manager.as_mut()?;
    let cfg = self.orchestration.subagent_config.clone();
    let task_id = match mgr.spawn(&name, &prompt, provider, tool_executor, skills, &cfg) { ... }
```

Notice: `self.context` (conversation history, session config, etc.) is **not passed**.

### AgentLoopArgs construction (manager.rs:946)

```rust
tokio::spawn(run_agent_loop(AgentLoopArgs {
    provider,
    executor,
    system_prompt,   // from def only
    task_prompt,     // user's prompt string
    skills,
    max_turns,
    cancel: cancel_clone,   // independent token
    ...
    initial_messages: vec![],   // ← always empty
    model: def.model.clone(),
}));
```

---

## 3. Gap Analysis

### P1 — Critical

#### GAP-01: No conversation history passed to subagent

**What CC does**: Fork agents receive the full parent conversation history as
`forkContextMessages`. Non-fork agents receive custom `initialMessages`. Subagents
always have context about what the parent was doing.

**What Zeph does**: `initial_messages: vec![]` — unconditionally empty. The subagent
starts with a blank message history regardless of the parent's state.

**Impact**: Subagent cannot reference prior turns, tool outputs, or files the parent
analyzed. The only context is the raw `task_prompt` string. Complex delegation tasks
("review what we discussed and write tests") fail silently.

**Affected files**:
- `crates/zeph-subagent/src/manager.rs:962` — `initial_messages: vec![]`
- `crates/zeph-core/src/agent/mod.rs:4530-4535` — spawn call site

---

### P2 — High

#### GAP-02: No parent context injection (recent messages / last N turns)

**What CC does**: Even without full history, the subagent knows its `querySource`
(`agent:builtin:fork`) and receives a prompt that encodes the parent's intent structurally.

**What Zeph does**: Subagent receives only the literal `task_prompt` string. No
summarized context, no recent turn window, no structured encoding of parent state.

**Impact**: Subagent has no way to understand why it was spawned or what information
the parent already has. Users must manually copy context into the prompt string.

---

#### GAP-03: No model inheritance

**What CC does**: `AgentDefinition.model` accepts `'inherit'` — the subagent uses
the same model as its parent. Also supports named aliases (`claude-opus`, etc.).

**What Zeph does**: `def.model` is `Option<String>` — `None` means use the default
provider's model (whatever `chat_with_tools` resolves to). No `inherit` semantics.
No way to say "use the same model the parent is using".

**Impact**: Subagents always use the definition's hardcoded model or the global
default — no dynamic model selection based on parent context.

---

#### GAP-04: MCP context not propagated

**What CC does**: `mcpClients` and `mcpResources` from the parent context are passed
to every subagent automatically. MCP servers spawned for the session are available
to all agents in the tree.

**What Zeph does**: Only the base `tool_executor: Arc<dyn ErasedToolExecutor>` is
passed. If MCP tools are registered in the parent's executor, they are available
(because the Arc is shared). However, MCP server lifecycle (`McpManager`) is not
passed — subagents cannot dynamically add MCP servers, and MCP resource metadata
is not propagated.

**Impact**: Subagents that need to enumerate MCP resources or spawn new MCP
connections cannot do so.

---

#### GAP-05: Cancel/abort does not propagate from parent to children

**What CC does**: `createChildAbortController(parentContext.abortController)` —
cancelling the parent cancels all synchronous children. Background agents use their
own controller but are tracked in `AppState`.

**What Zeph does**: Each subagent gets `CancellationToken::new()` — fully independent.
If the parent is stopped (e.g., Ctrl+C in CLI), running subagents continue until
their own `max_turns` or `timeout_secs`.

**Impact**: Resource leak on parent cancellation; subagents outlive their parent.

---

### P3 — Medium

#### GAP-06: No agent recursion depth guard

**What CC does**: `queryTracking.depth` is incremented on each spawn. Deep nesting
can be detected and limited.

**What Zeph does**: No depth tracking at all. An agent that spawns another that
spawns another will run unchecked until `max_concurrent` is hit.

**Impact**: Potential runaway recursion consuming LLM budget and concurrency slots.

---

#### GAP-07: Working directory not explicitly propagated ✅ Partially resolved (#2582)

**What CC does**: `worktreePath` is passed explicitly to `runAgent` and stored in
the agent context.

**What Zeph does**: Memory scope resolution uses `std::env::current_dir()` at spawn
time. If the CWD changes between parent startup and spawn (rare but possible), the
memory directory resolves to the wrong path.

**Fix applied (PR #2585, v0.18.2+)**: `build_system_prompt_with_memory` now appends
`"\nWorking directory: {cwd}"` to every subagent system prompt so the LLM explicitly
knows where the project is. The underlying architecture still uses implicit CWD
rather than explicit propagation (full fix tracked in Phase 2).

**Impact**: Residual risk is low — CWD is now visible to the model, resolving the
primary symptom (LLM hedging instead of acting).

---

#### GAP-08: Nested agent spawn not supported from within subagent

**What CC does**: `agentDefinitions` is passed through the context — any subagent
can spawn further subagents using the same definition registry.

**What Zeph does**: `SubAgentManager` is only accessible via the parent `Agent`
struct (`self.orchestration.subagent_manager`). Subagents run in `run_agent_loop`
which has no access to the manager. Nested spawning is architecturally impossible.

**Impact**: Limits multi-level orchestration patterns (planner → executor → validator).

---

#### GAP-08b: Agent loop exits immediately on text-only first response ✅ Fixed (#2582)

**What CC does**: The agent loop continues regardless of whether the LLM calls tools
or returns plain text — it is up to `max_turns` to terminate the loop.

**What Zeph did**: `handle_tool_step` returning `true` (text-only, no tool call) caused
`run_agent_loop` to `break` immediately, exiting after exactly 1 LLM turn with 0 tool
invocations. When the LLM announced intent instead of acting (common on the first turn),
the subagent completed with only the announcement.

**Fix applied (PR #2585, v0.18.2+)**: On text-only turn 1 with `any_tool_called == false`,
the loop pushes a nudge user message ("Please use the available tools to complete the task.
Do not announce intentions — execute them.") and continues for one more turn. Subsequent
text-only turns still terminate the loop normally.

**Impact**: Resolved. Subagents with intent-announcing LLMs now proceed to tool use on
the retry turn.

---

#### GAP-09: Serde asymmetry in SubAgentDef (known — IMP-CRIT-04)

`disallowed_tools` is deserialized from `tools.except` in YAML frontmatter but
serialized as a flat top-level key. Round-trip serialization is not supported.
Documented in source as a known issue to address before v1.0.0.

---

### P4 — Low

#### GAP-10: No inter-agent communication (team model)

**What CC does**: `InProcessTeammateTask` enables agents to communicate via mailbox
messages (`SendMessage`). Agents can coordinate, delegate subtasks, and report progress
to each other.

**What Zeph does**: Parent polls subagent status via `poll_subagent_until_done()`.
No bidirectional or multi-agent communication. No team model.

**Impact**: Multi-agent collaboration patterns require manual orchestration via the
parent's LLM turn.

---

#### GAP-11: No fork/prompt-cache optimization path

**What CC does**: Fork path reuses the parent's frozen system prompt bytes and full
conversation history as a prompt cache prefix, achieving high cache hit rates.

**What Zeph does**: Each spawn builds the system prompt fresh.
`build_system_prompt_with_memory` always constructs a new string.

**Impact**: Higher LLM API cost and latency when spawning many subagents for
parallel subtasks.

---

## 4. Summary Table

| Gap | Priority | Area | Complexity |
|---|---|---|---|
| GAP-01: No history passed | P1 | Context propagation | Medium |
| GAP-02: No parent context injection | P2 | Context propagation | Medium |
| GAP-03: No model inheritance | P2 | Config | Low |
| GAP-04: MCP context not propagated | P2 | Infrastructure | High |
| GAP-05: Cancel not cascading | P2 | Lifecycle | Medium |
| GAP-06: No recursion depth guard | P3 | Safety | Low |
| GAP-07: CWD not explicit | P3 | Correctness | Low | ✅ Partially resolved (#2585) |
| GAP-08: No nested spawn support | P3 | Architecture | High |
| GAP-08b: Loop exits on text-only first turn | P2 | Loop control | Low | ✅ Fixed (#2585) |
| GAP-09: Serde asymmetry | P3 | Known issue | Low |
| GAP-10: No inter-agent comm | P4 | Team model | High |
| GAP-11: No prompt cache opt | P4 | Performance | Medium |

---

## 5. Recommended Implementation Order

### Phase 1 — Context enrichment (unblocks real use cases)

1. **GAP-01**: Add `parent_context: Option<Vec<Message>>` parameter to `spawn()`.
   In `handle_agent_command`, pass the last N messages from `self.context.messages`
   (configurable, default 10). Inject as `initial_messages` in `AgentLoopArgs`.
   The subagent receives them before the system prompt in `run_agent_loop`.

2. **GAP-03**: Add `model: Option<ModelInheritance>` to `SubAgentDef` where
   `ModelInheritance` is `enum { Inherit, Named(String) }`. In `spawn()`, resolve
   `Inherit` to the parent provider's current model name.

3. **GAP-05**: Change `CancellationToken::new()` to
   `parent_cancel.child_token()` using `tokio_util`'s child token API.
   Add `parent_cancel: CancellationToken` param to `spawn()`.

### Phase 2 — Infrastructure gaps

4. **GAP-04**: Add `mcp_manager: Option<Arc<McpManager>>` to `AgentLoopArgs`.
   Expose MCP tool metadata to subagents for resource enumeration.

5. **GAP-06**: Add `depth: u32` field to `AgentLoopArgs`. Gate spawn inside
   `SubAgentManager` behind a `max_depth` config value (default 3).

### Phase 3 — Architecture changes (post-MVP)

6. **GAP-08**: Extract `SubAgentManager` behind an `Arc<Mutex<>>` or channel-based
   API so subagents can request spawns via message passing to the parent task.

7. **GAP-02**: Add structured context injection — summarize the last N parent turns
   and inject as a preamble in the subagent's first user message.

---

## 6. Files to Modify for Phase 1

| File | Change |
|---|---|
| `crates/zeph-subagent/src/manager.rs` | Add `parent_context`, `parent_cancel` to `spawn()` and `AgentLoopArgs` |
| `crates/zeph-core/src/agent/mod.rs:4530` | Pass `self.context.messages` slice and cancel token to spawn |
| `crates/zeph-subagent/src/def.rs` | Add `model` inheritance enum |
| `crates/zeph-config/src/subagent.rs` | Add `max_depth` config field |
| `crates/zeph-subagent/src/manager.rs:946` | Use `parent_cancel.child_token()` |
