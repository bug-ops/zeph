# Token Efficiency

Zeph's prompt construction is designed to minimize token usage regardless of how many skills and MCP tools are installed.

## The Problem

Naive AI agent implementations inject all available tools and instructions into every prompt. With 50 skills and 100 MCP tools, this means thousands of tokens consumed on every request — most of which are irrelevant to the user's query.

## Zeph's Approach

### Embedding-Based Selection

Per query, only the top-K most relevant skills (default: 5) are selected via cosine similarity of vector embeddings. The same pipeline handles MCP tools.

```
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

## Configuration

```toml
[skills]
max_active_skills = 5  # Increase for broader context, decrease for faster/cheaper queries
```

```bash
export ZEPH_SKILLS_MAX_ACTIVE=3  # Override via env var
```
