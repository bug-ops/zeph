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

OpenAI (or any OpenAI-compatible API):
```bash
export ZEPH_LLM_PROVIDER=openai
export ZEPH_OPENAI_API_KEY=sk-...
```

Config in `config/default.toml`:
```toml
[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-5.2"
max_tokens = 4096
embedding_model = "text-embedding-3-small"
reasoning_effort = "medium"  # low, medium, high (for reasoning models)
```

- `llm.openai.base_url`: API endpoint (change for Together, Groq, Fireworks, etc.)
- `llm.openai.model`: chat model name
- `llm.openai.max_tokens`: max response tokens (default: 4096)
- `llm.openai.embedding_model`: optional, enables embeddings support
- `llm.openai.reasoning_effort`: optional, `low`/`medium`/`high` for reasoning models (o3, etc.)

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
export ZEPH_A2A_REQUIRE_TLS=true
export ZEPH_A2A_SSRF_PROTECTION=true
export ZEPH_A2A_MAX_BODY_SIZE=1048576
```

Rate limit: requests per minute per IP (0 = unlimited).
TLS enforcement: reject HTTP endpoints when `require_tls = true`.
SSRF protection: block private IPs (10.x, 172.16.x, 192.168.x, 127.x) in outbound A2A calls.
Max body size: request payload limit in bytes (default 1 MiB).

## Tools

Config: `tools.enabled` (default: true) — master toggle for all tool execution.

Shell:
```bash
export ZEPH_TOOLS_TIMEOUT=30
export ZEPH_TOOLS_SHELL_ALLOWED_COMMANDS=curl,wget
export ZEPH_TOOLS_SHELL_ALLOWED_PATHS=/home/user/workspace,/tmp
export ZEPH_TOOLS_SHELL_ALLOW_NETWORK=true
```
Config: `tools.shell.blocked_commands` — additional command patterns to block.
Config: `tools.shell.allowed_commands` — commands to remove from the default blocklist.
Config: `tools.shell.allowed_paths` — restrict filesystem access (empty = cwd only).
Config: `tools.shell.allow_network` — `false` blocks curl/wget/nc.
Config: `tools.shell.confirm_patterns` — destructive commands requiring user confirmation.

Audit logging:
```bash
export ZEPH_TOOLS_AUDIT_ENABLED=true
export ZEPH_TOOLS_AUDIT_DESTINATION=./data/audit.jsonl
```

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

## Candle Local Inference

Build with `--features candle` to enable HuggingFace direct inference via candle ML framework. Supports GGUF quantized models.

```bash
export ZEPH_LLM_PROVIDER=candle
```

Config in `config/default.toml`:
```toml
[llm.candle]
source = "huggingface"
repo_id = "TheBloke/Mistral-7B-Instruct-v0.2-GGUF"
filename = "mistral-7b-instruct-v0.2.Q4_K_M.gguf"
chat_template = "mistral"
device = "auto"
embedding_repo = "sentence-transformers/all-MiniLM-L6-v2"

[llm.candle.generation]
temperature = 0.7
max_tokens = 2048
```

Device selection: `auto` picks Metal on macOS, CUDA on Linux with GPU, CPU otherwise. Build with `--features metal` or `--features cuda` for GPU acceleration.

Chat templates: `llama3`, `chatml`, `mistral`, `phi3`, `raw`.

## Model Orchestrator

Build with `--features orchestrator` to enable multi-model routing with task classification and fallback chains.

```bash
export ZEPH_LLM_PROVIDER=orchestrator
```

Config in `config/default.toml`:
```toml
[llm.orchestrator]
default = "ollama"
embed = "ollama"

[llm.orchestrator.providers.ollama]
provider_type = "ollama"

[llm.orchestrator.providers.claude]
provider_type = "claude"

[llm.orchestrator.routes]
coding = ["claude", "ollama"]
creative = ["claude", "ollama"]
general = ["ollama"]
```

Task types: `coding`, `creative`, `analysis`, `translation`, `summarization`, `general`. Each route is a fallback chain — if the first provider fails, the next one is tried.

## Skills

```bash
export ZEPH_SKILLS_MAX_ACTIVE=5
```

Config: `skills.paths` (default: `["./skills"]`). Top-K skills selected per query via embedding similarity. File changes detected automatically (hot-reload).

## Security

Secret redaction:
```bash
export ZEPH_SECURITY_REDACT_SECRETS=true
```

Scans LLM responses for API keys, tokens, passwords, and private keys. Replaces detected secrets with `[REDACTED]`.

Timeouts:
```bash
export ZEPH_TIMEOUT_LLM=120
export ZEPH_TIMEOUT_EMBEDDING=30
export ZEPH_TIMEOUT_A2A=30
```

Config: `timeouts.llm_seconds`, `timeouts.embedding_seconds`, `timeouts.a2a_seconds`.
