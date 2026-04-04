# Context Budgets

Zeph manages how much of the LLM's context window is used for each category of information. When `context_budget_tokens` is set, the available space is divided proportionally so that no single category dominates the prompt.

## Budget Allocation

| Category | Share | What it contains |
|----------|-------|------------------|
| Summaries | 15% | Compressed conversation history from past compaction events |
| Semantic recall | 25% | Relevant messages retrieved from past sessions via vector search |
| Recent history | 60% | The most recent messages in the current conversation |

The remaining space is used for the system prompt, active skills, graph memory facts (4% when enabled), and tool schemas.

```toml
[agent]
context_budget_tokens = 128000   # 0 = unlimited (default)
```

When left at `0`, Zeph uses all available context without budgeting. Set this to your model's context window size minus a margin for the response (e.g., `128000` for a 200K-token model).

## BATS Budget Hints

Budget-Aware Token Steering (BATS) injects a hint into the system prompt that tells the LLM how much context space remains. This helps the model:

- Produce appropriately-sized responses instead of exhausting the remaining budget
- Decide whether to call a tool (which adds tokens) or answer from existing context
- Choose concise tool arguments when budget is tight

BATS also implements a **utility-based action policy** that evaluates each turn against five action categories:

| Action | When preferred |
|--------|---------------|
| Respond | Enough context to answer directly |
| Search | Information gap detected, memory search likely to help |
| Tool-use | Task requires external action (shell, file, web) |
| Delegate | Sub-task is independent enough for a sub-agent |
| Wait | Ambiguous request, better to ask for clarification |

The action with the highest expected utility given the current budget and conversation state is selected. This prevents the agent from making expensive tool calls when the budget is nearly exhausted.

## Skill Prompt Modes

When context budget is tight, skill injection adapts automatically:

| Mode | Behavior |
|------|----------|
| `auto` (default) | Full skill bodies when budget allows, compact XML when tight |
| `compact` | Always use condensed format (~80% smaller) |
| `full` | Always inject full skill bodies |

```toml
[skills]
prompt_mode = "auto"   # "auto", "compact", or "full"
```

In compact mode, only the skill name, description, and trigger phrases are included — the full body is omitted. This keeps skill matching functional even when the context window is nearly full.

## Compaction Tiers

When messages exceed the budget, Zeph applies two tiers of compression:

1. **Soft compaction** (at 70% of budget) — prunes old tool outputs and applies pre-computed deferred summaries. No LLM call needed.
2. **Hard compaction** (at 90% of budget) — runs chunked LLM-based summarization. Messages are split into ~4096-token chunks, summarized in parallel, then merged.

Both tiers use dual-visibility flags: original messages become hidden from the LLM but remain visible in the UI. Summaries are visible to the LLM but hidden from the UI.

```toml
[memory]
soft_compaction_threshold = 0.70   # fraction of budget (default: 0.70)
hard_compaction_threshold = 0.90   # fraction of budget (default: 0.90)
```

## Next Steps

- [Context Engineering](../advanced/context.md) — full compaction pipeline, proactive compression, and tuning
- [Memory and Context](memory.md) — how memory and context work together
- [Token Efficiency](../architecture/token-efficiency.md) — how tokens are counted and optimized
