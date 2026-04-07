# Spec: Agent Loop

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
    messages: Vec<Message>,          // conversation history, system msg at [0]
    message_queue: VecDeque<QueuedMessage>, // injected messages, drained first
    provider_override: Arc<RwLock<Option<AnyProvider>>>,
    // sub-state structs: MemoryState, SkillState, McpState, MetricsState, ...
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

---

## MagicDocs: Auto-Maintained Markdown
`crates/zeph-core/src/agent/magic_docs.rs`. Implemented in v0.18.5. Closes #2702, #2714, #2727, #2732.

### Overview
Files containing a `# MAGIC DOC:` header, when read via file-read tools (`read`, `file_read`, `cat`, `view`, `open`), are registered in a per-session registry. After each response, a background `tokio::task` updates due docs (respecting `min_turns_between_updates`) via a single LLM `chat` call.

### Scanner Two-Phase Design
`scan_messages_for_magic_docs` performs a two-phase scan:
1. **Phase 1**: builds a `HashMap<tool_use_id → (tool_name, file_path)>` from all `ToolUse` parts in `Role::Assistant` messages.
2. **Phase 2**: handles both `ToolOutput` (by tool name, matching `Role::User` messages) and `ToolResult` (by `tool_use_id` lookup, native execution path).

### Utility Gate Bypass
`UtilityScorer` gains `exempt_tools: Vec<String>`. When `MagicDocs` is enabled, the builder automatically extends `exempt_tools` with file-read tool names (`read`, `file_read`, `cat`, `view`, `open`). File-read tools always execute regardless of utility score, ensuring MagicDocs detection sees real file content.

### Config
```toml
[magic_docs]
enabled = false
min_turns_between_updates = 5
update_provider = ""   # falls back to primary when empty
max_iterations = 4
```

### Key Invariants
- Doc updates run in a background `tokio::task` — never block the response path
- Scanner must walk all message roles (not only `Role::Assistant`) — `ToolOutput` parts live in `Role::User` messages
- `exempt_tools` list is extended additively — user-provided entries are preserved
- NEVER update a doc that was updated within `min_turns_between_updates` turns
- File-read tools in `exempt_tools` always bypass utility scoring — never apply utility gate to exempt tools
