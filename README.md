# Zeph

[![CI](https://img.shields.io/github/actions/workflow/status/bug-ops/zeph/ci.yml?branch=main)](https://github.com/bug-ops/zeph/actions)
[![codecov](https://codecov.io/gh/bug-ops/zeph/graph/badge.svg?token=S5O0GR9U6G)](https://codecov.io/gh/bug-ops/zeph)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Lightweight AI agent with hybrid inference (Ollama / Claude), skills-first architecture, and multi-channel I/O.

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

## Usage

### CLI mode (default)

```bash
./target/release/zeph
```

Type messages at the `You:` prompt. Type `exit`, `quit`, or press Ctrl-D to stop.

### Telegram mode

```bash
ZEPH_TELEGRAM_TOKEN="123:ABC" ./target/release/zeph
```

> [!TIP]
> Restrict access by setting `telegram.allowed_users` in the config file.

## Configuration

Zeph loads `config/default.toml` at startup and applies environment variable overrides.

```toml
[agent]
name = "Zeph"

[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "mistral:7b"

[llm.cloud]
model = "claude-sonnet-4-5-20250929"
max_tokens = 4096

[skills]
paths = ["./skills"]

[memory]
sqlite_path = "./data/zeph.db"
history_limit = 50
```

### Environment variables

| Variable | Description |
|----------|-------------|
| `ZEPH_LLM_PROVIDER` | `ollama` or `claude` |
| `ZEPH_LLM_BASE_URL` | Ollama API endpoint |
| `ZEPH_LLM_MODEL` | Model name for Ollama |
| `ZEPH_CLAUDE_API_KEY` | Anthropic API key (required for Claude) |
| `ZEPH_TELEGRAM_TOKEN` | Telegram bot token (enables Telegram mode) |
| `ZEPH_SQLITE_PATH` | SQLite database path |

## Skills

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

## Docker

### Apple Silicon (Ollama on host with Metal GPU)

```bash
ollama serve &
ZEPH_LLM_BASE_URL=http://host.docker.internal:11434 docker compose up
```

### Ollama in container (CPU)

```bash
docker compose --profile cpu up
```

### Linux with NVIDIA GPU

```bash
docker compose --profile gpu -f docker-compose.yml -f docker-compose.gpu.yml up
```

> [!TIP]
> Add `--build` to rebuild the image after code changes. Pass `ZEPH_TELEGRAM_TOKEN=xxx` to enable Telegram mode.

## Architecture

```
zeph (binary)
├── zeph-core       Agent loop, config, channel trait
├── zeph-llm        LlmProvider trait, Ollama + Claude backends, token streaming
├── zeph-skills     SKILL.md parser, registry, prompt formatter
├── zeph-memory     SQLite conversation persistence
├── zeph-channels   Telegram adapter (teloxide) with streaming
└── zeph-tools      ToolExecutor trait, ShellExecutor with bash parser
```

> [!IMPORTANT]
> Requires Rust 1.88+ (Edition 2024). Native async traits are used throughout — no `async-trait` crate.

## License

[MIT](LICENSE)
