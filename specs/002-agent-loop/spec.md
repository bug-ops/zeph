---
aliases:
  - Agent Loop
  - Turn Lifecycle
  - Context Pressure
tags:
  - sdd
  - spec
  - core
  - agent
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec]]"
  - "[[003-llm-providers/spec]]"
  - "[[004-memory/spec]]"
---

# Spec: Agent Loop

> [!info]
> Agent main loop, turn lifecycle, context pressure management, and HiAgent subgoal-aware compaction.
> See [[001-system-invariants/spec#2. Agent Loop Contract]] for invariants.

## Sources

### External
- **Context Engineering in Manus** (Oct 2025) — soft/hard compaction stages, schema-based summarization: https://rlancemartin.github.io/2025/10/15/manus/
- **ACON** (ICLR 2026) — failure-driven compression guidelines, 26–54% token reduction: https://arxiv.org/abs/2510.00615
- **Effective Context Engineering** (Anthropic, 2025) — just-in-time retrieval, tool output overflow: https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
- **Efficient Context Management** (JetBrains Research, Dec 2025) — observation masking vs. summarization: https://blog.jetbrains.com/research/2025/12/efficient-context-management/
- **Claude Context Management & Compaction API** (Anthropic, 2026): https://platform.claude.com/docs/en/build-with-claude/context-management

### Internal
| File | Contents |
|---|---|
| `crates/zeph-core/src/agent/mod.rs` | `Agent<C>`, `run()`, `process_user_message()`, sub-state structs |
| `crates/zeph-core/src/agent/feedback_detector.rs` | `FeedbackDetector`, `CorrectionSignal` |
| `crates/zeph-core/src/agent/error.rs` | `AgentError` typed hierarchy |
| `crates/zeph-core/src/channel.rs` | `Channel` trait, `ChannelError` |

---

`crates/zeph-core/src/agent/mod.rs` — the single execution context per session.

## Core Structure

```
Agent<C: Channel> {
    provider: AnyProvider,           // LLM backend, swappable at runtime
    channel: C,                      // I/O boundary, owned
    tool_executor: Arc<dyn ErasedToolExecutor>,
    // All sub-state in dedicated structs — no loose fields:
    msg: MessageState,               // messages vec, message_queue, system prompt
    memory_state: MemoryState,
    skill_state: SkillState,
    tool_state: ToolState,
    security: SecurityState,
    mcp: McpState,
    index: IndexState,
    debug_state: DebugState,
    runtime: RuntimeConfig,
    // + ExperimentState, FeedbackState, InstructionState, LifecycleState,
    //   MetricsState, OrchestrationState, ProviderState, SessionState, ...
}
```

## Turn Lifecycle (invariant order)

1. **Drain message queue** — process any `QueuedMessage` before reading channel
2. **`tokio::select!`** — race between:
   - `channel.recv()` — user message
   - skill reload event
   - instruction reload event
   - config reload event
   - scheduled task fire
3. **Builtin command check** — `/exit`, `/clear`, `/compact`, `/plan`, etc. short-circuit; return `Some(bool)` to continue/exit
4. **`process_user_message()`** — main LLM round-trip:
   a. Inject active skills into system prompt
   b. Recall from memory (semantic + code context + graph)
   c. Build context, apply deferred tool pair summaries
   d. Send to LLM provider
   e. Parse response: text / tool calls / thinking blocks
   f. Execute tool calls (confirmation gate if required)
   g. Store turn in memory
   h. Emit response to channel

## Key Invariants

- **System message is always `messages[0]`** — rebuilt each turn from config + skills + instructions
- **Thinking blocks are forwarded verbatim** to the next request — never stripped or summarized
- **Provider can be swapped at runtime** via `provider_override` without restarting the agent
- **Hot-reload events** (skills, instructions, config) are processed between turns, never mid-turn
- **Message queue takes priority** over channel recv — injected messages run before user input
- **Context prep timeout** (`[timeouts] context_prep_timeout_secs`, default 30 s): `advance_context_lifecycle` wrapped with wall-clock timeout; turn proceeds with degraded cached context on expiry instead of stalling (#3357, #3373)
- **NoProviders backoff** (`[timeouts] no_providers_backoff_secs`, default 2 s): after `NoProviders` error the agent records failure timestamp and sleeps; context preparation is skipped on the next turn if still within the backoff window, preventing busy-wait (#3357, #3373)

## Context Pressure Management

- Token counting via `tiktoken-rs` against provider's `context_window()`
- **Soft threshold (~60%)**: apply deferred tool pair summaries
- **Hard threshold (~90%)**: run full compaction (summarize old turns, evict by Ebbinghaus policy)
- Compaction result stored as `MessagePart::Compaction` — never removed from history

## Error Handling

- `AgentError` typed error hierarchy (thiserror)
- LLM errors: transient (retry with backoff) vs permanent (surface to user)
- Tool errors: `ToolError::kind()` → `Transient` / `Permanent`
- Channel errors abort the current turn but do not exit the loop (unless `ChannelError::Fatal`)

---

## HiAgent Subgoal-Aware Compaction

`crates/zeph-core/src/agent/compaction_strategy.rs`, `crates/zeph-core/src/agent/mod.rs`. Issue #2022.

### Overview

HiAgent-inspired pruning strategies (`subgoal` and `subgoal_mig`) track the agent's current subgoal via fire-and-forget LLM extraction and partition tool outputs into three eviction tiers. This preserves active working context across hard compaction events while aggressively evicting stale outputs from completed or abandoned subgoals.

### Eviction Tiers

| Tier | Relevance Score | Description |
|---|---|---|
| Active | 1.0 | Currently-being-worked subgoal — never evicted by scoring |
| Completed | 0.3 | Finished subgoal — candidate for summarization |
| Outdated | 0.1 | Before any subgoal or between completed subgoals — highest priority for eviction |

### `SubgoalRegistry`

In-memory data structure with:
- `subgoals`: list of tracked subgoals, each with `SubgoalState (Active|Completed)` and message span `[start, end)`
- `extend_active(new_msgs)`: incremental O(new_msgs) update; on first subgoal creation, retroactively tags pre-extraction messages (S4 fix)
- `rebuild_after_compaction(offset)`: repairs index maps after drain/reinsert — uses offset arithmetic, not fragile index assumptions (S1 fix)
- `active_subgoal()`: returns the current active subgoal for `/status` display
- `subgoal_state(msg_index)`: returns tier for scoring

### Subgoal Lifecycle

`maybe_refresh_subgoal()` two-phase fire-and-forget:
1. Uses last 6 agent-visible messages as context (M2 fix)
2. LLM extracts current subgoal description
3. If LLM returns `COMPLETED:` signal → current Active subgoal transitions to Completed (S3 fix)
4. New subgoal auto-completes any existing Active subgoal as defense-in-depth (M3 fix)

### Compaction Integration

`compact_context()` with `subgoal`/`subgoal_mig` strategies:
1. Extracts active-subgoal messages before drain
2. Runs standard compaction (drain + summarize)
3. Re-inserts active-subgoal messages after pinned messages (S2 fix)
4. Index repair after `apply_deferred_summaries` insertions (S5 fix)

### `subgoal_mig` Variant

Combines subgoal tier relevance with MIG (Marginal Information Gain) pairwise redundancy scoring:
`score = subgoal_relevance − max_redundancy_with_any_higher_scored_block`

Active subgoal messages (tier 1.0) have their MIG reduction capped so they are never evicted.

### Constraints

- `subgoal` and `SideQuest` eviction strategies are **mutually exclusive** — hard startup error if both enabled
- Config: `pruning_strategy = "subgoal"` or `"subgoal_mig"` in `[memory.compression]`

### Debug Output

`{N}-subgoal-registry.txt` written at pruning time when `--debug-dump` is active. `/status` shows active subgoal description when strategy is `subgoal` or `subgoal_mig`.

### Key Invariants

- Subgoal extraction is always fire-and-forget — never block the agent turn on subgoal LLM call
- Active subgoal messages are extracted before compaction drain and re-inserted after — never lost in compaction
- `rebuild_after_compaction` uses offset arithmetic (not index scanning) — never recalculate by iterating messages
- Index repair must run after `apply_deferred_summaries` insertions — deferred summaries can shift indices
- `subgoal` and `SideQuest` strategies must never be active simultaneously — hard error at startup
- NEVER evict Active-tier messages by scoring — their relevance is 1.0 (protected)
- NEVER run subgoal extraction synchronously in the tool loop — only between turns

## Focus Strategy Auto-Consolidation

`run_focus_auto_consolidation` (#3313, #3388): when the Focus compression strategy is active,
a periodic auto-consolidation pass merges similar focus segments to reduce fragmentation.

- Controlled by `[memory.focus] auto_consolidate_min_window` (default 4 turns); `0` is rejected at
  startup validation — `Config::validate()` rejects zero with a clear error (#3387, #3392)
- `FocusState::should_auto_consolidate()` returns `false` until the configured number of turns has elapsed
- The O(K²) pairwise MIG scoring loop is offloaded to `tokio::task::spawn_blocking` to prevent
  stalling the async executor on long sessions (#3386, #3398)

### Key Invariants

- `auto_consolidate_min_window = 0` MUST be rejected at config validation — it would trigger LLM on every compress call
- Auto-consolidation runs on `spawn_blocking` — never on the async executor thread pool
- Consolidation is guarded by turn count; no-op until the window has elapsed

## Provider Preference Persistence

Provider preference per channel is persisted to SQLite (#3308, #3385):

- Last-used provider (set via `/provider <name>`) saved after each successful switch
- Restored automatically on next session start
- Identity keyed by `(channel_type, channel_id)`; CLI/TUI use `channel_id = ""`
- Controlled by `[session] provider_persistence = true` (default enabled)
- Migrations: SQLite `079_channel_preferences.sql`, Postgres `075_channel_preferences.sql`

### Key Invariants

- Provider preference restore is best-effort — if the stored provider name no longer exists, fall back silently to the default
- NEVER block session startup on preference load failure

## Compaction Progress UX

`MetricsSnapshot` gains four fields for compaction observability (#3314, #3385):

| Field | Type | Meaning |
|---|---|---|
| `context_max_tokens` | `u64` | Effective context window for the active provider |
| `compaction_last_before` | `u64` | Token count before the last hard compaction |
| `compaction_last_after` | `u64` | Token count after the last hard compaction |
| `compaction_last_at_ms` | `u64` | Wall-clock timestamp of the last compaction (0 = never) |

- `Agent::publish_context_budget()` resolves effective context window from `context_manager.budget.max_tokens()` and publishes to `MetricsSnapshot` after provider pool construction and on every `/provider` switch
- `INFO` log (`tokens_before`, `tokens_after`, `saved`) and transient `send_status("Compacting: {b}→{a} tokens")` emitted after each successful hard compaction
- TUI: `context_gauge` widget (color-coded: green < 70%, yellow 70–90%, red > 90%); hidden when `context_max_tokens == 0`
- TUI: `compaction_badge` widget shows `"{before}k→{after}k (-{saved}k) {elapsed}"`; hidden until first compaction this session

## Hard Compaction Post-Processing: Orphaned `tool_result` Strip

After hard compaction the message list is drained and rebuilt. A `tool_result` message
references a prior `tool_use` by id. When the drain removes the originating `tool_use`
(e.g., the turn that produced it was summarized or evicted) the `tool_result` becomes
**orphaned** — its reference id points to a message no longer in the list. Sending an
orphaned `tool_result` to any provider causes a request validation error (Claude: 400,
OpenAI: 422).

Fix (#3256): after `apply_hard_compaction()` and after any `apply_deferred_summaries()`
step, run `strip_orphaned_tool_results(messages)`:

```
strip_orphaned_tool_results(messages: &mut Vec<Message>)
    collect_set of all tool_use ids present in messages
    remove any message where role == tool_result AND tool_use_id NOT IN that set
```

### Key Invariants

- `strip_orphaned_tool_results` runs after EVERY hard compaction event — no exceptions
- The strip runs AFTER `apply_deferred_summaries` (deferred insertions may add new tool_use messages)
- Removing an orphaned `tool_result` is silent (no WARN) unless `--debug-dump` is active
- This is a correctness invariant, not a heuristic — a single orphaned `tool_result` causes a provider 400/422 error
- NEVER send a `tool_result` whose `tool_use_id` is absent from the message list

---

## Goal Lifecycle (#3567)

The agent tracks a per-session *goal state* that reflects whether the current user
intent has been stated, is in progress, or has been completed. This is distinct from
the orchestration `TaskGraph` goal (which is a planned multi-step execution) — goal
lifecycle tracks the natural-language objective expressed by the user in conversation.

### GoalState Machine

```
Idle ──(user message with goal)──► Active(goal_text)
Active ──(agent signals completion)──► Completed(goal_text)
Completed ──(new user message)──► Active(new_goal_text)
Active ──(/clear or session reset)──► Idle
```

`GoalState` is stored on `LifecycleState`. The active goal text is made available
as a template variable in the system prompt (`{current_goal}`) when configured.

### Goal Completion Detection

The agent detects goal completion via a lightweight heuristic:

1. If the last assistant response contains a completion signal phrase (configurable
   pattern list, e.g., "task complete", "done", "finished") and no tool calls were
   emitted in that turn → transition `Active → Completed`
2. If the orchestration `TaskGraph` plan completes → `Active → Completed`
3. Explicit `/done` slash command → `Active → Completed`

Completion transitions emit a `GoalCompleted` event to the channel (displayed as a
status message, not a user-facing message).

### Config

```toml
[agent.goal]
enabled                  = true
track_in_system_prompt   = false      # inject {current_goal} into system prompt
completion_phrases       = ["task complete", "done", "finished", "completed"]
```

### Key Invariants

- Goal lifecycle is informational — it does NOT block tool execution or LLM calls
- NEVER surface `GoalState` to the LLM directly; it is agent-internal and operator-visible only
- The goal text is extracted from the first user message of the conversation; subsequent messages extend or replace the active goal heuristically

---

## TACO Output Compression (#3591)

TACO (Tool-output Automatic Compression and Offload) compresses large tool outputs
before they are injected into the context window. This is a targeted pre-injection
pass, distinct from the turn-level compaction that runs at the 60/90% pressure gates.

### When It Fires

TACO is evaluated after each tool call result is received, before the result is
appended to `messages`:

1. Measure the raw tool output token count via `tiktoken-rs`
2. If `token_count > taco_threshold` AND the tool is in the compressible-tool set
   → run TACO compression
3. Compressed result replaces the raw result in `messages`

### Compression Strategy

TACO compression uses a fast prompt to summarize the tool output:

```
System: You are a concise tool-output summarizer. Preserve all data values,
file paths, exit codes, and structured content. Remove verbose headers and
repeated patterns. Target: under {target_tokens} tokens.
Tool output:
{raw_output}
```

The compressed result is tagged with `MessagePart::TacoCompressed` so the TUI and
audit log can distinguish it from raw output.

### Compressible Tool Set

Default: `["shell", "web_scrape", "read"]`. Configurable via
`[tools.taco] compressible_tools`. MCP tools are excluded from TACO by default
because their structured output schema is unknown.

### Config

```toml
[tools.taco]
enabled             = false           # default off (opt-in)
taco_threshold      = 2000            # tokens; compress outputs above this
target_tokens       = 500             # target compressed size
taco_provider       = ""              # [[llm.providers]] name; empty = primary
compressible_tools  = ["shell", "web_scrape", "read"]
```

### Key Invariants

- TACO fires only on output that exceeds `taco_threshold`; short outputs are passed through untouched
- On compression failure (provider error, timeout) the **raw output is used** — TACO is best-effort
- NEVER compress `tool_result` messages from `execute_tool_call_confirmed` (fenced-block path) — user-approved results must not be silently summarized
- NEVER apply TACO to thinking blocks or system prompt parts
- `taco_provider` is resolved via the provider registry at runtime; empty = primary provider
- Compressed results carry `MessagePart::TacoCompressed` to make compression auditable

---

## Per-Turn ExecutionContext (#3589)

`ShellExecutor` now receives a per-turn `ExecutionContext` that carries the resolved
working directory and environment overrides for that specific turn. This replaces the
previous model where the working directory was a global field on `ShellExecutor`.

### Contents

```rust
pub struct ExecutionContext {
    pub cwd:     PathBuf,             // resolved working directory for this turn
    pub env:     HashMap<String, String>,  // turn-scoped env overrides (e.g., from hooks)
    pub session: SessionId,           // for audit correlation
}
```

### Propagation

`ExecutionContext` is constructed at the start of `process_user_message()` from the
current `LifecycleState::cwd` and any active hook-injected env vars. It is passed to
`ShellExecutor::execute_with_context(&call, &ctx)` instead of reading from a shared
field.

### Key Invariants

- The `cwd` in `ExecutionContext` reflects the working directory **as of the start of the turn** — changes made by `set_working_directory` tool calls in the current turn take effect in the NEXT turn's context
- NEVER mutate the `ExecutionContext` during a turn — it is immutable after construction
- The `ExecutionContext` is not serialized or persisted — it is reconstructed each turn

---

## Memory Retrieval Failure Logging (#3597)

OmniMem self-improvement loop requires a dataset of memory retrieval failures.
Starting from PR #3597, `OmniMem::recall()` logs retrieval failures into the
`skill_outcomes` table (existing SQLite table used by self-learning) with
`outcome_type = "memory_miss"`.

### Logged Fields

| Field | Value |
|-------|-------|
| `outcome_type` | `"memory_miss"` |
| `query` | The original recall query string (truncated to 512 chars) |
| `strategy` | Recall strategy that was attempted (e.g., `"semantic"`, `"graph"`, `"hybrid"`) |
| `error` | Error message or "no_results" |
| `session_id` | Current session UUID |
| `ts` | Unix timestamp |

### What Counts as a Failure

- Qdrant query returns 0 results above the similarity threshold
- Qdrant query returns an error (network, timeout)
- Graph BFS returns 0 edges above the confidence threshold
- Hybrid recall produces 0 non-empty results after merging

### Key Invariants

- Failure logging is fire-and-forget — it MUST NOT block the recall return path
- Logged queries are truncated to 512 characters before storage — no unbounded writes
- Failure logs are NOT surfaced to the LLM or the user; they are operator/self-improvement data only
- `outcome_type = "memory_miss"` is a stable string — consumers (scheduler micro-benchmark) depend on it
