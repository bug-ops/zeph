---
name: setup-guide
category: system
description: >
  Zeph configuration reference for LLM providers, memory, tools, channels, and features.
  Use when the user asks about setup, configuration, environment variables, TOML settings,
  or how to enable specific features like Telegram, Qdrant, A2A, MCP, Candle, or orchestrator.
  Keywords: config, setup, install, configure, toml, environment, provider, feature, enable.
metadata:
  author: zeph
  version: "1.0"
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
export ZEPH_SQLITE_PATH=.zeph/data/zeph.db
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
export ZEPH_A2A_MAX_BODY_SIZE=1048576
```

Rate limit: requests per minute per IP (0 = unlimited). TLS enforcement and SSRF
protection for outbound A2A connections are configured under `[a2a_client]` below,
not here — this server section has no security-policy fields of its own.

## A2A Client (`--connect`)

```bash
export ZEPH_A2A_CLIENT_REQUIRE_TLS=true
export ZEPH_A2A_CLIENT_SSRF_PROTECTION=true
export ZEPH_A2A_CARD_TRUST_POLICY=ignore
```

Security policy for `zeph --tui --connect <URL>` (remote-TUI attach to another
daemon's `/a2a/stream`). Loopback targets (`127.0.0.1`, `::1`, `localhost`) always
bypass both checks and connect over plain HTTP — this makes
`zeph --tui --connect http://127.0.0.1:8080/a2a/stream` work with a fresh/default
config. Non-loopback targets require HTTPS (`require_tls`) and are DNS-validated to
reject private/loopback ranges (`ssrf_protection`) — RFC 1918 ranges (10.x, 172.16.x,
192.168.x), loopback, and link-local. Max body size: request payload limit in bytes
(default 1 MiB).

`ZEPH_A2A_CARD_TRUST_POLICY` (`ignore` | `prefer` | `require`, default `ignore`)
controls peer AgentCard signature + URL-origin verification during discovery (A2A
1.0.0 §8.4, #5928). `require` needs the crate's `card-signing` feature compiled in
(part of the `a2a` feature bundle) or config validation fails at startup. Trusted
verification keys are configured via `[[a2a_client.trusted_agent_keys]]` in
`config.toml` (no env-var equivalent — see `config/default.toml` for the shape).

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
export ZEPH_TOOLS_AUDIT_DESTINATION=.zeph/data/audit.jsonl
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

MCP tools are discovered at startup, embedded into Qdrant (`zeph_mcp_tools` collection), and matched per query alongside skills. Tool invocations use `mcp` fenced blocks with JSON payloads.

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

Config: `skills.paths` (default: `[".zeph/skills"]`). Top-K skills selected per query via embedding similarity. File changes detected automatically (hot-reload).

## Sub-Agents

Live transcript forwarding — stream a running sub-agent's full per-turn text/thinking to the TUI runtime detail view and/or `--bare` stdout as it happens, instead of only a 120-char once-per-turn snippet:
```bash
export ZEPH_AGENTS_FORWARD_TRANSCRIPT=true
```
Opt-in, default `false` (`agents.forward_transcript` in config). Also settable via `--forward-subagent-text`. No effect unless a consumer surface (`--tui` or `--bare`) is active for the session.

Delegation mode — control whether the main agent may spawn sub-agents, and who may trigger it:
```bash
export ZEPH_AGENTS_DELEGATION_MODE=explicit_request_only
```
One of `disabled` (no spawn from any code path), `explicit_request_only` (only direct user
actions — `/agent spawn`, `/agent resume`, `/subagent spawn` — the main agent's own planner/
scheduler may never spawn autonomously), or `proactive` (default; both explicit and autonomous
spawns allowed). Orthogonal to `agents.enabled`, which remains the outer kill switch:
`enabled = false` always behaves as `disabled` regardless of this setting. Also settable via
`--delegation-mode`. Useful to restrict in semi-trusted channels (Telegram/Discord/webhook
ingestion) where prompt-injected input could otherwise trigger unsupervised delegation.

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
