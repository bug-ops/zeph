# Context Engineering

Zeph's context engineering pipeline manages how information flows into the LLM context window. It combines semantic recall, proportional budget allocation, message trimming, environment injection, tool output management, and runtime compaction into a unified system.

All context engineering features are **disabled by default** (`context_budget_tokens = 0`). Set a non-zero budget to activate the pipeline.

## Configuration

```toml
[memory]
context_budget_tokens = 128000    # Set to your model's context window size (0 = unlimited)
compaction_threshold = 0.75       # Compact when usage exceeds this fraction
compaction_preserve_tail = 4      # Keep last N messages during compaction

[memory.semantic]
enabled = true                    # Required for semantic recall
recall_limit = 5                  # Max semantically relevant messages to inject

[tools]
summarize_output = false          # Enable LLM-based tool output summarization
```

## Context Window Layout

When `context_budget_tokens > 0`, the context window is structured as:

```
┌─────────────────────────────────────────────────┐
│ BASE_PROMPT (identity + guidelines + security)  │  ~300 tokens
├─────────────────────────────────────────────────┤
│ <environment> cwd, git branch, os, model        │  ~50 tokens
├─────────────────────────────────────────────────┤
│ <project_context> ZEPH.md contents              │  0-500 tokens
├─────────────────────────────────────────────────┤
│ <repo_map> structural overview (if index on)    │  0-1024 tokens
├─────────────────────────────────────────────────┤
│ <available_skills> matched skills (full body)   │  200-2000 tokens
│ <other_skills> remaining (description-only)     │  50-200 tokens
├─────────────────────────────────────────────────┤
│ <code_context> RAG chunks (if index on)         │  30% of available
├─────────────────────────────────────────────────┤
│ [semantic recall] relevant past messages        │  10-25% of available
├─────────────────────────────────────────────────┤
│ [compaction summary] if compacted               │  200-500 tokens
├─────────────────────────────────────────────────┤
│ Recent message history                          │  50-60% of available
├─────────────────────────────────────────────────┤
│ [reserved for response generation]              │  20% of total
└─────────────────────────────────────────────────┘
```

## Proportional Budget Allocation

Available tokens (after reserving 20% for response) are split proportionally. When [code indexing](code-indexing.md) is enabled, the code context slot takes a share from summaries, recall, and history:

| Allocation | Without code index | With code index | Purpose |
|-----------|-------------------|-----------------|---------|
| Summaries | 15% | 10% | Conversation summaries from SQLite |
| Semantic recall | 25% | 10% | Relevant messages from past conversations via Qdrant |
| Code context | -- | 30% | Retrieved code chunks from project index |
| Recent history | 60% | 50% | Most recent messages in current conversation |

## Semantic Recall Injection

When semantic memory is enabled, the agent queries Qdrant for messages relevant to the current user query. Results are injected as transient system messages (prefixed with `[semantic recall]`) that are:

- Removed and re-injected on every turn (never stale)
- Not persisted to SQLite
- Bounded by the 25% token budget allocation

Requires Qdrant and `memory.semantic.enabled = true`.

## Message History Trimming

When recent messages exceed the 60% budget allocation, the oldest non-system messages are evicted. The system prompt and most recent messages are always preserved.

## Environment Context

Every system prompt rebuild injects an `<environment>` block with:

- Working directory
- OS (linux, macos, windows)
- Current git branch (if in a git repo)
- Active model name

## Runtime Context Compaction

When total message tokens exceed `compaction_threshold` (default: 75%) of the context budget:

1. Middle messages (between system prompt and last N recent) are extracted
2. Sent to the LLM with a structured summarization prompt
3. Replaced with a single summary message
4. Last `compaction_preserve_tail` messages (default: 4) are always preserved

Compaction is idempotent and runs automatically during the agent loop.

## Tool Output Management

### Truncation

Tool outputs exceeding 30,000 characters are automatically truncated using a head+tail split with UTF-8 safe boundaries. Both the first and last ~15K chars are preserved.

### Smart Summarization

When `tools.summarize_output = true`, long tool outputs are sent through the LLM with a prompt that preserves file paths, error messages, and numeric values. On LLM failure, falls back to truncation.

```bash
export ZEPH_TOOLS_SUMMARIZE_OUTPUT=true
```

## Progressive Skill Loading

Skills matched by embedding similarity (top-K) are injected with their full body. Remaining skills are listed in a description-only `<other_skills>` catalog — giving the model awareness of all capabilities while consuming minimal tokens.

## ZEPH.md Project Config

Zeph walks up the directory tree from the current working directory looking for:

- `ZEPH.md`
- `ZEPH.local.md`
- `.zeph/config.md`

Found configs are concatenated (global first, then ancestors from root to cwd) and injected into the system prompt as a `<project_context>` block. Use this to provide project-specific instructions.

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS` | Context budget in tokens | `0` (unlimited) |
| `ZEPH_MEMORY_COMPACTION_THRESHOLD` | Compaction trigger threshold | `0.75` |
| `ZEPH_MEMORY_COMPACTION_PRESERVE_TAIL` | Messages preserved during compaction | `4` |
| `ZEPH_TOOLS_SUMMARIZE_OUTPUT` | Enable LLM-based tool output summarization | `false` |
