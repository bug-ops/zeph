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
