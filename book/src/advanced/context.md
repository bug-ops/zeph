# Context Engineering

Zeph's context engineering pipeline manages how information flows into the LLM context window. It combines semantic recall, proportional budget allocation, message trimming, environment injection, tool output management, and runtime compaction into a unified system.

All context engineering features are **disabled by default** (`context_budget_tokens = 0`). Set a non-zero budget or enable `auto_budget = true` to activate the pipeline.

## Configuration

```toml
[memory]
context_budget_tokens = 128000    # Set to your model's context window size (0 = unlimited)
soft_compaction_threshold = 0.60  # Soft tier: prune tool outputs + apply deferred summaries (no LLM)
hard_compaction_threshold = 0.90  # Hard tier: full LLM summarization when usage exceeds this fraction
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

## Context Strategy Modes

The `[memory.context_strategy]` setting controls how Zeph assembles the conversation history portion of the context window.

| Strategy | Behavior |
|----------|----------|
| `full_history` | Always include the full message history, trimmed to budget. This is the default. |
| `memory_first` | Drop raw conversation history; assemble context from summaries, semantic recall, cross-session memory, and session digest only. Useful for long-running assistants where history is a liability. |
| `adaptive` | Start as `full_history`; automatically switch to `memory_first` once the turn count exceeds `crossover_turn_threshold`. |

```toml
[memory]
context_strategy = "adaptive"        # full_history | memory_first | adaptive
crossover_turn_threshold = 20        # switch to memory_first after N turns (adaptive only)
```

`crossover_turn_threshold` defaults to `20`. In `memory_first` mode the semantic recall, cross-session, and digest slots still receive their normal budget allocations, so factual continuity is maintained through retrieval rather than raw history.

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

After each tool execution, `maybe_summarize_tool_pair()` checks whether the number of unsummarized tool call/response pairs exceeds `tool_call_cutoff` (default: 6). When the threshold is exceeded, the oldest eligible pair is summarized via LLM and the result is stored as a deferred summary. Summaries are applied lazily when context usage exceeds `soft_compaction_threshold` (default: 0.60), preserving the message prefix for API cache hits.

### How It Works

1. `count_unsummarized_pairs()` scans for consecutive Assistant(`ToolUse`) + User(`ToolResult`/`ToolOutput`) pairs where both have `agent_visible = true` and no `deferred_summary` is pending.
2. If the count exceeds `tool_call_cutoff`, `find_oldest_unsummarized_pair()` locates the first eligible pair (skipping pairs with pruned content).
3. `build_tool_pair_summary_prompt()` constructs a prompt with XML-delimited sections (`<tool_request>` and `<tool_response>`) to prevent content injection.
4. The summary provider generates a 1-2 sentence summary capturing tool name, key parameters, and outcome.
5. The summary is stored in `messages[resp_idx].metadata.deferred_summary` — the original messages remain visible.
6. When context usage exceeds `soft_compaction_threshold`, `apply_deferred_summaries()` batch-applies all pending summaries: hides the original pairs and inserts Assistant `Summary` messages.

### Visibility After Summarization

| Message | `agent_visible` | `user_visible` | Appears in |
|---------|-----------------|----------------|------------|
| Original tool request | `false` | `true` | UI only |
| Original tool response | `false` | `true` | UI only |
| `[tool summary]` message | `true` | `false` | LLM context only |

Summarization runs synchronously between tool iterations. If the LLM call fails, the error is logged and the pair is left unsummarized.

## TypedPage and ClawVM Context Compaction

During context compaction, Zeph produces pages of different types — tool outputs, conversation turns, memory excerpts, system context — each with distinct fidelity requirements. ClawVM (Compact Low-Alignment View Machine) classifies every compacted page into a `PageType` enum and enforces per-type `PageInvariant` traits at compaction boundaries. This ensures that critical information structures are preserved during summarization.

**Page types and their invariants:**

| Type | Content | Invariant |
|------|---------|-----------|
| `ToolOutput` | Single tool result (bash output, file read, etc.) | No orphaned ToolUse/ToolResult pairs — tool requests and responses remain linked |
| `ConversationTurn` | User or assistant message | Multipart structure intact — text, tool calls, and reasoning blocks stay together |
| `MemoryExcerpt` | Recalled or injected semantic memory | Citation completeness — references to facts or sources remain valid |
| `SystemContext` | Project context (ZEPH.md) + instructions | No truncation of logical sections — guidelines remain self-contained |

**How it works:**

1. **Classification** — as the LLM produces a summary, each output message is tokenized and assigned a `PageType` based on its source
2. **Validation** — before the page enters the SQLite store, `PageInvariant::validate()` is called to check fidelity constraints
3. **Audit logging** — when invariants succeed, an audit record is appended to a bounded async sink, allowing external systems to verify enforcement
4. **Graceful degradation** — if validation fails, the page is either rejected (strict mode) or admitted with a warning flag (permissive mode), depending on `compaction.invariant_mode`

**Configuration:**

```toml
[memory.compaction]
invariant_mode = "permissive"    # "strict" | "permissive" (default: "permissive")
audit_enabled = true             # Log invariant checks to SQLite (default: false)
```

- `strict` — reject pages that fail invariant checks. Compaction may not produce a summary if too many pages are rejected. Use for safety-critical deployments.
- `permissive` — admit pages with failed invariants but flag them with a warning. Ensures compaction always completes. Use for long sessions where occasional information loss is acceptable.

When `audit_enabled = true`, each compaction pass writes invariant check results to the `compaction_audit` table, allowing you to detect which page types are degrading. Query this table to identify patterns where critical information is being lost during compaction.

### Summary Provider Configuration

By default, tool-pair summarization uses the primary LLM provider. You can dedicate a faster or cheaper model to this task using either the structured `[llm.summary_provider]` section or the `summary_model` string shorthand.

#### Structured config (recommended)

`[llm.summary_provider]` uses the same struct as `[[llm.providers]]` entries:

```toml
# Claude — model falls back to the claude provider entry when omitted
[llm.summary_provider]
type = "claude"
model = "claude-haiku-4-5-20251001"

# OpenAI — model/base_url fall back to the openai provider entry when omitted
[llm.summary_provider]
type = "openai"
model = "gpt-4o-mini"

# Ollama — model/base_url fall back to [llm] when omitted
[llm.summary_provider]
type = "ollama"
model = "qwen3:1.7b"
base_url = "http://localhost:11434"

# OpenAI-compatible server — `model` is the entry name in [[llm.providers]]
[[llm.providers]]
name = "lm-studio"
type = "compatible"
base_url = "http://localhost:8080/v1"
model = "llama-3.2-1b"

[llm.summary_provider]
type = "compatible"
model = "lm-studio"   # matches [[llm.providers]] name field

# Local candle inference (requires candle feature)
[llm.summary_provider]
type = "candle"
model = "mistral-7b-instruct"   # HuggingFace repo_id; overrides [llm.candle]
device = "metal"                 # "cpu", "cuda", or "metal"; overrides [llm.candle].device
```

Fields:

| Field | Required | Description |
|-------|----------|-------------|
| `type` | yes | `claude`, `openai`, `compatible`, `ollama`, or `candle` |
| `model` | no | Model name override (for `compatible`: the `[[llm.providers]]` entry name) |
| `base_url` | no | Override endpoint URL (`ollama` and `openai` only) |
| `embedding_model` | no | Override embedding model (`ollama` and `openai` only) |
| `device` | no | Inference device: `cpu`, `cuda`, `metal` (`candle` only) |

#### String shorthand (`summary_model`)

`summary_model` accepts a compact provider/model string. `[llm.summary_provider]` takes precedence when both are set.

```toml
[llm]
summary_model = "claude"                              # Claude with model from the claude provider entry
summary_model = "claude/claude-haiku-4-5-20251001"   # Claude with explicit model
summary_model = "openai"                              # OpenAI with model from the openai provider entry
summary_model = "openai/gpt-4o-mini"                 # OpenAI with explicit model
summary_model = "compatible/my-server"               # OpenAI-compatible using [[llm.providers]] name
summary_model = "ollama/qwen3:1.7b"                  # Ollama with explicit model
summary_model = "candle"                              # Local candle inference
```

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

## Failure-Driven Compression Guidelines

Zeph can learn from its own compaction mistakes using the ACON (Adaptive COmpaction with Notes) mechanism. When `[memory.compression_guidelines]` is enabled:

1. After each hard compaction event, the agent opens a detection window spanning `detection_window_turns` turns.
2. Within that window, every LLM response is scanned for a two-signal pattern: an uncertainty phrase (e.g. "I don't recall", "I'm not sure") **and** a prior-context reference (e.g. "earlier you mentioned", "we discussed"). Both signals must appear together — this two-signal requirement reduces false positives.
3. Confirmed failure pairs (compressed context snapshot + failure reason) are stored in `compression_failure_pairs` in SQLite.
4. A background task wakes every `update_interval_secs` seconds. When the count of unprocessed pairs reaches `update_threshold`, it calls the LLM with a synthesis prompt that includes the current guidelines and the new failure pairs.
5. The LLM produces an updated numbered list of preservation rules. The output is sanitized (prompt injection patterns stripped, length bounded by `max_guidelines_tokens`), then stored atomically using a single `INSERT ... SELECT COALESCE(MAX(version), 0) + 1` statement that eliminates TOCTOU version conflicts.
6. Every subsequent compaction injects the active guidelines inside a `<compression-guidelines>` block, steering the summarizer to preserve previously-lost information categories.

Configuration:

```toml
[memory.compression_guidelines]
enabled = true
update_threshold = 5             # Failure pairs needed to trigger a guidelines update (default: 5)
max_guidelines_tokens = 500      # Token budget for the synthesized guidelines (default: 500)
max_pairs_per_update = 10        # Pairs consumed per update cycle (default: 10)
detection_window_turns = 10      # Turns to watch for context loss after hard compaction (default: 10)
update_interval_secs = 300       # Background updater interval in seconds (default: 300)
max_stored_pairs = 100           # Cleanup threshold for stored failure pairs (default: 100)
```

The feature is opt-in (`enabled = false` by default). When disabled, compression prompts are unchanged and no failure pairs are recorded. Guidelines accumulate incrementally across sessions — the agent improves its compression behavior over time.

## Focus Agent

The Focus Agent introduces a lightweight task-scoping mechanism using two tools injected into the LLM's tool set: `start_focus` and `complete_focus`. When the agent calls `start_focus`, it records a task goal and a Knowledge block. The Knowledge block persists across subsequent turns, keeping relevant context visible without filling the full history. When the agent calls `complete_focus`, it marks the task done and archives the Knowledge block.

Focus prevents context bloat on long multi-step tasks by giving the agent an explicit workspace. The agent is prompted to start a focus after `compression_interval` turns without one, and reminded every `reminder_interval` turns if a focus is overdue.

```toml
[agent.focus]
enabled               = false  # disable or enable focus tools
compression_interval  = 12     # suggest focus after N turns without one
reminder_interval     = 15     # remind every N turns when overdue
min_messages_per_focus = 8     # minimum message count before suggesting
max_knowledge_tokens  = 4096   # token budget for the Knowledge block
```

Enable or disable per-session with `--focus` / `--no-focus` flags.

## Two-Tier Reactive Compaction

When context usage crosses predefined thresholds, a two-tier compaction strategy activates. Each tier is cheaper than the next. Tier 0 (eager deferred summaries) runs continuously during tool loops independently of these tiers.

### Soft Tier: Apply Deferred Summaries + Prune Tool Outputs (at `soft_compaction_threshold`)

When context usage exceeds `soft_compaction_threshold` (default: 0.60), Zeph first batch-applies all pending deferred summaries (in-memory, no LLM call), then prunes tool outputs outside the protected tail. This tier does **not** prevent the hard tier from firing in the same turn.

The soft tier also fires mid-iteration inside tool execution loops (via `maybe_soft_compact_mid_iteration()`), after summarization and stale pruning. This prevents large tool outputs from pushing context past the hard threshold within a single LLM turn without touching turn counters or cooldown.

**Why lazy application?** Tool pair summaries are computed eagerly (right after each tool call) but their application to the message array is deferred. As long as context usage stays below 0.60, the original tool call/response messages remain in the array unchanged. This keeps the message prefix stable across consecutive turns, which is the key requirement for the Claude API prompt cache to produce hits.

### Hard Tier: Selective Tool Output Pruning + LLM Compaction (at `hard_compaction_threshold`)

When context usage exceeds `hard_compaction_threshold` (default: 0.90), Zeph applies deferred summaries, prunes tool outputs, and — if pruning is insufficient — falls back to full LLM-based chunked compaction. Once hard compaction fires, it sets `compacted_this_turn` to prevent double LLM summarization.

Zeph scans messages outside the protected tail for `ToolOutput` parts and replaces their content with a short placeholder. This is a cheap, synchronous operation that often frees enough tokens to stay under the threshold without an LLM call.

- Only tool outputs in messages older than the protected tail are pruned
- The most recent `prune_protect_tokens` tokens (default: 40,000) worth of messages are never pruned, preserving recent tool context
- Pruned parts have their `compacted_at` timestamp set, body is cleared from memory to reclaim heap, and they are not pruned again
- Pruned parts are persisted to SQLite before clearing, so pruning state survives session restarts
- The `tool_output_prunes` metric tracks how many parts were pruned

### Chunked LLM Compaction (Hard Tier Fallback)

If Tier 1 does not free enough tokens, adaptive chunked compaction runs:

1. Middle messages (between system prompt and last N recent) are split into ~4096-token chunks
2. Chunks are summarized in parallel via `futures::stream::buffer_unordered(4)` — up to 4 concurrent LLM calls
3. Partial summaries are merged into a final summary via a second LLM pass
4. `replace_conversation()` atomically updates the compacted range and inserts the summary in SQLite
5. Last `compaction_preserve_tail` messages (default: 4) are always preserved

If a single chunk fits all messages, or if chunked summarization fails, the system falls back to a single-pass summarization over the full message range.

Both tiers are idempotent and run automatically during the agent loop.

### Compression Archive Mode

Three additional knobs in `[memory.compression]` control how tool outputs are preserved and how token budget is distributed during compaction:

| Field | Default | Description |
|-------|---------|-------------|
| `archive_tool_outputs` | `false` | When `true`, tool output bodies are written to an overflow file with a postfix reference instead of being discarded during compaction, so the agent can reload them if needed. |
| `high_density_budget` | `0.7` | Fraction of the compaction token budget allocated to high-density content (code, tool results, structured data); must sum to `1.0` with `low_density_budget`. |
| `low_density_budget` | `0.3` | Fraction allocated to low-density content (prose, reasoning, conversational turns); must sum to `1.0` with `high_density_budget`. |
| `focus_scorer_provider` | `""` | Named provider used for segment scoring in the Focus compression strategy; empty string falls back to the primary provider. |

```toml
[memory.compression]
archive_tool_outputs = false
high_density_budget = 0.7
low_density_budget = 0.3
focus_scorer_provider = "fast"  # optional: use a cheaper model for scoring
```

### Post-Compression Validation (Compaction Probe)

After hard-tier LLM compaction produces a candidate summary, an optional
validation step can verify that the summary preserves critical facts before
committing it. The compaction probe generates factual questions from the
original messages, answers them using only the summary, and scores the
answers. The probe runs only during hard-tier compaction events — soft-tier
pruning and deferred summaries are not validated.

The feature is disabled by default (`[memory.compression.probe] enabled = false`).
On errors or timeouts, the probe fails open — compaction proceeds without
validation.

#### How It Works

1. After `summarize_messages()` produces a summary, the probe generates up to
   `max_questions` factual questions from the original messages. Tool output
   bodies are truncated to 500 characters to focus on decisions and outcomes.
2. Questions target concrete details: file paths, function/struct names,
   architectural decisions, config values, error messages, and action items.
3. A second LLM call answers the questions using ONLY the summary text.
   If information is absent from the summary, the model answers "UNKNOWN".
4. Answers are scored against expected values using token-set-ratio similarity
   (Jaccard-based with substring boost). Refusal patterns ("unknown",
   "not mentioned", "n/a", etc.) score 0.0.
5. The average score determines the verdict.

If the probe generates fewer than 2 questions (e.g., very short conversations
with insufficient factual content), the probe is skipped and compaction
proceeds without validation.

#### Verdict Behavior

| Verdict | Score Range (defaults) | Action | Metric incremented |
|---------|----------------------|--------|-------------------|
| Pass | >= 0.60 | Commit summary | `compaction_probe_passes` |
| SoftFail | [0.35, 0.60) | Commit summary + WARN log | `compaction_probe_soft_failures` |
| HardFail | < 0.35 | Block compaction, preserve original messages | `compaction_probe_failures` |
| Error | N/A (LLM/timeout) | Non-blocking, proceed with compaction | `compaction_probe_errors` |

When HardFail blocks compaction, the outcome is `ProbeRejected`. This sets an
internal cooldown but does NOT trigger the `Exhausted` state — the compactor
can retry on a later turn with new messages.

#### User-Facing Messages

- **During probe**: status indicator shows "Validating compaction quality..."
- **HardFail** (via `/compact`): "Compaction rejected: summary quality below
  threshold. Original context preserved."
- **SoftFail**: warning in logs only; user sees normal "Context compacted
  successfully."
- **Pass**: normal "Context compacted successfully."

#### Configuration

```toml
[memory.compression.probe]
enabled = false           # Enable compaction probe validation (default: false)
model = ""                # Model for probe LLM calls (empty = summary provider)
threshold = 0.6           # Minimum score to pass without warnings
hard_fail_threshold = 0.35 # Score below this blocks compaction (HardFail)
max_questions = 3         # Maximum factual questions per probe
timeout_secs = 15         # Timeout for the entire probe (both LLM calls)
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `false` | Enable probe validation after each hard compaction |
| `model` | string | `""` | Model override for probe LLM calls. Empty = use summary provider. Non-Haiku models increase cost (~10x) |
| `threshold` | float | `0.6` | Minimum average score for Pass verdict |
| `hard_fail_threshold` | float | `0.35` | Score below this triggers HardFail (blocks compaction) |
| `max_questions` | integer | `3` | Number of factual questions generated per probe |
| `timeout_secs` | integer | `15` | Timeout for both LLM calls combined |

**Threshold tuning:**

- Decrease `threshold` to 0.45-0.50 for creative or conversational sessions
  where verbatim detail preservation matters less.
- Raise `threshold` to 0.75-0.80 for coding sessions where file paths and
  architectural decisions must survive compaction.
- Keep a gap of at least 0.15-0.20 between `hard_fail_threshold` and
  `threshold` to maintain a meaningful SoftFail range.
- `max_questions = 3` balances probe accuracy against latency and cost.
  Increase to 5 for higher statistical power at the expense of slower probes.

#### Debug Dump Output

When [debug dump](debug-dump.md) is enabled, each probe writes a
`{id:04}-compaction-probe.json` file with the full probe result:

```json
{
  "score": 0.75,
  "threshold": 0.6,
  "hard_fail_threshold": 0.35,
  "verdict": "Pass",
  "model": "claude-haiku-4-5-20251001",
  "duration_ms": 2340,
  "questions": [
    {
      "question": "What file was modified to fix the auth bug?",
      "expected": "crates/zeph-core/src/auth.rs",
      "actual": "The file crates/zeph-core/src/auth.rs was modified",
      "score": 1.0
    }
  ]
}
```

The `questions` array merges question text, expected answer, actual LLM answer,
and per-question score into a single object per question for easy inspection.

#### Troubleshooting

**Frequent HardFail verdicts**

- The summary model may be too small for the conversation complexity.
  Try a larger model via `model = "claude-sonnet-4-5-20250514"` (higher cost).
- Lower `hard_fail_threshold` if false negatives are common (probe is too strict).
- Increase `max_questions` to 5 for more statistical power (increases latency).

**Probe always returns SoftFail**

- Check debug dump: if per-question scores show one strong and one weak answer,
  the summary may be partially lossy. This is expected behavior — SoftFail
  means "good enough" and does not block compaction.
- Consider enabling [Failure-Driven Compression Guidelines](#failure-driven-compression-guidelines)
  to teach the summarizer what to preserve.

**Probe timeout warnings**

- Default 15s should be sufficient for most models. Increase `timeout_secs`
  for slow providers (e.g., local Ollama with large models).
- On timeout, compaction proceeds without validation (fail-open).

**Performance considerations**

- Each probe makes 2 LLM calls (question generation + answer verification).
- With Haiku: ~$0.001-0.003 per probe, 1-3 seconds latency.
- With Sonnet: ~$0.01-0.03 per probe, 2-5 seconds latency.
- Probes run only during hard compaction events, not on every turn.
- The probe timeout does not affect the main agent loop — it only gates
  whether the compaction summary is committed.

#### Metrics

| Metric | Description |
|--------|-------------|
| `compaction_probe_passes` | Total Pass verdicts |
| `compaction_probe_soft_failures` | Total SoftFail verdicts |
| `compaction_probe_failures` | Total HardFail verdicts (compaction blocked) |
| `compaction_probe_errors` | Total Error verdicts (LLM/timeout, non-blocking) |
| `last_probe_verdict` | Most recent verdict (Pass/SoftFail/HardFail/Error) |
| `last_probe_score` | Most recent probe score in [0.0, 1.0] |

### Compaction Loop Prevention

`maybe_compact()` tracks whether compaction is making progress. The `compaction_exhausted` flag is set permanently when any of the following conditions are detected after a hard-tier attempt:

- Fewer than 2 messages are eligible for compaction (nothing useful to summarize).
- The LLM summary consumes as many tokens as were freed — net reduction is zero.
- Context usage remains above `hard_compaction_threshold` even after a successful summarization pass.

Once exhausted, all further compaction calls are skipped for the session. A one-time warning is emitted to the user channel and to the log (`WARN` level):

```
Warning: context budget is too tight — compaction cannot free enough space.
Consider increasing [memory] context_budget_tokens or starting a new session.
```

This prevents infinite compaction loops when the configured budget is smaller than the minimum required for the system prompt and response reservation combined.

### Structured Anchored Summarization

When hard compaction fires, the summarizer can produce structured `AnchoredSummary` objects with five mandatory sections:

| Section | Content |
|---------|---------|
| `session_intent` | What the user is trying to accomplish |
| `files_modified` | File paths, function names, structs touched |
| `decisions_made` | Architectural decisions with rationale |
| `open_questions` | Unresolved items or ambiguities |
| `next_steps` | Concrete actions to take immediately |

Anchored summaries are validated for completeness (`session_intent` and `next_steps` must be non-empty) and rendered as Markdown with `[anchored summary]` headers. This structured format reduces information loss compared to the free-form 9-section prompt below.

### Subgoal-Aware Compaction

When task orchestration is active, the `SubgoalRegistry` tracks which messages belong to each subgoal and their state (Active, Completed, Abandoned). During hard compaction:

- Messages in **active** subgoal ranges are preserved unconditionally
- Messages in **completed** subgoal ranges are aggressively compacted
- The registry state is dumped alongside each compaction event when debug dump is enabled (`{id:04}-subgoal-registry.txt`)

This prevents compaction from destroying the context that an in-progress orchestration task depends on.

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

## Native `compress_context` Tool

When the `context-compression` feature is enabled, Zeph registers a `compress_context` native tool that the model can invoke explicitly to trigger context compression on demand — without waiting for the automatic threshold-based compaction pipeline to fire.

The tool supports two compression strategies:

| Strategy | Behavior |
|----------|----------|
| `Reactive` | Apply pending deferred summaries and prune old tool outputs (no LLM call). Equivalent to a soft-tier compaction triggered on demand. |
| `Autonomous` | Run full LLM-based chunked compaction immediately, regardless of current token usage. The model decides when to invoke this based on its own assessment of context quality. |

`Autonomous` mode uses the `compress_provider` for the summarization call. Configure it in `[memory.compression]`:

```toml
[memory.compression]
compress_provider = "fast"   # Provider name for autonomous compress_context calls
```

When `compress_provider` is unset, the default LLM provider is used. The `compress_context` tool does not appear in the tool catalog when the `context-compression` feature is disabled at build time.

**Invocation:**

The model calls the tool with a `strategy` parameter:

```json
{ "strategy": "Autonomous" }
```

After execution, the tool returns a summary of tokens freed and the compaction outcome. The result is visible in the chat panel and in the debug dump.

## Tool Output Management

### SideQuest Eviction

The SideQuest eviction system (`[memory.sidequest]`) uses an LLM to identify and remove tool output chains that are no longer relevant to the main task. It runs periodically during the agent loop and evicts stale "side-thread" tool output segments — for example, exploratory searches or dead-end investigations that no longer contribute to the current goal.

**How it works:** Every `interval_turns` user turns, the eviction pass scores tool output groups (cursors) against the current conversation goal. Groups below the relevance threshold and above `min_cursor_tokens` are candidates for eviction. At most `max_eviction_ratio` of all cursors are evicted per pass.

```toml
[memory.sidequest]
enabled = false                  # Enable LLM-based side-thread eviction (default: false)
interval_turns = 4               # Run eviction every N user turns
max_eviction_ratio = 0.5         # Maximum fraction of tool output cursors to evict per pass
max_cursors = 10                 # Maximum number of cursors to evaluate per pass
min_cursor_tokens = 100          # Exclude tool outputs smaller than this from eviction candidates
```

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

## Session Digest and Shutdown Summary

### Session Digest

A session digest is a concise LLM-generated summary of the current session, produced at session end and stored in the vector store. On the next session start it is retrieved and injected into context, providing continuity even when the conversation history is trimmed or replaced by `memory_first` strategy.

```toml
[memory.digest]
enabled = false              # Enable session digest generation at session end (default: false)
provider = ""                # Provider name from [[llm.providers]]; falls back to primary when empty
max_tokens = 500             # Maximum tokens for the digest text
max_input_messages = 50      # Maximum messages fed into the digest prompt
```

Digests complement hard-compaction summaries: they cover sessions that ended cleanly without ever triggering compaction. When a session digest already exists for a conversation (from a previous compaction), a new digest is not generated.

### Shutdown Summary

On clean agent shutdown, Zeph can generate a short LLM summary of the session and store it in the vector store. This enables cross-session semantic recall for conversations that were too short to trigger hard compaction — such as quick one-off queries.

```toml
[memory]
shutdown_summary = true                 # Generate a summary on clean shutdown (default: true)
shutdown_summary_min_messages = 4       # Minimum user turns before a shutdown summary is generated
shutdown_summary_max_messages = 20      # Maximum recent messages sent to the LLM for summarization
shutdown_summary_timeout_secs = 10      # Per-attempt timeout for the LLM call
```

The shutdown summary is stored with the same schema as compaction summaries and is retrievable in future sessions via cross-session semantic recall. Sessions with fewer than `shutdown_summary_min_messages` user turns are considered trivial and skipped.

## Lifelong Memory Consolidation

The consolidation sweep (`[memory.consolidation]`) is a background loop that periodically clusters semantically similar memories and merges duplicate or contradictory entries via an LLM call. This keeps the long-term memory store clean and reduces redundancy without deleting history — original messages are marked consolidated and deprioritized in recall via temporal decay.

**How it works:**

1. The background loop wakes every `sweep_interval_secs` seconds.
2. It loads up to `sweep_batch_size` messages and clusters those with cosine similarity above `similarity_threshold`.
3. For each cluster, an LLM call proposes a topology operation (merge, supersede, or link). Operations with LLM-assigned confidence below `confidence_threshold` are discarded.
4. Accepted operations are applied: a new consolidated entry is created and originals are flagged so they rank lower in future recall.

```toml
[memory.consolidation]
enabled = false                  # Enable the consolidation background loop (default: false)
consolidation_provider = ""      # Provider name from [[llm.providers]]; falls back to primary
confidence_threshold = 0.7       # Minimum LLM confidence for a topology operation to be applied
sweep_interval_secs = 3600       # How often the sweep runs, in seconds
sweep_batch_size = 50            # Maximum messages evaluated per sweep cycle
similarity_threshold = 0.85      # Minimum cosine similarity for two messages to be candidates
```

Requires Qdrant (vector backend must be enabled). Originals are never deleted from SQLite — only their recall priority is reduced.

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS` | Context budget in tokens | `0` (unlimited) |
| `ZEPH_MEMORY_SOFT_COMPACTION_THRESHOLD` | Soft compaction threshold: prune tool outputs + apply deferred summaries (no LLM) | `0.60` |
| `ZEPH_MEMORY_COMPACTION_THRESHOLD` | Hard compaction threshold (backward compat alias for `hard_compaction_threshold`) | `0.90` |
| `ZEPH_MEMORY_COMPACTION_PRESERVE_TAIL` | Messages preserved during compaction | `4` |
| `ZEPH_MEMORY_PRUNE_PROTECT_TOKENS` | Tokens protected from Tier 1 tool output pruning | `40000` |
| `ZEPH_MEMORY_CROSS_SESSION_SCORE_THRESHOLD` | Minimum relevance score for cross-session memory results | `0.35` |
| `ZEPH_MEMORY_TOOL_CALL_CUTOFF` | Max visible tool pairs before oldest is summarized | `6` |
| `ZEPH_MEMORY_SEMANTIC_TEMPORAL_DECAY_ENABLED` | Enable temporal decay scoring | `false` |
| `ZEPH_MEMORY_SEMANTIC_TEMPORAL_DECAY_HALF_LIFE_DAYS` | Half-life for temporal decay | `30` |
| `ZEPH_MEMORY_SEMANTIC_MMR_ENABLED` | Enable MMR re-ranking | `false` |
| `ZEPH_MEMORY_SEMANTIC_MMR_LAMBDA` | MMR relevance-diversity trade-off | `0.7` |
| `ZEPH_TOOLS_SUMMARIZE_OUTPUT` | Enable LLM-based tool output summarization | `false` |
