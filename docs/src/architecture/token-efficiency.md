# Token Efficiency

Zeph's prompt construction is designed to minimize token usage regardless of how many skills and MCP tools are installed.

## The Problem

Naive AI agent implementations inject all available tools and instructions into every prompt. With 50 skills and 100 MCP tools, this means thousands of tokens consumed on every request — most of which are irrelevant to the user's query.

## Zeph's Approach

### Embedding-Based Selection

Per query, only the top-K most relevant skills (default: 5) are selected via cosine similarity of vector embeddings. The same pipeline handles MCP tools.

```text
User query → embed(query) → cosine_similarity(query, skills) → top-K → inject into prompt
```

This makes prompt size **O(K)** instead of **O(N)**, where:
- K = `max_active_skills` (default: 5, configurable)
- N = total skills + MCP tools installed

### Progressive Loading

Even selected skills don't load everything at once:

| Stage | What loads | When | Token cost |
|-------|-----------|------|------------|
| Startup | Skill metadata (name, description) | Once | ~100 tokens per skill |
| Query | Skill body (instructions, examples) | On match | <5000 tokens per skill |
| Query | Resource files (references, scripts) | On match + OS filter | Variable |

Metadata is always in memory for matching. Bodies are loaded lazily via `OnceLock` and cached after first access. Resources are loaded on demand with OS filtering (e.g., `linux.md` only loads on Linux).

### Two-Tier Skill Catalog

Non-matched skills are listed in a description-only `<other_skills>` catalog — giving the model awareness of all available capabilities without injecting their full bodies. This means the model can request a specific skill if needed, while consuming only ~20 tokens per unmatched skill instead of thousands.

### MCP Tool Matching

MCP tools follow the same pipeline:

- Tools are embedded in Qdrant (`zeph_mcp_tools` collection) with BLAKE3 content-hash delta sync
- Only re-embedded when tool definitions change
- Unified matching ranks both skills and MCP tools by relevance score
- Prompt contains only the top-K combined results

### Practical Impact

| Scenario | Naive approach | Zeph |
|----------|---------------|------|
| 10 skills, no MCP | ~50K tokens/prompt | ~25K tokens/prompt |
| 50 skills, 100 MCP tools | ~250K tokens/prompt | ~25K tokens/prompt |
| 200 skills, 500 MCP tools | ~1M tokens/prompt | ~25K tokens/prompt |

Prompt size stays constant as you add more capabilities. The only cost of more skills is a slightly larger embedding index in Qdrant or memory.

### Output Filter Pipeline

Tool output is compressed before it enters the LLM context. A command-aware filter pipeline matches each shell command against a set of built-in filters (test runner output, Clippy diagnostics, git log/diff, directory listings, log deduplication) and strips noise while preserving signal. The pipeline runs synchronously inside the tool executor, so the LLM never sees raw output.

Typical savings by command type:

| Command | Raw lines | Filtered lines | Savings |
|---------|-----------|----------------|---------|
| `cargo test` (100 passing, 2 failing) | ~340 | ~30 | ~91% |
| `cargo clippy` (many warnings) | ~200 | ~50 | ~75% |
| `git log --oneline -50` | 50 | 20 | 60% |

After each filtered execution, CLI mode prints a one-line stats summary and TUI mode accumulates the savings in the Resources panel. See [Tool System — Output Filter Pipeline](../advanced/tools.md#output-filter-pipeline) for configuration details.

### Token Savings Tracking

`MetricsSnapshot` tracks cumulative filter metrics across the session:

- `filter_raw_tokens` / `filter_saved_tokens` — aggregate volume before and after filtering
- `filter_total_commands` / `filter_filtered_commands` — hit rate denominator/numerator
- `filter_confidence_full/partial/fallback` — distribution of filter confidence levels

These feed into the [TUI filter metrics display](../advanced/tui.md#filter-metrics) and are emitted as `tracing::debug!` every 50 commands.

### Two-Tier Context Pruning

Long conversations accumulate tool outputs that consume significant context space. Zeph uses a two-tier strategy: Tier 1 selectively prunes old tool outputs (cheap, no LLM call), and Tier 2 falls back to full LLM compaction only when Tier 1 is insufficient. See [Context Engineering](../advanced/context.md) for details.

## Configuration

```toml
[skills]
max_active_skills = 5  # Increase for broader context, decrease for faster/cheaper queries
```

```bash
export ZEPH_SKILLS_MAX_ACTIVE=3  # Override via env var
```
