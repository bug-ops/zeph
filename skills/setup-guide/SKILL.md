---
name: setup-guide
description: Zeph configuration reference. Use when the user asks about setup, configuration, environment variables, TOML settings, or how to enable specific features like Telegram, Qdrant, or A2A.
---
# Setup Guide

## LLM Provider

Ollama (default):
```bash
export ZEPH_LLM_PROVIDER=ollama
export ZEPH_LLM_BASE_URL=http://localhost:11434
export ZEPH_LLM_MODEL=mistral:7b
```

Claude:
```bash
export ZEPH_LLM_PROVIDER=claude
export ZEPH_CLAUDE_API_KEY=sk-ant-...
```

Cloud model settings in `config/default.toml`:
- `llm.cloud.model` (default: `claude-sonnet-4-5-20250929`)
- `llm.cloud.max_tokens` (default: 4096)

## Embeddings

```bash
export ZEPH_LLM_EMBEDDING_MODEL=qwen3-embedding
```

Used for skill matching and semantic memory. Pull model first:
```bash
ollama pull qwen3-embedding
```

## Memory

SQLite storage:
```bash
export ZEPH_SQLITE_PATH=./data/zeph.db
```

Config: `memory.history_limit` (default: 50) — recent messages loaded into context.

## Semantic Memory (Qdrant)

```bash
export ZEPH_MEMORY_SEMANTIC_ENABLED=true
export ZEPH_QDRANT_URL=http://localhost:6334
export ZEPH_MEMORY_RECALL_LIMIT=5
```

Start Qdrant:
```bash
docker compose up -d qdrant
```

When semantic memory is enabled and Qdrant is reachable, skill embeddings are persisted in a `zeph_skills` collection. On startup, only changed skills are re-embedded (BLAKE3 content hash comparison). The Qdrant HNSW index is used for skill matching instead of in-memory cosine similarity. If Qdrant is unavailable, the agent falls back to in-memory matching.

## Summarization

```bash
export ZEPH_MEMORY_SUMMARIZATION_THRESHOLD=100
export ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS=0
```

Threshold: message count before triggering summarization (0 = disabled).
Budget: total token limit for context (0 = unlimited). Split: 15% summaries, 25% recall, 60% recent.

## Telegram Mode

```bash
export ZEPH_TELEGRAM_TOKEN=123456:ABC-DEF...
```

Config: `telegram.token` (prefer env var for security). Access control via `telegram.allowed_users` in config.

## A2A Server

```bash
export ZEPH_A2A_ENABLED=true
export ZEPH_A2A_HOST=0.0.0.0
export ZEPH_A2A_PORT=8080
export ZEPH_A2A_PUBLIC_URL=https://my-agent.example.com
export ZEPH_A2A_AUTH_TOKEN=secret-token
export ZEPH_A2A_RATE_LIMIT=60
```

Rate limit: requests per minute per IP (0 = unlimited).

## Tools

Config: `tools.enabled` (default: true) — master toggle for all tool execution.

Shell:
```bash
export ZEPH_TOOLS_TIMEOUT=30
export ZEPH_TOOLS_SHELL_ALLOWED_COMMANDS=curl,wget
```
Config: `tools.shell.blocked_commands` — additional command patterns to block.
Config: `tools.shell.allowed_commands` — commands to remove from the default blocklist.

Scrape:
```bash
export ZEPH_TOOLS_SCRAPE_TIMEOUT=15
export ZEPH_TOOLS_SCRAPE_MAX_BODY=1048576
```

## MCP (Model Context Protocol)

Build with `--features mcp` to enable MCP tool integration.

Config in `config/default.toml`:
```toml
[[mcp.servers]]
id = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
timeout = 30

[mcp.servers.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_PERSONAL_ACCESS_TOKEN}"
```

MCP tools are discovered at startup, embedded into Qdrant (`zeph_mcp_tools` collection), and matched per query alongside skills. Tool invocations use ` ```mcp ` fenced blocks with JSON payloads.

## Skills

```bash
export ZEPH_SKILLS_MAX_ACTIVE=5
```

Config: `skills.paths` (default: `["./skills"]`). Top-K skills selected per query via embedding similarity. File changes detected automatically (hot-reload).
