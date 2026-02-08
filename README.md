# Zeph

[![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main)](https://github.com/bug-ops/zeph/actions)
[![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
[![Security](https://img.shields.io/badge/security-hardened-brightgreen)](SECURITY.md)
[![Trivy Scan](https://img.shields.io/badge/Trivy-0%20CVEs-success)](https://github.com/bug-ops/zeph/security)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-blue)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Lightweight AI agent with hybrid inference (Ollama / Claude), skills-first architecture, semantic memory with Qdrant, and multi-channel I/O. **Cross-platform**: Linux, macOS, Windows (x86_64 + ARM64).

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
docker pull ghcr.io/bug-ops/zeph:v0.4.3
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
```

</details>

> [!IMPORTANT]
> Shell commands are filtered for safety. See [Security](#security) section for complete list of 12 blocked patterns and customization options.

<details>
<summary><b>🔧 Environment Variables</b> (click to expand)</summary>

| Variable | Description |
|----------|-------------|
| `ZEPH_LLM_PROVIDER` | `ollama` or `claude` |
| `ZEPH_LLM_BASE_URL` | Ollama API endpoint |
| `ZEPH_LLM_MODEL` | Model name for Ollama |
| `ZEPH_LLM_EMBEDDING_MODEL` | Embedding model for Ollama (default: `qwen3-embedding`) |
| `ZEPH_CLAUDE_API_KEY` | Anthropic API key (required for Claude) |
| `ZEPH_TELEGRAM_TOKEN` | Telegram bot token (enables Telegram mode) |
| `ZEPH_SQLITE_PATH` | SQLite database path |
| `ZEPH_QDRANT_URL` | Qdrant server URL (default: `http://localhost:6334`) |
| `ZEPH_MEMORY_SUMMARIZATION_THRESHOLD` | Trigger summarization after N messages (default: 100) |
| `ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS` | Context budget for proportional token allocation (default: 0 = unlimited) |
| `ZEPH_TOOLS_TIMEOUT` | Shell command timeout in seconds (default: 30) |

</details>

## Skills

<details>
<summary><b>🛠️ Skills System</b> (click to expand)</summary>

Drop `SKILL.md` files into subdirectories under `skills/` to extend agent capabilities:

```
skills/
  web-search/
    SKILL.md
  file-ops/
    SKILL.md
```

`SKILL.md` format:

```markdown
---
name: web-search
description: Search the web for information.
---
# Instructions
Use curl to fetch search results...
```

All loaded skills are injected into the system prompt.

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

**Note:** Docker Compose automatically pulls the latest image from GitHub Container Registry. To use a specific version, set `ZEPH_IMAGE=ghcr.io/bug-ops/zeph:v0.4.3`.

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

### Using Specific Version

```bash
# Use a specific release version
ZEPH_IMAGE=ghcr.io/bug-ops/zeph:v0.4.3 docker compose up

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

- **Zero `unsafe` blocks:** Project policy enforces `#![forbid(unsafe_code)]`
- **No panic in production:** `unwrap()` and `expect()` linted via clippy
- **Secure dependencies:** All crates audited with `cargo-deny`
- **MSRV policy:** Rust 1.88+ (Edition 2024) for latest security patches

### Secrets Management

> [!CAUTION]
> API keys and tokens must be passed via environment variables. Never commit secrets to version control.

**Current:** Environment variables (`ZEPH_CLAUDE_API_KEY`, `ZEPH_TELEGRAM_TOKEN`)
**Planned:** Vault integration for centralized secret rotation (see [#70](https://github.com/bug-ops/zeph/issues/70))

### Reporting Security Issues

Found a vulnerability? See [SECURITY.md](SECURITY.md) for responsible disclosure process.

**Security contact:** Submit via GitHub Security Advisories (confidential)

## Architecture

<details>
<summary><b>🏗️ Project Structure</b> (click to expand)</summary>

```
zeph (binary)
├── zeph-core       Agent loop, config, channel trait, context builder
├── zeph-llm        LlmProvider trait, Ollama + Claude backends, token streaming, embeddings
├── zeph-skills     SKILL.md parser, registry, prompt formatter
├── zeph-memory     SQLite + Qdrant, SemanticMemory orchestrator, summarization
├── zeph-channels   Telegram adapter (teloxide) with streaming
└── zeph-tools      ToolExecutor trait, ShellExecutor with bash parser
```

</details>

> [!IMPORTANT]
> Requires Rust 1.88+ (Edition 2024). Native async traits are used throughout — no `async-trait` crate.

## License

[MIT](LICENSE)
