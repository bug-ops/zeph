# Context Engineering

Zeph's context engineering pipeline manages how information flows into the LLM context window. It combines semantic recall, proportional budget allocation, message trimming, environment injection, tool output management, and runtime compaction into a unified system.

All context engineering features are **disabled by default** (`context_budget_tokens = 0`). Set a non-zero budget or enable `auto_budget = true` to activate the pipeline.

## Configuration

```toml
[memory]
context_budget_tokens = 128000    # Set to your model's context window size (0 = unlimited)
deferred_apply_threshold = 0.70   # Apply pre-computed tool pair summaries when usage exceeds this (must be < compaction_threshold)
compaction_threshold = 0.75       # Compact when usage exceeds this fraction
compaction_preserve_tail = 4      # Keep last N messages during compaction
prune_protect_tokens = 40000      # Protect recent N tokens from Tier 1 tool output pruning
cross_session_score_threshold = 0.35  # Minimum relevance for cross-session results (0.0-1.0)
tool_call_cutoff = 6              # Summarize oldest tool pair when visible pairs exceed this

[memory.semantic]
enabled = true                    # Required for semantic recall
recall_limit = 5                  # Max semantically relevant messages to inject

[memory.routing]
strategy = "heuristic"            # Query-aware memory backend selection

[memory.compression]
strategy = "proactive"            # "reactive" (default) or "proactive"
threshold_tokens = 80000          # Proactive: fire when context exceeds this (>= 1000)
max_summary_tokens = 4000         # Proactive: summary cap (>= 128)

[tools]
summarize_output = false          # Enable LLM-based tool output summarization
```

## Context Window Layout

When `context_budget_tokens > 0`, the context window is structured as:

```text
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
│ [knowledge graph] entity facts (if graph on)    │  3% of available
├─────────────────────────────────────────────────┤
│ <code_context> RAG chunks (if index on)         │  30% of available
├─────────────────────────────────────────────────┤
│ [semantic recall] relevant past messages        │  5-8% of available
├─────────────────────────────────────────────────┤
│ [known facts] graph entity-relationship facts   │  0-4% of available
├─────────────────────────────────────────────────┤
│ [compaction summary] if compacted               │  200-500 tokens
├─────────────────────────────────────────────────┤
│ Recent message history                          │  50-60% of available
├─────────────────────────────────────────────────┤
│ [reserved for response generation]              │  20% of total
└─────────────────────────────────────────────────┘
```

## Parallel Context Preparation

Context sources (summaries, cross-session recall, semantic recall, code RAG) are fetched concurrently via `tokio::try_join!`, reducing context build latency to the slowest single source rather than the sum of all.

## Proportional Budget Allocation

Available tokens (after reserving 20% for response) are split proportionally. When [code indexing](code-indexing.md) is enabled, the code context slot takes a share from summaries, recall, and history. When [graph memory](../concepts/graph-memory.md) is enabled, an additional 4% is allocated for graph facts, reducing summaries, semantic recall, cross-session, and code context by 1% each:

| Allocation | Without code index | With code index | With graph memory | Purpose |
|-----------|-------------------|-----------------|-------------------|---------|
| Summaries | 15% | 8% | 7% | Conversation summaries from SQLite |
| Semantic recall | 25% | 8% | 7% | Relevant messages from past conversations via Qdrant |
| Cross-session | -- | 4% | 3% | Messages from other conversations |
| Code context | -- | 30% | 29% | Retrieved code chunks from project index |
| Graph facts | -- | -- | 4% | Entity-relationship facts from graph memory |
| Recent history | 60% | 50% | 50% | Most recent messages in current conversation |

> **Note:** The "With graph memory" column assumes code indexing is also enabled. Graph facts receive 0 tokens when the `graph-memory` feature is disabled or `[memory.graph] enabled = false`.

## Semantic Recall Injection

When semantic memory is enabled, the agent queries the vector backend for messages relevant to the current user query. Two optional post-processing stages improve result quality:

- **Temporal decay** — exponential score attenuation based on message age. Configure via `memory.semantic.temporal_decay_enabled` and `temporal_decay_half_life_days` (default: 30).
- **MMR re-ranking** — Maximal Marginal Relevance diversifies results by penalizing similarity to already-selected items. Configure via `memory.semantic.mmr_enabled` and `mmr_lambda` (default: 0.7, range 0.0-1.0).

Results are injected as transient system messages (prefixed with `[semantic recall]`) that are:

- Removed and re-injected on every turn (never stale)
- Not persisted to SQLite
- Bounded by the allocated token budget (25%, or 10% when [code indexing](code-indexing.md) is enabled)

Requires Qdrant and `memory.semantic.enabled = true`.

## Message History Trimming

When recent messages exceed the 60% budget allocation, the oldest non-system messages are evicted. The system prompt and most recent messages are always preserved.

## Environment Context

Every system prompt rebuild injects an `<environment>` block with:

- Working directory
- OS (linux, macos, windows)
- Current git branch (if in a git repo)
- Active model name

`EnvironmentContext` is built once at agent bootstrap and cached. On skill hot-reload, only `git_branch` and `model_name` are refreshed. This avoids spawning a git subprocess on every agent turn.

## Tool-Pair Summarization

After each tool execution, `maybe_summarize_tool_pair()` checks whether the number of unsummarized tool call/response pairs exceeds `tool_call_cutoff` (default: 6). When the threshold is exceeded, the oldest eligible pair is summarized via LLM and the result is stored as a deferred summary. Summaries are applied lazily when context usage exceeds `deferred_apply_threshold` (default: 0.70), preserving the message prefix for API cache hits.

### How It Works

1. `count_unsummarized_pairs()` scans for consecutive Assistant(`ToolUse`) + User(`ToolResult`/`ToolOutput`) pairs where both have `agent_visible = true` and no `deferred_summary` is pending.
2. If the count exceeds `tool_call_cutoff`, `find_oldest_unsummarized_pair()` locates the first eligible pair (skipping pairs with pruned content).
3. `build_tool_pair_summary_prompt()` constructs a prompt with XML-delimited sections (`<tool_request>` and `<tool_response>`) to prevent content injection.
4. The summary provider generates a 1-2 sentence summary capturing tool name, key parameters, and outcome.
5. The summary is stored in `messages[resp_idx].metadata.deferred_summary` — the original messages remain visible.
6. When context usage exceeds `deferred_apply_threshold`, `apply_deferred_summaries()` batch-applies all pending summaries: hides the original pairs and inserts Assistant `Summary` messages.

### Visibility After Summarization

| Message | `agent_visible` | `user_visible` | Appears in |
|---------|-----------------|----------------|------------|
| Original tool request | `false` | `true` | UI only |
| Original tool response | `false` | `true` | UI only |
| `[tool summary]` message | `true` | `false` | LLM context only |

Summarization runs synchronously between tool iterations. If the LLM call fails, the error is logged and the pair is left unsummarized.

## Query-Aware Memory Routing

When semantic memory is enabled, the `MemoryRouter` trait decides which backend(s) to query for each recall request. The default `HeuristicRouter` classifies queries based on lexical cues:

- **Keyword** (SQLite FTS5 only) — code patterns (`::`, `/`), pure `snake_case` identifiers, short queries (<=3 words without question words)
- **Semantic** (Qdrant vectors only) — natural language questions (`what`, `how`, `why`, ...), long queries (>=6 words)
- **Hybrid** (both + reciprocal rank fusion) — medium-length queries without clear signals
- **Graph** (graph store + hybrid fallback) — relationship patterns (`related to`, `opinion on`, `connection between`, `know about`). Triggers `graph_recall` BFS traversal in addition to hybrid message recall. Requires the `graph-memory` feature; falls back to Hybrid when disabled

Relationship patterns take priority over all other heuristics.

Configure via `[memory.routing]`:

```toml
[memory.routing]
strategy = "heuristic"   # Only option currently; selected by default
```

When Qdrant is unavailable, Semantic-route queries return empty results and Hybrid-route queries fall back to FTS5 only.

## Proactive Context Compression

By default, context compression is **reactive** — it fires only when the two-tier pruning pipeline detects threshold overflow. Proactive compression fires earlier, based on an absolute token count threshold, to prevent overflow altogether.

```toml
[memory.compression]
strategy = "proactive"
threshold_tokens = 80000       # Compress when context exceeds this (>= 1000)
max_summary_tokens = 4000      # Cap for the compressed summary (>= 128)
```

Proactive compression runs at the start of the context management phase, before reactive compaction. If proactive compression fires, reactive compaction is skipped for that turn (mutual exclusion via `compacted_this_turn` flag, reset each turn).

Metrics: `compression_events` (count), `compression_tokens_saved` (cumulative tokens freed).

## Three-Tier Context Pruning

When context usage crosses predefined thresholds, a three-tier pruning strategy activates in order. Each tier is attempted before escalating to the next.

### Tier 0: Apply Deferred Summaries (at `deferred_apply_threshold`)

When context usage exceeds `deferred_apply_threshold` (default: 0.70), all pending deferred summaries are batch-applied. This is a purely in-memory operation — no LLM call is required, since summaries were pre-computed after each tool execution.

**Why lazy application?** Tool pair summaries are computed eagerly (right after each tool call) but their application to the message array is deferred. As long as context usage stays below 0.70, the original tool call/response messages remain in the array unchanged. This keeps the message prefix stable across consecutive turns, which is the key requirement for the Claude API prompt cache to produce hits. Applying summaries only when space is actually needed ensures the cache prefix is not disturbed unnecessarily.

### Tier 1: Selective Tool Output Pruning (at `compaction_threshold`)

When context usage exceeds `compaction_threshold` (default: 0.75) and Tier 0 did not free enough space, Zeph scans messages outside the protected tail for `ToolOutput` parts and replaces their content with a short placeholder. This is a cheap, synchronous operation that often frees enough tokens to stay under the threshold without an LLM call.

- Only tool outputs in messages older than the protected tail are pruned
- The most recent `prune_protect_tokens` tokens (default: 40,000) worth of messages are never pruned, preserving recent tool context
- Pruned parts have their `compacted_at` timestamp set, body is cleared from memory to reclaim heap, and they are not pruned again
- Pruned parts are persisted to SQLite before clearing, so pruning state survives session restarts
- The `tool_output_prunes` metric tracks how many parts were pruned

### Tier 2: Chunked LLM Compaction (Fallback)

If Tier 1 does not free enough tokens, adaptive chunked compaction runs:

1. Middle messages (between system prompt and last N recent) are split into ~4096-token chunks
2. Chunks are summarized in parallel via `futures::stream::buffer_unordered(4)` — up to 4 concurrent LLM calls
3. Partial summaries are merged into a final summary via a second LLM pass
4. `replace_conversation()` atomically updates the compacted range and inserts the summary in SQLite
5. Last `compaction_preserve_tail` messages (default: 4) are always preserved

If a single chunk fits all messages, or if chunked summarization fails, the system falls back to a single-pass summarization over the full message range.

Both tiers are idempotent and run automatically during the agent loop.

### Structured Compaction Prompt

Compaction summaries use a 9-section structured prompt designed for self-consumption. The LLM is instructed to produce exactly these sections:

1. **User Intent** — what the user is ultimately trying to accomplish
2. **Technical Concepts** — key technologies, patterns, constraints discussed
3. **Files & Code** — file paths, function names, structs, enums touched or relevant
4. **Errors & Fixes** — every error encountered and whether/how it was resolved
5. **Problem Solving** — approaches tried, decisions made, alternatives rejected
6. **User Messages** — verbatim user requests that are still pending or relevant
7. **Pending Tasks** — items explicitly promised or left TODO
8. **Current Work** — the exact task in progress at the moment of compaction
9. **Next Step** — the single most important action to take immediately after compaction

The prompt favors thoroughness over brevity: longer summaries that preserve actionable detail are preferred over terse ones. When multiple chunks are summarized in parallel, a consolidation pass merges partial summaries into the same 9-section structure.

### Progressive Tool Response Removal

When the LLM compaction itself hits a context length error (the messages being compacted are too large for the summarization model), `summarize_messages()` applies progressive middle-out tool response removal before retrying:

| Tier | Fraction removed | Description |
|------|-----------------|-------------|
| 1 | 10% | Remove ~10% of tool responses from the center outward |
| 2 | 20% | Increase removal to ~20% |
| 3 | 50% | Remove half of all tool responses |
| 4 | 100% | Remove all tool responses |

The **middle-out** strategy starts removal from the center of the tool response list and alternates outward toward the edges. This preserves the earliest responses (which establish context) and the most recent ones (which reflect current work), while discarding the middle of the conversation first.

At each tier, `ToolResult` content is replaced with `[compacted]` and `ToolOutput` bodies are cleared (with `compacted_at` timestamp set). The reduced message set is then retried through the LLM summarization pipeline.

### Metadata-Only Fallback

If all LLM summarization attempts fail (including after 100% tool response removal), `build_metadata_summary()` produces a lightweight summary without any LLM call:

```text
[metadata summary — LLM compaction unavailable]
Messages compacted: 47 (23 user, 22 assistant, 2 system)
Last user message: <first 200 chars of last user message>
Last assistant message: <first 200 chars of last assistant message>
```

Text previews use safe UTF-8 truncation (`truncate_chars()`) that never splits a Unicode scalar value. This fallback guarantees that compaction always succeeds, even when the LLM is unreachable or the context is too large for any available model.

## Reactive Retry on Context Length Errors

LLM calls in the agent loop (`call_llm_with_retry()` and `call_chat_with_tools_retry()`) intercept context length errors and automatically compact before retrying. The flow:

1. Send messages to the LLM provider
2. If the provider returns a context length error, trigger `compact_context()`
3. Retry the LLM call with the compacted context
4. If the error persists after `max_attempts` (default: 2), propagate the error

Non-context-length errors (rate limits, network failures, etc.) are propagated immediately without retry.

### Context Length Error Detection

`LlmError::is_context_length_error()` detects context overflow across providers via pattern matching on error messages:

| Provider | Matched patterns |
|----------|-----------------|
| Claude | `"maximum number of tokens"` |
| OpenAI | `"maximum context length"`, `"context_length_exceeded"` |
| Ollama | `"context length exceeded"`, `"prompt is too long"`, `"input too long"` |

The dedicated `LlmError::ContextLengthExceeded` variant is also recognized. This unified detection allows the retry logic to work identically across all supported LLM backends.

### Dual-Visibility Compaction

Compaction is non-destructive. Each `Message` carries `MessageMetadata` with `agent_visible` and `user_visible` flags:

| Message state | `agent_visible` | `user_visible` | Appears in |
|---------------|-----------------|----------------|------------|
| Normal | `true` | `true` | LLM context + UI |
| Compacted original | `false` | `true` | UI only |
| Compaction summary | `true` | `false` | LLM context only |

`replace_conversation()` performs both updates atomically in a single SQLite transaction: it sets `agent_visible=0, compacted_at=<timestamp>` on the compacted range, then inserts the summary with `agent_visible=1, user_visible=0`. This guarantees the user retains full scroll-back history while the LLM sees only the compact summary.

Semantic recall (vector + FTS5) filters by `agent_visible=1`, so compacted originals are excluded from retrieval. Use `load_history_filtered(conversation_id, agent_visible, user_visible)` to query messages by visibility.

## Tool Output Management

### Truncation

Tool outputs exceeding 30,000 characters are automatically truncated using a head+tail split with UTF-8 safe boundaries. Both the first and last ~15K chars are preserved.

### Smart Summarization

When `tools.summarize_output = true`, long tool outputs are sent through the LLM with a prompt that preserves file paths, error messages, and numeric values. On LLM failure, falls back to truncation.

```bash
export ZEPH_TOOLS_SUMMARIZE_OUTPUT=true
```

## Skill Prompt Modes

The `skills.prompt_mode` setting controls how matched skills are rendered in the system prompt:

| Mode | Behavior |
|------|----------|
| `full` | Full XML skill bodies with instructions, examples, and references |
| `compact` | Condensed XML with name, description, and trigger list only (~80% smaller) |
| `auto` (default) | Selects `compact` when the remaining context budget is below 8192 tokens, `full` otherwise |

```toml
[skills]
prompt_mode = "auto"  # "full", "compact", or "auto"
```

`compact` mode is useful for small context windows or when many skills are active. It preserves enough information for the model to select the right skill while minimizing token consumption.

## Progressive Skill Loading

Skills matched by embedding similarity (top-K) are injected with their full body (or compact summary, depending on `prompt_mode`). Remaining skills are listed in a description-only `<other_skills>` catalog — giving the model awareness of all capabilities while consuming minimal tokens.

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
| `ZEPH_MEMORY_DEFERRED_APPLY_THRESHOLD` | Apply deferred summaries when context usage exceeds this fraction | `0.70` |
| `ZEPH_MEMORY_COMPACTION_THRESHOLD` | Compaction trigger threshold | `0.75` |
| `ZEPH_MEMORY_COMPACTION_PRESERVE_TAIL` | Messages preserved during compaction | `4` |
| `ZEPH_MEMORY_PRUNE_PROTECT_TOKENS` | Tokens protected from Tier 1 tool output pruning | `40000` |
| `ZEPH_MEMORY_CROSS_SESSION_SCORE_THRESHOLD` | Minimum relevance score for cross-session memory results | `0.35` |
| `ZEPH_MEMORY_TOOL_CALL_CUTOFF` | Max visible tool pairs before oldest is summarized | `6` |
| `ZEPH_MEMORY_SEMANTIC_TEMPORAL_DECAY_ENABLED` | Enable temporal decay scoring | `false` |
| `ZEPH_MEMORY_SEMANTIC_TEMPORAL_DECAY_HALF_LIFE_DAYS` | Half-life for temporal decay | `30` |
| `ZEPH_MEMORY_SEMANTIC_MMR_ENABLED` | Enable MMR re-ranking | `false` |
| `ZEPH_MEMORY_SEMANTIC_MMR_LAMBDA` | MMR relevance-diversity trade-off | `0.7` |
| `ZEPH_TOOLS_SUMMARIZE_OUTPUT` | Enable LLM-based tool output summarization | `false` |
