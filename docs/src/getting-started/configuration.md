# Configuration

## Configuration Wizard

Run `zeph init` to generate a `config.toml` interactively. The wizard walks through five steps:

1. **LLM Provider** -- select Ollama (local), Claude, OpenAI, Orchestrator (multi-model routing), or a compatible endpoint. Orchestrator prompts for a primary and fallback provider, enabling automatic failover. Provide the base URL, model name, and API key as needed. Choose an embedding model (default: `qwen3-embedding`).
2. **Memory** -- set the SQLite database path and optionally enable semantic memory with Qdrant.
3. **Channel** -- pick CLI (default), Telegram, Discord, or Slack. Provide tokens and credentials for the selected channel.
4. **Secrets backend** -- choose `env` (environment variables) or `age` (encrypted file via `~/.zeph/vault.age`).
5. **Review and write** -- inspect the generated TOML, confirm the output path, and save.

Specify the output path directly:

```bash
zeph init --output ~/.zeph/config.toml
```

If the target file already exists, the wizard asks before overwriting.

After writing, the wizard prints the environment variables you need to set (API keys, tokens) depending on the chosen secrets backend.

## Config File Resolution

Zeph loads `config/default.toml` at startup and applies environment variable overrides.

The config path can be overridden via CLI argument or environment variable:

```bash
# CLI argument (highest priority)
zeph --config /path/to/custom.toml

# Environment variable
ZEPH_CONFIG=/path/to/custom.toml zeph

# Default (fallback)
# config/default.toml
```

Priority: `--config` > `ZEPH_CONFIG` > `config/default.toml`.

## Validation

`Config::validate()` runs at startup and rejects out-of-range values:

| Field | Constraint |
|-------|-----------|
| `memory.history_limit` | <= 10,000 |
| `memory.context_budget_tokens` | <= 1,000,000 (when > 0) |
| `agent.max_tool_iterations` | <= 100 |
| `a2a.rate_limit` | > 0 |
| `gateway.rate_limit` | > 0 |

## Hot-Reload

Zeph watches the config file for changes and applies runtime-safe fields without restart. The file watcher uses 500ms debounce to avoid redundant reloads.

**Reloadable fields** (applied immediately):

| Section | Fields |
|---------|--------|
| `[security]` | `redact_secrets` |
| `[timeouts]` | `llm_seconds`, `embedding_seconds`, `a2a_seconds` |
| `[memory]` | `history_limit`, `summarization_threshold`, `context_budget_tokens`, `compaction_threshold`, `compaction_preserve_tail`, `prune_protect_tokens`, `cross_session_score_threshold` |
| `[memory.semantic]` | `recall_limit` |
| `[index]` | `repo_map_ttl_secs`, `watch` |
| `[agent]` | `max_tool_iterations` |
| `[skills]` | `max_active_skills` |

**Not reloadable** (require restart): LLM provider/model, SQLite path, Qdrant URL, Telegram token, MCP servers, A2A config, skill paths.

Check for `config reloaded` in the log to confirm a successful reload.

## Configuration File

```toml
[agent]
name = "Zeph"
max_tool_iterations = 10  # Max tool loop iterations per response (default: 10)

[llm]
provider = "ollama"  # ollama, claude, openai, candle, compatible, orchestrator, router
base_url = "http://localhost:11434"
model = "mistral:7b"
embedding_model = "qwen3-embedding"  # Model for text embeddings
# vision_model = "llava:13b"        # Ollama only: dedicated model for image requests

[llm.cloud]
model = "claude-sonnet-4-5-20250929"
max_tokens = 4096

# [llm.openai]
# base_url = "https://api.openai.com/v1"
# model = "gpt-5.2"
# max_tokens = 4096
# embedding_model = "text-embedding-3-small"
# reasoning_effort = "medium"  # low, medium, high (for reasoning models)

[llm.stt]
provider = "whisper"
model = "whisper-1"
# Requires `stt` feature. Uses the OpenAI API key from [llm.openai] or ZEPH_OPENAI_API_KEY.

[skills]
paths = ["./skills"]
max_active_skills = 5  # Top-K skills per query via embedding similarity
disambiguation_threshold = 0.05  # LLM disambiguation when top-2 score delta < threshold (0.0 = disabled)

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
summarization_threshold = 100  # Trigger summarization after N messages
context_budget_tokens = 0      # 0 = unlimited (proportional split: 15% summaries, 25% recall, 60% recent)
compaction_threshold = 0.75    # Compact when context usage exceeds this fraction
compaction_preserve_tail = 4   # Keep last N messages during compaction
prune_protect_tokens = 40000   # Protect recent N tokens from tool output pruning
cross_session_score_threshold = 0.35  # Minimum relevance for cross-session results

[memory.semantic]
enabled = false               # Enable semantic search via Qdrant
recall_limit = 5              # Number of semantically relevant messages to inject

[tools]
enabled = true
summarize_output = false      # LLM-based summarization for long tool outputs

[tools.shell]
timeout = 30
blocked_commands = []
allowed_commands = []
allowed_paths = []          # Directories shell can access (empty = cwd only)
allow_network = true        # false blocks curl/wget/nc
confirm_patterns = ["rm ", "git push -f", "git push --force", "drop table", "drop database", "truncate "]

[tools.file]
allowed_paths = []          # Directories file tools can access (empty = cwd only)

# Pattern-based permissions per tool (optional; overrides legacy blocked_commands/confirm_patterns)
# [tools.permissions.bash]
# [[tools.permissions.bash]]
# pattern = "*sudo*"
# action = "deny"
# [[tools.permissions.bash]]
# pattern = "cargo *"
# action = "allow"
# [[tools.permissions.bash]]
# pattern = "*"
# action = "ask"

[tools.scrape]
timeout = 15
max_body_bytes = 1048576  # 1MB

[tools.audit]
enabled = false             # Structured JSON audit log for tool executions
destination = "stdout"      # "stdout" or file path

[security]
redact_secrets = true       # Redact API keys/tokens in LLM responses

[timeouts]
llm_seconds = 120           # LLM chat completion timeout
embedding_seconds = 30      # Embedding generation timeout
a2a_seconds = 30            # A2A remote call timeout

[vault]
backend = "env"  # "env" (default) or "age"; CLI --vault overrides this

[observability]
exporter = "none"           # "none" or "otlp" (requires `otel` feature)
endpoint = "http://localhost:4317"

[cost]
enabled = false
max_daily_cents = 500       # Daily budget in cents (USD), UTC midnight reset

[a2a]
enabled = false
host = "0.0.0.0"
port = 8080
# public_url = "https://agent.example.com"
# auth_token = "secret"
rate_limit = 60
```

> Shell commands are sandboxed with path restrictions, network control, and destructive command confirmation. See [Security](../security.md) for details.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `ZEPH_LLM_PROVIDER` | `ollama`, `claude`, `openai`, `candle`, `compatible`, `orchestrator`, or `router` |
| `ZEPH_LLM_BASE_URL` | Ollama API endpoint |
| `ZEPH_LLM_MODEL` | Model name for Ollama |
| `ZEPH_LLM_EMBEDDING_MODEL` | Embedding model for Ollama (default: `qwen3-embedding`) |
| `ZEPH_LLM_VISION_MODEL` | Vision model for Ollama image requests (optional) |
| `ZEPH_CLAUDE_API_KEY` | Anthropic API key (required for Claude) |
| `ZEPH_OPENAI_API_KEY` | OpenAI API key (required for OpenAI provider) |
| `ZEPH_TELEGRAM_TOKEN` | Telegram bot token (enables Telegram mode) |
| `ZEPH_SQLITE_PATH` | SQLite database path |
| `ZEPH_QDRANT_URL` | Qdrant server URL (default: `http://localhost:6334`) |
| `ZEPH_MEMORY_SUMMARIZATION_THRESHOLD` | Trigger summarization after N messages (default: 100) |
| `ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS` | Context budget for proportional token allocation (default: 0 = unlimited) |
| `ZEPH_MEMORY_COMPACTION_THRESHOLD` | Compaction trigger threshold as fraction of context budget (default: 0.75) |
| `ZEPH_MEMORY_COMPACTION_PRESERVE_TAIL` | Messages preserved during compaction (default: 4) |
| `ZEPH_MEMORY_PRUNE_PROTECT_TOKENS` | Tokens protected from Tier 1 tool output pruning (default: 40000) |
| `ZEPH_MEMORY_CROSS_SESSION_SCORE_THRESHOLD` | Minimum relevance score for cross-session memory (default: 0.35) |
| `ZEPH_MEMORY_SEMANTIC_ENABLED` | Enable semantic memory with Qdrant (default: false) |
| `ZEPH_MEMORY_RECALL_LIMIT` | Max semantically relevant messages to recall (default: 5) |
| `ZEPH_SKILLS_MAX_ACTIVE` | Max skills per query via embedding match (default: 5) |
| `ZEPH_AGENT_MAX_TOOL_ITERATIONS` | Max tool loop iterations per response (default: 10) |
| `ZEPH_TOOLS_SUMMARIZE_OUTPUT` | Enable LLM-based tool output summarization (default: false) |
| `ZEPH_TOOLS_TIMEOUT` | Shell command timeout in seconds (default: 30) |
| `ZEPH_TOOLS_SCRAPE_TIMEOUT` | Web scrape request timeout in seconds (default: 15) |
| `ZEPH_TOOLS_SCRAPE_MAX_BODY` | Max response body size in bytes (default: 1048576) |
| `ZEPH_A2A_ENABLED` | Enable A2A server (default: false) |
| `ZEPH_A2A_HOST` | A2A server bind address (default: `0.0.0.0`) |
| `ZEPH_A2A_PORT` | A2A server port (default: `8080`) |
| `ZEPH_A2A_PUBLIC_URL` | Public URL for agent card discovery |
| `ZEPH_A2A_AUTH_TOKEN` | Bearer token for A2A server authentication |
| `ZEPH_A2A_RATE_LIMIT` | Max requests per IP per minute (default: 60) |
| `ZEPH_A2A_REQUIRE_TLS` | Require HTTPS for outbound A2A connections (default: true) |
| `ZEPH_A2A_SSRF_PROTECTION` | Block private/loopback IPs in A2A client (default: true) |
| `ZEPH_A2A_MAX_BODY_SIZE` | Max request body size in bytes (default: 1048576) |
| `ZEPH_TOOLS_FILE_ALLOWED_PATHS` | Comma-separated directories file tools can access (empty = cwd) |
| `ZEPH_TOOLS_SHELL_ALLOWED_PATHS` | Comma-separated directories shell can access (empty = cwd) |
| `ZEPH_TOOLS_SHELL_ALLOW_NETWORK` | Allow network commands from shell (default: true) |
| `ZEPH_TOOLS_AUDIT_ENABLED` | Enable audit logging for tool executions (default: false) |
| `ZEPH_TOOLS_AUDIT_DESTINATION` | Audit log destination: `stdout` or file path |
| `ZEPH_SECURITY_REDACT_SECRETS` | Redact secrets in LLM responses (default: true) |
| `ZEPH_TIMEOUT_LLM` | LLM call timeout in seconds (default: 120) |
| `ZEPH_TIMEOUT_EMBEDDING` | Embedding generation timeout in seconds (default: 30) |
| `ZEPH_TIMEOUT_A2A` | A2A remote call timeout in seconds (default: 30) |
| `ZEPH_OBSERVABILITY_EXPORTER` | Tracing exporter: `none` or `otlp` (default: `none`, requires `otel` feature) |
| `ZEPH_OBSERVABILITY_ENDPOINT` | OTLP gRPC endpoint (default: `http://localhost:4317`) |
| `ZEPH_COST_ENABLED` | Enable cost tracking (default: false) |
| `ZEPH_COST_MAX_DAILY_CENTS` | Daily spending limit in cents (default: 500) |
| `ZEPH_STT_PROVIDER` | STT provider: `whisper` or `candle-whisper` (default: `whisper`, requires `stt` feature) |
| `ZEPH_STT_MODEL` | STT model name (default: `whisper-1`) |
| `ZEPH_CONFIG` | Path to config file (default: `config/default.toml`) |
| `ZEPH_TUI` | Enable TUI dashboard: `true` or `1` (requires `tui` feature) |
