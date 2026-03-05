# Configuration Reference

Complete reference for the Zeph configuration file and environment variables. For the interactive 7-step setup wizard (including daemon/A2A configuration), see [Configuration Wizard](../getting-started/wizard.md).

## Config File Resolution

Zeph loads `config/default.toml` at startup and applies environment variable overrides.

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
| `memory.compression.threshold_tokens` | >= 1,000 (proactive only) |
| `memory.compression.max_summary_tokens` | >= 128 (proactive only) |
| `memory.token_safety_margin` | > 0.0 |
| `agent.max_tool_iterations` | <= 100 |
| `a2a.rate_limit` | > 0 |
| `acp.max_sessions` | > 0 |
| `acp.session_idle_timeout_secs` | > 0 |
| `acp.permission_file` | valid file path (optional) |
| `gateway.rate_limit` | > 0 |
| `gateway.max_body_size` | <= 10,485,760 (10 MiB) |

## Hot-Reload

Zeph watches the config file for changes and applies runtime-safe fields without restart (500ms debounce).

**Reloadable fields:**

| Section | Fields |
|---------|--------|
| `[security]` | `redact_secrets` |
| `[timeouts]` | `llm_seconds`, `embedding_seconds`, `a2a_seconds` |
| `[memory]` | `history_limit`, `summarization_threshold`, `context_budget_tokens`, `compaction_threshold`, `compaction_preserve_tail`, `prune_protect_tokens`, `cross_session_score_threshold` |
| `[memory.semantic]` | `recall_limit` |
| `[index]` | `repo_map_ttl_secs`, `watch` |
| `[agent]` | `max_tool_iterations` |
| `[skills]` | `max_active_skills` |

**Not reloadable** (require restart): LLM provider/model, SQLite path, Qdrant URL, vector backend, Telegram token, MCP servers, A2A config, ACP config, agents config, skill paths.

## Configuration File

```toml
[agent]
name = "Zeph"
max_tool_iterations = 10  # Max tool loop iterations per response (default: 10)
auto_update_check = true  # Query GitHub Releases API for newer versions (default: true)

[agent.instructions]
auto_detect    = true    # Auto-detect provider-specific files: CLAUDE.md, AGENTS.md, GEMINI.md (default: true)
extra_files    = []      # Additional instruction files (absolute or relative to cwd)
max_size_bytes = 262144  # Per-file size cap in bytes (default: 256 KiB)
# zeph.md and .zeph/zeph.md are always loaded regardless of auto_detect.
# Use --instruction-file <path> at the CLI to supply extra files at startup.

[agent.learning]
correction_detection = true           # Enable implicit correction detection (default: true)
correction_confidence_threshold = 0.7 # Jaccard token overlap threshold for correction candidates (default: 0.7)
correction_recall_limit = 3           # Max corrections injected into system prompt (default: 3)
correction_min_similarity = 0.75      # Min cosine similarity for correction recall from Qdrant (default: 0.75)

[llm]
provider = "ollama"  # ollama, claude, openai, candle, compatible, orchestrator, router
base_url = "http://localhost:11434"
model = "qwen3:8b"
embedding_model = "qwen3-embedding"  # Model for text embeddings
# vision_model = "llava:13b"        # Ollama only: dedicated model for image requests
# response_cache_enabled = false     # SQLite-backed LLM response cache (default: false)
# response_cache_ttl_secs = 3600     # Cache TTL in seconds (default: 3600)

[llm.cloud]
model = "claude-sonnet-4-5-20250929"
max_tokens = 4096

# [llm.openai]
# base_url = "https://api.openai.com/v1"
# model = "gpt-5.2"
# max_tokens = 4096
# embedding_model = "text-embedding-3-small"
# reasoning_effort = "medium"  # low, medium, high (for reasoning models)

# [llm.ollama]
# tool_use = false  # Enable Ollama native tool calling (default: false)
#                   # Requires a model with function-calling support (e.g. qwen3:8b, llama3.1)

router_ema_enabled = false         # EMA-based provider latency routing (default: false)
router_ema_alpha = 0.1             # EMA smoothing factor, 0.0–1.0 (default: 0.1)
router_reorder_interval = 10       # Re-order providers every N requests (default: 10)

# [llm.router]
# chain = ["claude", "openai", "ollama"]  # Ordered fallback chain (required when provider = "router")
# strategy = "ema"                        # "ema" (default) or "thompson"
# thompson_state_path = "~/.zeph/router_thompson_state.json"  # Thompson state persistence path

[llm.stt]
provider = "whisper"
model = "whisper-1"
# base_url = "http://127.0.0.1:8080/v1"  # optional: OpenAI-compatible server
# language = "en"                          # optional: ISO-639-1 code or "auto"
# Requires `stt` feature. When base_url is set, targets a local server (no API key needed).
# When omitted, uses the OpenAI API key from [llm.openai] or ZEPH_OPENAI_API_KEY.

[skills]
paths = ["./skills"]
max_active_skills = 5              # Top-K skills per query via embedding similarity
disambiguation_threshold = 0.05    # LLM disambiguation when top-2 score delta < threshold (0.0 = disabled)
prompt_mode = "auto"               # Skill prompt format: "full", "compact", or "auto" (default: "auto")
cosine_weight = 0.7                # Cosine signal weight in BM25+cosine fusion (default: 0.7)
hybrid_search = false              # Enable BM25+cosine hybrid skill matching (default: false)

[skills.learning]
enabled = true                     # Enable self-learning skill improvement (default: true)
auto_activate = false              # Require manual approval for new versions (default: false)
min_failures = 3                   # Failures before triggering improvement (default: 3)
improve_threshold = 0.7            # Success rate below which improvement starts (default: 0.7)
rollback_threshold = 0.5           # Auto-rollback when success rate drops below this (default: 0.5)
min_evaluations = 5                # Minimum evaluations before rollback decision (default: 5)
max_versions = 10                  # Max auto-generated versions per skill (default: 10)
cooldown_minutes = 60              # Cooldown between improvements for same skill (default: 60)
detector_mode = "regex"            # Correction detector: "regex" (default) or "judge" (LLM-backed)
judge_model = ""                   # Model for judge calls; empty = use primary provider
judge_adaptive_low = 0.5           # Regex confidence below this bypasses judge (default: 0.5)
judge_adaptive_high = 0.8          # Regex confidence at/above this bypasses judge (default: 0.8)

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
summarization_threshold = 100  # Trigger summarization after N messages
context_budget_tokens = 0      # 0 = unlimited (proportional split: 15% summaries, 25% recall, 60% recent)
compaction_threshold = 0.75    # Compact when context usage exceeds this fraction
compaction_preserve_tail = 4   # Keep last N messages during compaction
prune_protect_tokens = 40000   # Protect recent N tokens from tool output pruning
cross_session_score_threshold = 0.35  # Minimum relevance for cross-session results
vector_backend = "qdrant"     # Vector store: "qdrant" (default) or "sqlite" (embedded)
sqlite_pool_size = 5          # SQLite connection pool size (default: 5)
response_cache_cleanup_interval_secs = 3600  # Interval for purging expired LLM response cache entries (default: 3600)
token_safety_margin = 1.0     # Multiplier for token budget safety margin (default: 1.0)
redact_credentials = true     # Scrub credential patterns from LLM context (default: true)
autosave_assistant = false    # Persist assistant responses to SQLite and embed (default: false)
autosave_min_length = 20      # Min content length for assistant embedding (default: 20)
tool_call_cutoff = 6          # Summarize oldest tool pair when visible pairs exceed this (default: 6)

[memory.semantic]
enabled = false               # Enable semantic search via Qdrant
recall_limit = 5              # Number of semantically relevant messages to inject
temporal_decay_enabled = false        # Attenuate scores by message age (default: false)
temporal_decay_half_life_days = 30    # Half-life for temporal decay in days (default: 30)
mmr_enabled = false                   # MMR re-ranking for result diversity (default: false)
mmr_lambda = 0.7                      # MMR relevance-diversity trade-off, 0.0-1.0 (default: 0.7)

[memory.routing]
strategy = "heuristic"        # Routing strategy for memory backend selection (default: "heuristic")

[memory.compression]
strategy = "reactive"         # "reactive" (default) or "proactive"
# Proactive strategy fields (required when strategy = "proactive"):
# threshold_tokens = 80000   # Fire compression when context exceeds this token count (>= 1000)
# max_summary_tokens = 4000  # Cap for the compressed summary (>= 128)
# model = ""                 # Reserved — currently unused

[memory.graph]
enabled = false                        # Enable graph memory (default: false, requires graph-memory feature)
extract_model = ""                     # LLM model for entity extraction; empty = agent's model
max_entities_per_message = 10          # Max entities extracted per message (default: 10)
max_edges_per_message = 15             # Max edges extracted per message (default: 15)
community_refresh_interval = 100       # Messages between community recalculation (default: 100)
entity_similarity_threshold = 0.85     # Cosine threshold for entity dedup (default: 0.85)
extraction_timeout_secs = 15           # Timeout for background extraction (default: 15)
use_embedding_resolution = false       # Use embedding-based entity resolution (default: false)
max_hops = 2                           # BFS traversal depth for graph recall (default: 2)
recall_limit = 10                      # Max graph facts injected into context (default: 10)

[tools]
enabled = true
summarize_output = false      # LLM-based summarization for long tool outputs

[tools.shell]
timeout = 30
blocked_commands = []
allowed_commands = []
allowed_paths = []          # Directories shell can access (empty = cwd only)
allow_network = true        # false blocks curl/wget/nc
confirm_patterns = ["rm ", "git push -f", "git push --force", "drop table", "drop database", "truncate ", "$(", "`", "<(", ">(", "<<<", "eval "]

[tools.file]
allowed_paths = []          # Directories file tools can access (empty = cwd only)

[tools.scrape]
timeout = 15
max_body_bytes = 1048576  # 1MB

[tools.filters]
enabled = true              # Enable smart output filtering for tool results

# [tools.filters.test]
# enabled = true
# max_failures = 10         # Truncate after N test failures
# truncate_stack_trace = 50 # Max stack trace lines per failure

# [tools.filters.git]
# enabled = true
# max_log_entries = 20      # Max git log entries
# max_diff_lines = 500      # Max diff lines

# [tools.filters.clippy]
# enabled = true

# [tools.filters.cargo_build]
# enabled = true

# [tools.filters.dir_listing]
# enabled = true

# [tools.filters.log_dedup]
# enabled = true

# [tools.filters.security]
# enabled = true
# extra_patterns = []       # Additional regex patterns to redact

# Per-tool permission rules (glob patterns with allow/ask/deny actions).
# Overrides legacy blocked_commands/confirm_patterns when set.
# [tools.permissions]
# shell = [
#   { pattern = "/tmp/*", action = "allow" },
#   { pattern = "/etc/*", action = "deny" },
#   { pattern = "*sudo*", action = "deny" },
#   { pattern = "cargo *", action = "allow" },
#   { pattern = "*", action = "ask" },
# ]

[tools.overflow]
threshold = 50000           # Offload output larger than N chars to file (default: 50000)
retention_days = 7          # Days to retain overflow files before cleanup (default: 7)
# dir = "/custom/path"     # Custom overflow directory (default: ~/.zeph/data/tool-output)

[tools.audit]
enabled = false             # Structured JSON audit log for tool executions
destination = "stdout"      # "stdout" or file path

[security]
redact_secrets = true       # Redact API keys/tokens in LLM responses

[security.content_isolation]
enabled = true              # Master switch for untrusted content sanitizer
max_content_size = 65536    # Max bytes per source before truncation (default: 64 KiB)
flag_injection_patterns = true  # Detect and flag injection patterns
spotlight_untrusted = true  # Wrap untrusted content in XML delimiters

[security.content_isolation.quarantine]
enabled = false             # Opt-in: route high-risk sources through quarantine LLM
sources = ["web_scrape", "a2a_message"]  # Source kinds to quarantine
model = "claude"            # Provider/model for quarantine extraction

[security.exfiltration_guard]
block_markdown_images = true  # Strip external markdown images from LLM output
validate_tool_urls = true     # Flag tool calls using URLs from injection-flagged content
guard_memory_writes = true    # Skip Qdrant embedding for injection-flagged content

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
# auth_token = "secret"     # Bearer token for A2A server auth (from vault ZEPH_A2A_AUTH_TOKEN); warn logged at startup if unset
rate_limit = 60

[acp]
max_sessions = 4                   # Max concurrent ACP sessions; LRU eviction when exceeded (default: 4)
session_idle_timeout_secs = 1800   # Idle session reaper timeout in seconds (default: 1800)
# permission_file = "~/.config/zeph/acp-permissions.toml"  # Path to persisted permission decisions (default: ~/.config/zeph/acp-permissions.toml)
# auth_bearer_token = ""           # Bearer token for ACP HTTP/WS auth (env: ZEPH_ACP_AUTH_TOKEN, CLI: --acp-auth-token); omit for open mode (local use only)
discovery_enabled = true           # Expose GET /.well-known/acp.json manifest endpoint (env: ZEPH_ACP_DISCOVERY_ENABLED, default: true)

[mcp]
allowed_commands = ["npx", "uvx", "node", "python", "python3"]
max_dynamic_servers = 10

# [[mcp.servers]]
# id = "filesystem"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
# env = {}                  # Environment variables passed to the child process
# timeout = 30

[agents]
enabled = false            # Enable sub-agent system (default: false)
max_concurrent = 1         # Max concurrent sub-agents (default: 1)
extra_dirs = []            # Additional directories to scan for agent definitions
# default_memory_scope = "project"  # Default memory scope for agents without explicit `memory` field
                                    # Valid: "user", "project", "local". Omit to disable.
# Lifecycle hooks — see Sub-Agent Orchestration > Hooks for details
# [agents.hooks]
# [[agents.hooks.start]]
# type = "command"
# command = "echo started"
# [[agents.hooks.stop]]
# type = "command"
# command = "./scripts/cleanup.sh"

[orchestration]
enabled = false                          # Enable task orchestration (default: false, requires `orchestration` feature)
max_tasks = 20                           # Max tasks per graph (default: 20)
max_parallel = 4                         # Max concurrent task executions (default: 4)
default_failure_strategy = "abort"       # abort, retry, skip, or ask (default: "abort")
default_max_retries = 3                  # Retries for the "retry" strategy (default: 3)
task_timeout_secs = 300                  # Per-task timeout in seconds, 0 = no timeout (default: 300)
# planner_model = "claude-sonnet-4-20250514"  # Model override for planning LLM calls; unset = agent's primary model
planner_max_tokens = 4096                # Max tokens for planner LLM response (default: 4096; reserved — not yet enforced)

[gateway]
enabled = false
bind = "127.0.0.1"
port = 8090
# auth_token = "secret"     # Bearer token for gateway auth (from vault ZEPH_GATEWAY_TOKEN); warn logged at startup if unset
rate_limit = 120
max_body_size = 1048576     # 1 MiB
```

### Orchestrator Sub-Providers

When `provider = "orchestrator"`, each sub-provider under `[llm.orchestrator.providers.<name>]` accepts:

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | Provider backend (`ollama`, `claude`, `openai`, `candle`, `compatible`) |
| `model` | string? | Override chat model for this provider |
| `base_url` | string? | Override API endpoint (Ollama / Compatible) |
| `embedding_model` | string? | Override embedding model for this provider |
| `filename` | string? | GGUF filename (Candle only) |
| `device` | string? | Compute device (Candle only) |

Field resolution: per-provider value → parent section (`[llm]`, `[llm.cloud]`) → global default. See [Orchestrator](../advanced/orchestrator.md) for details and examples.

> **Note:** The TOML key is `type`, not `provider_type`. The Rust struct uses `#[serde(rename = "type")]`.

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
| `ZEPH_MEMORY_VECTOR_BACKEND` | Vector backend: `qdrant` or `sqlite` (default: `qdrant`) |
| `ZEPH_MEMORY_TOKEN_SAFETY_MARGIN` | Token budget safety margin multiplier (default: 1.0) |
| `ZEPH_MEMORY_REDACT_CREDENTIALS` | Scrub credentials from LLM context (default: true) |
| `ZEPH_MEMORY_AUTOSAVE_ASSISTANT` | Persist assistant responses to SQLite (default: false) |
| `ZEPH_MEMORY_AUTOSAVE_MIN_LENGTH` | Min content length for assistant embedding (default: 20) |
| `ZEPH_MEMORY_TOOL_CALL_CUTOFF` | Max visible tool pairs before oldest is summarized (default: 6) |
| `ZEPH_LLM_RESPONSE_CACHE_ENABLED` | Enable SQLite-backed LLM response cache (default: false) |
| `ZEPH_LLM_RESPONSE_CACHE_TTL_SECS` | Response cache TTL in seconds (default: 3600) |
| `ZEPH_MEMORY_SQLITE_POOL_SIZE` | SQLite connection pool size (default: 5) |
| `ZEPH_MEMORY_RESPONSE_CACHE_CLEANUP_INTERVAL_SECS` | Interval for purging expired LLM response cache entries in seconds (default: 3600) |
| `ZEPH_MEMORY_SEMANTIC_ENABLED` | Enable semantic memory (default: false) |
| `ZEPH_MEMORY_RECALL_LIMIT` | Max semantically relevant messages to recall (default: 5) |
| `ZEPH_MEMORY_SEMANTIC_TEMPORAL_DECAY_ENABLED` | Enable temporal decay scoring (default: false) |
| `ZEPH_MEMORY_SEMANTIC_TEMPORAL_DECAY_HALF_LIFE_DAYS` | Half-life for temporal decay in days (default: 30) |
| `ZEPH_MEMORY_SEMANTIC_MMR_ENABLED` | Enable MMR re-ranking (default: false) |
| `ZEPH_MEMORY_SEMANTIC_MMR_LAMBDA` | MMR relevance-diversity trade-off (default: 0.7) |
| `ZEPH_SKILLS_MAX_ACTIVE` | Max skills per query via embedding match (default: 5) |
| `ZEPH_AGENT_MAX_TOOL_ITERATIONS` | Max tool loop iterations per response (default: 10) |
| `ZEPH_TOOLS_SUMMARIZE_OUTPUT` | Enable LLM-based tool output summarization (default: false) |
| `ZEPH_TOOLS_TIMEOUT` | Shell command timeout in seconds (default: 30) |
| `ZEPH_TOOLS_SCRAPE_TIMEOUT` | Web scrape request timeout in seconds (default: 15) |
| `ZEPH_TOOLS_SCRAPE_MAX_BODY` | Max response body size in bytes (default: 1048576) |
| `ZEPH_ACP_MAX_SESSIONS` | Max concurrent ACP sessions (default: 4) |
| `ZEPH_ACP_SESSION_IDLE_TIMEOUT_SECS` | Idle session reaper timeout in seconds (default: 1800) |
| `ZEPH_ACP_PERMISSION_FILE` | Path to persisted ACP permission decisions (default: `~/.config/zeph/acp-permissions.toml`) |
| `ZEPH_ACP_AUTH_TOKEN` | Bearer token for ACP HTTP/WS authentication; omit for open mode (local use only) |
| `ZEPH_ACP_DISCOVERY_ENABLED` | Expose `GET /.well-known/acp.json` manifest endpoint (default: `true`) |
| `ZEPH_A2A_ENABLED` | Enable A2A server (default: false) |
| `ZEPH_A2A_HOST` | A2A server bind address (default: `0.0.0.0`) |
| `ZEPH_A2A_PORT` | A2A server port (default: `8080`) |
| `ZEPH_A2A_PUBLIC_URL` | Public URL for agent card discovery |
| `ZEPH_A2A_AUTH_TOKEN` | Bearer token for A2A server authentication |
| `ZEPH_A2A_RATE_LIMIT` | Max requests per IP per minute (default: 60) |
| `ZEPH_A2A_REQUIRE_TLS` | Require HTTPS for outbound A2A connections (default: true) |
| `ZEPH_A2A_SSRF_PROTECTION` | Block private/loopback IPs in A2A client (default: true) |
| `ZEPH_A2A_MAX_BODY_SIZE` | Max request body size in bytes (default: 1048576) |
| `ZEPH_AGENTS_ENABLED` | Enable sub-agent system (default: false) |
| `ZEPH_AGENTS_MAX_CONCURRENT` | Max concurrent sub-agents (default: 1) |
| `ZEPH_GATEWAY_ENABLED` | Enable HTTP gateway (default: false) |
| `ZEPH_GATEWAY_BIND` | Gateway bind address (default: `127.0.0.1`) |
| `ZEPH_GATEWAY_PORT` | Gateway HTTP port (default: `8090`) |
| `ZEPH_GATEWAY_TOKEN` | Bearer token for gateway authentication; warn logged at startup if unset |
| `ZEPH_GATEWAY_RATE_LIMIT` | Max requests per IP per minute (default: 120) |
| `ZEPH_GATEWAY_MAX_BODY_SIZE` | Max request body size in bytes (default: 1048576) |
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
| `ZEPH_STT_BASE_URL` | STT server base URL (e.g. `http://127.0.0.1:8080/v1` for local whisper.cpp) |
| `ZEPH_STT_LANGUAGE` | STT language: ISO-639-1 code or `auto` (default: `auto`) |
| `ZEPH_CONFIG` | Path to config file (default: `config/default.toml`) |
| `ZEPH_TUI` | Enable TUI dashboard: `true` or `1` (requires `tui` feature) |
| `ZEPH_AUTO_UPDATE_CHECK` | Enable automatic update checks: `true` or `false` (default: `true`) |
