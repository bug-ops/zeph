# Configuration

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

## Configuration File

```toml
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"
embedding_model = "qwen3-embedding"  # Model for text embeddings

[llm.cloud]
model = "claude-sonnet-4-5-20250929"
max_tokens = 4096

# [llm.openai]
# base_url = "https://api.openai.com/v1"
# model = "gpt-5.2"
# max_tokens = 4096
# embedding_model = "text-embedding-3-small"
# reasoning_effort = "medium"  # low, medium, high (for reasoning models)

[skills]
paths = ["./skills"]
max_active_skills = 5  # Top-K skills per query via embedding similarity

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
summarization_threshold = 100  # Trigger summarization after N messages
context_budget_tokens = 0      # 0 = unlimited (proportional split: 15% summaries, 25% recall, 60% recent)

[memory.semantic]
enabled = false               # Enable semantic search via Qdrant
recall_limit = 5              # Number of semantically relevant messages to inject

[tools]
enabled = true

[tools.shell]
timeout = 30
blocked_commands = []
allowed_commands = []
allowed_paths = []          # Directories shell can access (empty = cwd only)
allow_network = true        # false blocks curl/wget/nc
confirm_patterns = ["rm ", "git push -f", "git push --force", "drop table", "drop database", "truncate "]

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
backend = "env"  # "env" (default) or "age"

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
| `ZEPH_LLM_PROVIDER` | `ollama`, `claude`, `openai`, `candle`, or `orchestrator` |
| `ZEPH_LLM_BASE_URL` | Ollama API endpoint |
| `ZEPH_LLM_MODEL` | Model name for Ollama |
| `ZEPH_LLM_EMBEDDING_MODEL` | Embedding model for Ollama (default: `qwen3-embedding`) |
| `ZEPH_CLAUDE_API_KEY` | Anthropic API key (required for Claude) |
| `ZEPH_OPENAI_API_KEY` | OpenAI API key (required for OpenAI provider) |
| `ZEPH_TELEGRAM_TOKEN` | Telegram bot token (enables Telegram mode) |
| `ZEPH_SQLITE_PATH` | SQLite database path |
| `ZEPH_QDRANT_URL` | Qdrant server URL (default: `http://localhost:6334`) |
| `ZEPH_MEMORY_SUMMARIZATION_THRESHOLD` | Trigger summarization after N messages (default: 100) |
| `ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS` | Context budget for proportional token allocation (default: 0 = unlimited) |
| `ZEPH_SKILLS_MAX_ACTIVE` | Max skills per query via embedding match (default: 5) |
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
| `ZEPH_TOOLS_SHELL_ALLOWED_PATHS` | Comma-separated directories shell can access (empty = cwd) |
| `ZEPH_TOOLS_SHELL_ALLOW_NETWORK` | Allow network commands from shell (default: true) |
| `ZEPH_TOOLS_AUDIT_ENABLED` | Enable audit logging for tool executions (default: false) |
| `ZEPH_TOOLS_AUDIT_DESTINATION` | Audit log destination: `stdout` or file path |
| `ZEPH_SECURITY_REDACT_SECRETS` | Redact secrets in LLM responses (default: true) |
| `ZEPH_TIMEOUT_LLM` | LLM call timeout in seconds (default: 120) |
| `ZEPH_TIMEOUT_EMBEDDING` | Embedding generation timeout in seconds (default: 30) |
| `ZEPH_TIMEOUT_A2A` | A2A remote call timeout in seconds (default: 30) |
| `ZEPH_CONFIG` | Path to config file (default: `config/default.toml`) |
| `ZEPH_TUI` | Enable TUI dashboard: `true` or `1` (requires `tui` feature) |
