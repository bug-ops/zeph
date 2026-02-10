# Zeph

[![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
[![Security](https://img.shields.io/badge/security-hardened-brightgreen)](SECURITY.md)
[![Trivy Scan](https://img.shields.io/badge/Trivy-0%20CVEs-success)](https://github.com/bug-ops/zeph/security)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-blue)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Lightweight AI agent with hybrid inference (Ollama / Claude / HuggingFace via candle), skills-first architecture, semantic memory with Qdrant, MCP client, A2A protocol support, multi-model orchestration, self-learning skill evolution, and multi-channel I/O. **Cross-platform**: Linux, macOS, Windows (x86_64 + ARM64).

<div align="center">
  <img src="asset/zeph-logo.png" alt="Zeph" width="600">
</div>

## Installation

### From source

```bash
git clone https://github.com/bug-ops/zeph
cd zeph
cargo build --release
```

The binary is produced at `target/release/zeph`.

### Pre-built binaries

Download from [GitHub Releases](https://github.com/bug-ops/zeph/releases/latest):

| Platform | Architecture | Download |
|----------|-------------|----------|
| Linux | x86_64 | `zeph-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | aarch64 | `zeph-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | x86_64 | `zeph-x86_64-apple-darwin.tar.gz` |
| macOS | aarch64 | `zeph-aarch64-apple-darwin.tar.gz` |
| Windows | x86_64 | `zeph-x86_64-pc-windows-msvc.zip` |

### Docker

Pull the latest image from GitHub Container Registry:

```bash
docker pull ghcr.io/bug-ops/zeph:latest
```

Or use a specific version:

```bash
docker pull ghcr.io/bug-ops/zeph:v0.7.1
```

**Security:** Images are scanned with [Trivy](https://trivy.dev/) in CI/CD and use Oracle Linux 9 Slim base with **0 HIGH/CRITICAL CVEs**. Multi-platform: linux/amd64, linux/arm64.

## Usage

### CLI mode (default)

**Unix (Linux/macOS):**
```bash
./target/release/zeph
```

**Windows:**
```powershell
.\target\release\zeph.exe
```

Type messages at the `You:` prompt. Type `exit`, `quit`, or press Ctrl-D to stop.

### Telegram mode

**Unix (Linux/macOS):**
```bash
ZEPH_TELEGRAM_TOKEN="123:ABC" ./target/release/zeph
```

**Windows:**
```powershell
$env:ZEPH_TELEGRAM_TOKEN="123:ABC"; .\target\release\zeph.exe
```

**Tip:** Restrict access by setting `telegram.allowed_users` in the config file.

## Configuration

**Note:** When using Ollama, ensure both the LLM model and embedding model are pulled:
```bash
ollama pull mistral:7b
ollama pull qwen3-embedding
```
The default configuration uses `mistral:7b` for text generation and `qwen3-embedding` for vector embeddings.

<details>
<summary><b>📝 Configuration File</b> (click to expand)</summary>

Zeph loads `config/default.toml` at startup and applies environment variable overrides.

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
blocked_commands = []  # Additional patterns beyond defaults

[tools.scrape]
timeout = 15
max_body_bytes = 1048576  # 1MB

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

</details>

> [!IMPORTANT]
> Shell commands are filtered for safety. See [Security](#security) section for complete list of 12 blocked patterns and customization options.

<details>
<summary><b>🔧 Environment Variables</b> (click to expand)</summary>

| Variable | Description |
|----------|-------------|
| `ZEPH_LLM_PROVIDER` | `ollama`, `claude`, `candle`, or `orchestrator` |
| `ZEPH_LLM_BASE_URL` | Ollama API endpoint |
| `ZEPH_LLM_MODEL` | Model name for Ollama |
| `ZEPH_LLM_EMBEDDING_MODEL` | Embedding model for Ollama (default: `qwen3-embedding`) |
| `ZEPH_CLAUDE_API_KEY` | Anthropic API key (required for Claude) |
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

</details>

## Skills

Zeph uses an embedding-based skill system with progressive loading: only metadata is loaded at startup, skill bodies are deferred until activation, and resource files are resolved on demand.

Eleven bundled skills: `web-search`, `web-scrape`, `file-ops`, `system-info`, `git`, `docker`, `api-request`, `setup-guide`, `skill-audit`, `skill-creator`, `mcp-generate`. Use `/skills` in chat to list available skills with usage statistics.

<details>
<summary><b>🛠️ Skills System</b> (click to expand)</summary>

Drop `SKILL.md` files into subdirectories under `skills/` to extend agent capabilities:

```
skills/
  web-search/
    SKILL.md
    scripts/       # optional: executable scripts
    references/    # optional: reference documents
    assets/        # optional: static assets
  git/
    SKILL.md
```

`SKILL.md` format (per [agentskills.io](https://agentskills.io) spec):

```markdown
---
name: web-search
description: Search the web for information.
compatibility: requires curl
license: MIT
allowed-tools: shell, web-scrape
---
# Instructions
Use curl to fetch search results...
```

Extended frontmatter fields: `compatibility`, `license`, `metadata` (arbitrary key-value pairs), `allowed-tools`. Unknown keys are preserved in metadata for forward compatibility.

**Name validation:** Skill names must be 1-64 characters, lowercase letters/numbers/hyphens only, no leading/trailing/consecutive hyphens, and must match the directory name.

**Progressive loading:** Only metadata (~100 tokens per skill) is loaded at startup for embedding and matching. Full body (<5000 tokens) is loaded lazily on first activation and cached via `OnceLock`. Resource files in `scripts/`, `references/`, `assets/` are loaded on demand with path traversal protection.

**Embedding-based matching:** Per query, only the top-K most relevant skills (default: 5) are selected via cosine similarity of embeddings and injected into the system prompt. Configure with `skills.max_active_skills` or `ZEPH_SKILLS_MAX_ACTIVE`.

**Hot-reload:** SKILL.md file changes are detected via filesystem watcher (500ms debounce) and re-embedded without restart. Cached bodies are invalidated on reload.

**Priority:** When multiple `skills.paths` contain a skill with the same name, the first path takes precedence.

**Usage tracking:** Per-skill invocation counts and timestamps are stored in SQLite. View with the `/skills` chat command.

</details>

## Semantic Memory (Optional)

Enable semantic search to retrieve contextually relevant messages from conversation history using vector similarity.

**Note:** Requires Ollama with an embedding model (e.g., `qwen3-embedding`). Claude API does not support embeddings natively.

<details>
<summary><b>🧠 Semantic Memory with Qdrant</b> (click to expand)</summary>

Zeph supports optional integration with [Qdrant](https://qdrant.tech/) for semantic memory:

1. **Start Qdrant:**

   ```bash
   docker compose up -d qdrant
   ```

2. **Enable semantic memory in config:**

   ```toml
   [memory.semantic]
   enabled = true
   recall_limit = 5
   ```

3. **Automatic setup:** Qdrant collection (`zeph_conversations`) is created automatically on first use with correct vector dimensions (896 for `qwen3-embedding`) and Cosine distance metric. No manual initialization required.

4. **Automatic embedding:** Messages are embedded asynchronously using the configured `embedding_model` and stored in Qdrant alongside SQLite.

5. **Semantic recall:** Context builder injects semantically relevant messages from full history, not just recent messages.

6. **Graceful degradation:** If Qdrant is unavailable, Zeph falls back to SQLite-only mode (recency-based history).

</details>

## Conversation Summarization (Optional)

Automatically compress long conversation histories using LLM-based summarization to stay within context budget limits.

> [!IMPORTANT]
> Requires an LLM provider (Ollama or Claude). Set `context_budget_tokens = 0` to disable proportional allocation and use unlimited context.

<details>
<summary><b>📝 Automatic Conversation Summarization</b> (click to expand)</summary>

Zeph supports automatic conversation summarization:

- Triggered when message count exceeds `summarization_threshold` (default: 100)
- Summaries stored in SQLite with token estimates
- Context builder allocates proportional token budget:
  - 15% for summaries
  - 25% for semantic recall (if enabled)
  - 60% for recent message history

Enable via configuration:

```toml
[memory]
summarization_threshold = 100
context_budget_tokens = 8000  # Set to LLM context window size (0 = unlimited)
```

</details>

## Docker

**Note:** Docker Compose automatically pulls the latest image from GitHub Container Registry. To use a specific version, set `ZEPH_IMAGE=ghcr.io/bug-ops/zeph:v0.7.1`.

<details>
<summary><b>🐳 Docker Deployment Options</b> (click to expand)</summary>

### Quick Start (Ollama + Qdrant in containers)

```bash
# Pull Ollama models first
docker compose --profile cpu run --rm ollama ollama pull mistral:7b
docker compose --profile cpu run --rm ollama ollama pull qwen3-embedding

# Start all services
docker compose --profile cpu up
```

### Apple Silicon (Ollama on host with Metal GPU)

```bash
# Use Ollama on macOS host for Metal GPU acceleration
ollama pull mistral:7b
ollama pull qwen3-embedding
ollama serve &

# Start Zeph + Qdrant, connect to host Ollama
ZEPH_LLM_BASE_URL=http://host.docker.internal:11434 docker compose up
```

### Linux with NVIDIA GPU

```bash
# Pull models first
docker compose --profile gpu run --rm ollama ollama pull mistral:7b
docker compose --profile gpu run --rm ollama ollama pull qwen3-embedding

# Start all services with GPU
docker compose --profile gpu -f docker-compose.yml -f docker-compose.gpu.yml up
```

### Age Vault (Encrypted Secrets)

```bash
# Mount key and vault files into container
docker compose -f docker-compose.yml -f docker-compose.vault.yml up
```

Override file paths via environment variables:

```bash
ZEPH_VAULT_KEY=./my-key.txt ZEPH_VAULT_PATH=./my-secrets.age \
  docker compose -f docker-compose.yml -f docker-compose.vault.yml up
```

> [!IMPORTANT]
> The image must be built with `vault-age` feature enabled. Pre-built images include this feature by default.

### Using Specific Version

```bash
# Use a specific release version
ZEPH_IMAGE=ghcr.io/bug-ops/zeph:v0.7.1 docker compose up

# Always pull latest
docker compose pull && docker compose up
```

### Local Development

Full stack with debug tracing (builds from source via `Dockerfile.dev`, uses host Ollama via `host.docker.internal`):

```bash
# Build and start Qdrant + Zeph with debug logging
docker compose -f docker-compose.dev.yml up --build
```

Dependencies only (run zeph natively on host):

```bash
# Start Qdrant
docker compose -f docker-compose.deps.yml up

# Run zeph natively with debug tracing
RUST_LOG=zeph=debug,zeph_channels=trace cargo run
```

</details>

## Security

Zeph implements defense-in-depth security for safe AI agent operations in production environments.

### Shell Command Filtering

> [!WARNING]
> All shell commands from LLM responses pass through a security filter before execution. Commands matching blocked patterns are rejected with detailed error messages.

**12 blocked patterns by default:**

| Pattern | Risk Category | Examples |
|---------|---------------|----------|
| `rm -rf /`, `rm -rf /*` | Filesystem destruction | Prevents accidental system wipe |
| `sudo`, `su` | Privilege escalation | Blocks unauthorized root access |
| `mkfs`, `fdisk` | Filesystem operations | Prevents disk formatting |
| `dd if=`, `dd of=` | Low-level disk I/O | Blocks dangerous write operations |
| `curl \| bash`, `wget \| sh` | Arbitrary code execution | Prevents remote code injection |
| `nc`, `ncat`, `netcat` | Network backdoors | Blocks reverse shell attempts |
| `shutdown`, `reboot`, `halt` | System control | Prevents service disruption |

**Configuration:**
```toml
[tools.shell]
timeout = 30  # Command execution timeout
blocked_commands = ["custom_pattern"]  # Additional patterns (additive to defaults)
```

> [!IMPORTANT]
> Custom patterns are **additive** — you cannot weaken default security. Matching is case-insensitive (`SUDO`, `Sudo`, `sudo` all blocked).

**Safe execution model:**
- Commands parsed for blocked patterns before execution
- Timeout enforcement (default: 30s, configurable)
- Sandboxed execution with restricted environment
- Full errors logged to system, sanitized messages shown to users

### Container Security

Docker images follow security best practices:

| Security Layer | Implementation | Status |
|----------------|----------------|--------|
| **Base image** | Oracle Linux 9 Slim | Production-hardened |
| **Vulnerability scanning** | Trivy in CI/CD | **0 HIGH/CRITICAL CVEs** |
| **User privileges** | Non-root `zeph` user (UID 1000) | ✅ Enforced |
| **Attack surface** | Minimal package installation | Distroless-style |
| **Image signing** | Coming soon (issue #TBD) | 🚧 Planned |

**Continuous security:**
- Every release scanned with [Trivy](https://trivy.dev/) before publishing
- Automated Dependabot PRs for dependency updates
- `cargo-deny` checks in CI for license/vulnerability compliance

### Code Security

Rust-native memory safety guarantees:

- **Minimal `unsafe`:** One audited `unsafe` block behind `candle` feature flag (memory-mapped safetensors loading). Core crates enforce `#![deny(unsafe_code)]`
- **No panic in production:** `unwrap()` and `expect()` linted via clippy
- **Secure dependencies:** All crates audited with `cargo-deny`
- **MSRV policy:** Rust 1.88+ (Edition 2024) for latest security patches

### Secrets Management

> [!CAUTION]
> Never commit secrets to version control. Use environment variables or age-encrypted vault files.

Zeph resolves secrets (`ZEPH_CLAUDE_API_KEY`, `ZEPH_TELEGRAM_TOKEN`, `ZEPH_A2A_AUTH_TOKEN`) through a pluggable `VaultProvider` with redacted debug output via the `Secret` newtype.

**Backends:**

| Backend | Description | Activation |
|---------|-------------|------------|
| `env` (default) | Read secrets from environment variables | `--vault env` or omit |
| `age` | Decrypt age-encrypted JSON vault file at startup | `--vault age --vault-key <identity> --vault-path <vault.age>` |

**Age vault workflow:**

```bash
# Generate an age identity key
age-keygen -o key.txt

# Create a JSON secrets file and encrypt it
echo '{"ZEPH_CLAUDE_API_KEY":"sk-...","ZEPH_TELEGRAM_TOKEN":"123:ABC"}' | \
  age -r $(grep 'public key' key.txt | awk '{print $NF}') -o secrets.age

# Run with age vault
cargo build --release --features vault-age
./target/release/zeph --vault age --vault-key key.txt --vault-path secrets.age
```

> [!TIP]
> The `vault-age` feature flag is disabled by default for zero build-time cost. Enable it only when age vault support is needed.

### Reporting Security Issues

Found a vulnerability? See [SECURITY.md](SECURITY.md) for responsible disclosure process.

**Security contact:** Submit via GitHub Security Advisories (confidential)

## A2A Server (Optional)

Zeph includes an embedded [A2A protocol](https://github.com/a2aproject/A2A) server for agent-to-agent communication. When enabled, other agents can discover and interact with Zeph via the standard A2A JSON-RPC 2.0 API.

```bash
ZEPH_A2A_ENABLED=true ZEPH_A2A_AUTH_TOKEN=secret ./target/release/zeph
```

The server exposes:
- `/.well-known/agent-card.json` — agent discovery (public, no auth)
- `/a2a` — JSON-RPC endpoint (`message/send`, `tasks/get`, `tasks/cancel`)
- `/a2a/stream` — SSE streaming endpoint

> [!TIP]
> Set `ZEPH_A2A_AUTH_TOKEN` to secure the server with bearer token authentication. The agent card endpoint remains public per A2A spec.

## Local Inference (Optional)

Run HuggingFace models locally via [candle](https://github.com/huggingface/candle) without external API dependencies. Supports GGUF quantized models with Metal/CUDA acceleration.

```bash
cargo build --release --features candle,metal  # macOS with Metal GPU
```

<details>
<summary><b>Candle Configuration</b> (click to expand)</summary>

```toml
[llm]
provider = "candle"

[llm.candle]
source = "huggingface"
repo_id = "TheBloke/Mistral-7B-Instruct-v0.2-GGUF"
filename = "mistral-7b-instruct-v0.2.Q4_K_M.gguf"
template = "mistral"              # llama3, chatml, mistral, phi3, raw
embedding_repo = "sentence-transformers/all-MiniLM-L6-v2"  # optional BERT embeddings

[llm.candle.generation]
temperature = 0.7
top_p = 0.9
top_k = 40
max_tokens = 2048
repeat_penalty = 1.1
```

Supported chat templates: `llama3`, `chatml`, `mistral`, `phi3`, `raw`.

Device auto-detection: Metal on macOS, CUDA on Linux with GPU, CPU fallback.

</details>

## Model Orchestrator (Optional)

Route tasks to different LLM providers based on content classification. Each task type (coding, creative, analysis, translation, summarization, general) maps to a provider chain with automatic fallback.

```bash
cargo build --release --features candle,orchestrator
```

<details>
<summary><b>Orchestrator Configuration</b> (click to expand)</summary>

```toml
[llm]
provider = "orchestrator"

[llm.orchestrator.providers.local]
type = "candle"
repo_id = "TheBloke/Mistral-7B-Instruct-v0.2-GGUF"
filename = "mistral-7b-instruct-v0.2.Q4_K_M.gguf"
template = "mistral"

[llm.orchestrator.providers.cloud]
type = "claude"

[llm.orchestrator.routes]
coding = ["local", "cloud"]       # try local first, fallback to cloud
creative = ["cloud"]              # cloud only
analysis = ["cloud", "local"]     # prefer cloud
general = ["local"]               # local only
```

Task classification uses keyword heuristics. Fallback chains try providers in order until one succeeds.

</details>

## MCP Integration (Optional)

Connect external tool servers via [Model Context Protocol](https://modelcontextprotocol.io/) (MCP). Tools are discovered, embedded, and matched alongside skills using the same cosine similarity pipeline.

```bash
cargo build --release --features mcp
```

<details>
<summary><b>MCP Configuration</b> (click to expand)</summary>

```toml
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@anthropic/mcp-filesystem"]

[[mcp.servers]]
name = "github"
command = "npx"
args = ["-y", "@anthropic/mcp-github"]
```

MCP tools are embedded in Qdrant (`zeph_mcp_tools` collection) with BLAKE3 content-hash delta sync. Unified matching injects both skills and MCP tools into the system prompt by relevance score.

</details>

## Self-Learning Skills (Optional)

Automatically improve skills based on execution outcomes. When a skill fails repeatedly, Zeph uses self-reflection and LLM-generated improvements to create better skill versions.

```bash
cargo build --release --features self-learning
```

<details>
<summary><b>Self-Learning Configuration</b> (click to expand)</summary>

```toml
[skills.learning]
enabled = true
auto_activate = false     # require manual approval for new versions
min_failures = 3          # failures before triggering improvement
improve_threshold = 0.7   # success rate below which improvement starts
rollback_threshold = 0.5  # auto-rollback when success rate drops below this
min_evaluations = 5       # minimum evaluations before rollback decision
max_versions = 10         # max auto-generated versions per skill
cooldown_minutes = 60     # cooldown between improvements for same skill
```

**Chat commands:**
- `/skill stats` — view skill execution metrics
- `/skill versions` — list generated skill versions
- `/skill activate <id>` — activate a specific version
- `/skill approve <id>` — approve a pending version
- `/skill reset <name>` — reset skill to original version
- `/feedback` — provide explicit feedback on skill quality

</details>

> [!TIP]
> Set `auto_activate = false` (default) to review and manually approve LLM-generated skill improvements before they go live.

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `a2a` | Enabled | [A2A protocol](https://github.com/a2aproject/A2A) client and server for agent-to-agent communication |
| `mcp` | Disabled | MCP client for external tool servers via stdio transport |
| `candle` | Disabled | Local HuggingFace model inference via [candle](https://github.com/huggingface/candle) (GGUF quantized models) |
| `metal` | Disabled | Metal GPU acceleration for candle on macOS (implies `candle`) |
| `cuda` | Disabled | CUDA GPU acceleration for candle on Linux (implies `candle`) |
| `orchestrator` | Disabled | Multi-model routing with task-based classification and fallback chains |
| `vault-age` | Disabled | Age-encrypted vault backend for file-based secret storage ([age](https://age-encryption.org/)) |
| `self-learning` | Disabled | Skill evolution via failure detection, self-reflection, and LLM-generated improvements |

Build with specific features:

```bash
cargo build --release                                     # default (a2a only)
cargo build --release --features candle,orchestrator      # local inference + orchestrator
cargo build --release --features candle,metal             # macOS with Metal GPU
cargo build --release --features self-learning            # skill evolution system
cargo build --release --features vault-age               # age-encrypted secrets vault
cargo build --release --no-default-features               # minimal binary
```

## Architecture

<details>
<summary><b>🏗️ Project Structure</b> (click to expand)</summary>

```
zeph (binary)
├── zeph-core       Agent loop, config, channel trait, context builder
├── zeph-llm        LlmProvider trait, Ollama + Claude + Candle backends, orchestrator, embeddings
├── zeph-skills     SKILL.md parser, registry with lazy body loading, embedding matcher, resource resolver, hot-reload
├── zeph-memory     SQLite + Qdrant, SemanticMemory orchestrator, summarization
├── zeph-channels   Telegram adapter (teloxide) with streaming
├── zeph-tools      ToolExecutor trait, ShellExecutor, WebScrapeExecutor, CompositeExecutor
├── zeph-mcp        MCP client via rmcp, multi-server lifecycle, unified tool matching (optional)
└── zeph-a2a        A2A protocol client + server, agent discovery, JSON-RPC 2.0 (optional)
```

</details>

> [!IMPORTANT]
> Requires Rust 1.88+ (Edition 2024). Native async traits are used throughout — no `async-trait` crate.

## License

[MIT](LICENSE)
