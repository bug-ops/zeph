# Configuration Recipes

Copy-paste configs for the most common Zeph setups. Each recipe shows only the sections that
differ from the defaults — paste them into a new `config.toml` and run:

```bash
zeph --config config.toml
```

> **Tip:** Run `zeph init` for an interactive wizard that generates the config file for you.
> These recipes are for when you want to start from a known baseline or understand what each
> setting does.

## Which recipe do I need?

| I want to… | Recipe |
|---|---|
| Try Zeph with no accounts or cloud services | [1. Minimal local (Ollama)](#1-minimal-local-ollama) |
| Use Claude API for best quality | [2. Full cloud — Claude](#2-full-cloud--claude) |
| Use OpenAI API | [3. Full cloud — OpenAI](#3-full-cloud--openai) |
| Use Groq, Together, vLLM, or another compatible API | [4. Compatible provider](#4-compatible-provider) |
| Keep Ollama as primary, fall back to Claude on failure | [5. Hybrid: Ollama + Claude fallback](#5-hybrid-ollama--claude-fallback) |
| Run multi-step agentic workflows locally | [6. Orchestrator for complex tasks](#6-orchestrator-for-complex-tasks) |
| Code assistant with LSP and code search | [7. Coding assistant](#7-coding-assistant) |
| Run a Telegram bot | [8. Telegram bot](#8-telegram-bot) |
| No internet at all, maximum privacy | [9. Privacy-first (fully local)](#9-privacy-first-fully-local) |
| Add semantic memory to any of the above | [10. Semantic memory add-on (Qdrant)](#10-semantic-memory-add-on-qdrant) |

---

<h2 id="1-minimal-local-ollama">1. Minimal local (Ollama)</h2>

<details>
<summary>Zero cloud dependencies. Good for first-time setup or offline use.<br>
<strong>Prerequisites:</strong> <a href="https://ollama.com">Ollama</a> installed and running (<code>ollama serve</code>), models pulled (<code>ollama pull qwen3:8b &amp;&amp; ollama pull qwen3-embedding</code>).</summary>

```toml
[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"  # for semantic skill matching

[vault]
backend = "env"  # no secrets needed for local Ollama

[memory]
history_limit = 20  # keep context lean for smaller models
```

> **Note:** `qwen3-embedding` is needed for skill matching. Without it, Zeph falls back to
> keyword-based skill selection.

See [LLM Providers](../concepts/providers.md) for other Ollama-compatible models.

</details>

---

<h2 id="2-full-cloud--claude">2. Full cloud — Claude</h2>

<details>
<summary>Best response quality. Uses Anthropic's API for chat and context compaction.<br>
<strong>Prerequisites:</strong> <code>ZEPH_CLAUDE_API_KEY</code> environment variable set.</summary>

```toml
[llm]
provider = "claude"
# Claude does not provide embeddings; skill matching uses keyword fallback.
# For semantic memory, combine with an Ollama embedding model (see recipe #5).

[llm.cloud]
model = "claude-sonnet-4-5-20250929"
max_tokens = 8192
# server_compaction = true  # let Claude API manage context instead of client-side compaction

[vault]
backend = "env"  # reads ZEPH_CLAUDE_API_KEY from environment

[memory]
history_limit = 50
```

> **Tip:** Claude does not support embeddings natively. For semantic memory and skill matching,
> combine with Ollama embeddings using [recipe #5](#5-hybrid-ollama--claude-fallback).

See [Use a Cloud Provider](cloud-provider.md) and [Model Orchestrator](../advanced/orchestrator.md).

</details>

---

<h2 id="3-full-cloud--openai">3. Full cloud — OpenAI</h2>

<details>
<summary>Uses OpenAI for both chat and embeddings — no Ollama required.<br>
<strong>Prerequisites:</strong> <code>ZEPH_OPENAI_API_KEY</code> environment variable set.</summary>

```toml
[llm]
provider = "openai"
embedding_model = "text-embedding-3-small"  # used for skill matching and semantic memory

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
max_tokens = 4096
embedding_model = "text-embedding-3-small"

[vault]
backend = "env"  # reads ZEPH_OPENAI_API_KEY from environment

[memory]
history_limit = 50
```

> **Tip:** With `embedding_model` set, Zeph uses OpenAI embeddings for both skill matching and
> semantic memory — no separate embedding service needed.

</details>

---

<h2 id="4-compatible-provider">4. Compatible provider</h2>

<details>
<summary>Any OpenAI-compatible API: Groq, Together, Mistral, Fireworks, local vLLM, etc.<br>
<strong>Prerequisites:</strong> Provider API key — set <code>ZEPH_COMPATIBLE_&lt;NAME&gt;_API_KEY</code> in your environment.</summary>

```toml
[llm]
provider = "groq"  # must match the `name` field in [[llm.compatible]] below

[[llm.compatible]]
name = "groq"
base_url = "https://api.groq.com/openai/v1"
model = "llama-3.3-70b-versatile"
max_tokens = 4096
# API key: set ZEPH_COMPATIBLE_GROQ_API_KEY in your environment

[vault]
backend = "env"
```

To switch providers, change `name`, `base_url`, and `model`. Common base URLs:

| Provider | `base_url` |
|---|---|
| Together AI | `https://api.together.xyz/v1` |
| Groq | `https://api.groq.com/openai/v1` |
| Fireworks | `https://api.fireworks.ai/inference/v1` |
| Local vLLM | `http://localhost:8000/v1` |

> **Note:** The env var name is `ZEPH_COMPATIBLE_<NAME>_API_KEY` where `<NAME>` is the `name`
> field uppercased. For the example above: `ZEPH_COMPATIBLE_GROQ_API_KEY`.

</details>

---

<h2 id="5-hybrid-ollama--claude-fallback">5. Hybrid: Ollama + Claude fallback</h2>

<details>
<summary>Ollama runs locally for free; Claude handles requests when Ollama fails or is unavailable.<br>
<strong>Prerequisites:</strong> Ollama running locally + <code>ZEPH_CLAUDE_API_KEY</code> set.</summary>

```toml
[llm]
provider = "router"
base_url = "http://localhost:11434"     # used by the ollama sub-provider
model = "qwen3:8b"                      # ollama model
embedding_model = "qwen3-embedding"     # local embeddings — always available offline

[llm.cloud]
model = "claude-haiku-4-5-20251001"    # fast + cheap fallback
max_tokens = 4096

[llm.router]
# Try ollama first; on connection error or timeout, fall back to claude.
chain = ["ollama", "claude"]

[vault]
backend = "env"
```

> **Tip:** This setup keeps embeddings local (free, private) while giving you a cloud fallback
> for chat when the local model is unavailable or overloaded.

See [Adaptive Inference](../advanced/adaptive-inference.md) for Thompson Sampling and latency-based routing.

</details>

---

<h2 id="6-orchestrator-for-complex-tasks">6. Orchestrator for complex tasks</h2>

<details>
<summary>Routes planning and execution to different local models. Enables <code>/plan</code> commands.<br>
<strong>Prerequisites:</strong> Ollama running with at least two models pulled (<code>qwen3:8b</code> and <code>qwen3:14b</code>).</summary>

```toml
[llm]
provider = "orchestrator"
base_url = "http://localhost:11434"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"

[llm.orchestrator]
default = "ollama/qwen3:8b"    # default provider for unclassified tasks
embed = "qwen3-embedding"       # embedding model for semantic memory

[llm.orchestrator.providers.planner]
type = "ollama"
model = "qwen3:14b"            # larger model for planning and goal decomposition

[llm.orchestrator.providers.executor]
type = "ollama"
model = "qwen3:8b"             # smaller model for tool execution steps

[llm.orchestrator.routes]
coding = ["planner", "executor"]    # plan with large model, execute with small
general = ["executor"]              # general chat: use the smaller model directly

[orchestration]
enabled = true            # enable /plan commands and task graph execution
max_tasks = 20
max_parallel = 2          # conservative for local inference
confirm_before_execute = true

[vault]
backend = "env"
```

> **Note:** `[orchestration]` (lowercase) enables `/plan` CLI commands. `[llm.orchestrator]`
> routes LLM calls between models. The two sections are independent.

See [Task Orchestration](../concepts/task-orchestration.md) and [Model Orchestrator](../advanced/orchestrator.md).

</details>

---

<h2 id="7-coding-assistant">7. Coding assistant</h2>

<details>
<summary>LSP code intelligence and AST-based code indexing on top of local inference.<br>
<strong>Prerequisites:</strong> Ollama running + a language server installed + <code>mcpls</code> (<code>cargo install mcpls</code>).</summary>

```toml
[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"

[vault]
backend = "env"

# AST-based code indexing: builds a semantic map of the repository.
# Uses SQLite vector backend by default; add recipe #10 for Qdrant.
[index]
enabled = true
watch = true          # reindex incrementally on file changes
max_chunks = 12
repo_map_tokens = 500 # include a structural map in the system prompt

[tools.shell]
allow_network = false  # restrict shell tools to local-only for coding sessions
confirm_patterns = ["rm ", "git push"]

# LSP code intelligence via mcpls MCP server.
# mcpls auto-detects language servers from project files.
[[mcp.servers]]
id = "mcpls"
command = "mcpls"
args = ["--workspace-root", "."]
timeout = 60  # LSP servers need warmup time
```

> **Tip:** `mcpls` auto-detects language servers: `Cargo.toml` → rust-analyzer,
> `package.json` → typescript-language-server, `pyproject.toml` → pyright, etc.

See [LSP Code Intelligence](../guides/lsp.md) and [Code Indexing](../advanced/code-indexing.md).

</details>

---

<h2 id="8-telegram-bot">8. Telegram bot</h2>

<details>
<summary>Persistent Telegram bot. Suitable for a server or always-on machine.<br>
<strong>Prerequisites:</strong> Telegram bot token (from <a href="https://t.me/BotFather">@BotFather</a>) + <code>ZEPH_CLAUDE_API_KEY</code> set.</summary>

```toml
[llm]
provider = "claude"

[llm.cloud]
model = "claude-sonnet-4-5-20250929"
max_tokens = 4096

[vault]
backend = "env"  # reads ZEPH_CLAUDE_API_KEY and ZEPH_TELEGRAM_BOT_TOKEN

[telegram]
# token = "your-bot-token"  # or set ZEPH_TELEGRAM_BOT_TOKEN env var
allowed_users = ["yourusername"]  # restrict access — do not leave empty on a public server

[memory]
history_limit = 50  # longer history for async messaging patterns

[security]
autonomy_level = "supervised"  # always ask before destructive operations

[daemon]
enabled = true         # keep the process alive and restart on crash
pid_file = "~/.zeph/zeph.pid"
```

> **Warning:** Always set `allowed_users`. An open bot with tool execution enabled is a security
> risk. See [Security](../reference/security.md).

Run in background: `zeph --config config.toml &` or use a systemd service.
See [Run via Telegram](telegram.md) and [Daemon Mode](daemon-mode.md).

</details>

---

<h2 id="9-privacy-first-fully-local">9. Privacy-first (fully local)</h2>

<details>
<summary>No outbound connections. No API keys. No telemetry. Shell restricted to local commands.<br>
<strong>Prerequisites:</strong> Ollama running locally with desired models pulled.</summary>

```toml
[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"

[vault]
backend = "env"  # no secrets needed

[memory]
history_limit = 30
vector_backend = "sqlite"  # embedded vector index — no Qdrant required

[memory.semantic]
enabled = true

[tools.shell]
allow_network = false
blocked_commands = ["curl", "wget", "nc", "ssh", "scp", "rsync"]
confirm_patterns = ["rm ", "git push", "sudo "]

[security]
autonomy_level = "supervised"
redact_secrets = true

[security.content_isolation]
enabled = true

[a2a]
enabled = false  # no agent-to-agent network server

[gateway]
enabled = false  # no HTTP gateway

[observability]
exporter = ""  # no telemetry
```

> **Note:** `vector_backend = "sqlite"` uses an embedded vector index — no Qdrant required.
> Good for personal workloads (up to ~100K embeddings).

</details>

---

<h2 id="10-semantic-memory-add-on-qdrant">10. Semantic memory add-on (Qdrant)</h2>

<details>
<summary>Layer persistent vector memory onto <strong>any</strong> recipe above.<br>
<strong>Prerequisites:</strong> Qdrant running locally — <code>docker run -d -p 6334:6334 qdrant/qdrant</code>.</summary>

Add these sections to your base config:

```toml
[memory]
qdrant_url = "http://localhost:6334"
vector_backend = "qdrant"   # switch from embedded SQLite to external Qdrant

[memory.semantic]
enabled = true
recall_limit = 5             # messages recalled per query
vector_weight = 0.7          # blend of vector similarity vs keyword (FTS5)
keyword_weight = 0.3
temporal_decay_enabled = true
temporal_decay_half_life_days = 30  # older memories fade gradually
mmr_enabled = true           # diversify results (avoid near-duplicate recalls)
mmr_lambda = 0.7
```

> **Note:** When the primary provider does not support embeddings (e.g. Claude), Zeph needs a
> separate embedding source. Add Ollama as a secondary provider (recipe #5) or use OpenAI
> embeddings (recipe #3).

See [Set Up Semantic Memory](semantic-memory.md) for collection management and tuning.

</details>

---

## Combining recipes

Recipes 1–9 are standalone base configs. Recipe 10 (semantic memory) can be layered on top of
any of them by merging the `[memory]` sections.

Common combinations:

- **Local with memory**: recipe 1 + recipe 10 (use `vector_backend = "sqlite"` for zero dependencies)
- **Cloud + memory**: recipe 2 or 3 + recipe 10 (OpenAI handles embeddings natively)
- **Privacy + memory**: recipe 9 already includes `vector_backend = "sqlite"` — semantic memory is on
- **Coding + orchestrator**: recipe 7 + recipe 6 sections for multi-model routing

For the full configuration reference with all available options, see [Configuration](../reference/configuration.md).
