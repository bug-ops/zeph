# Zeph

Lightweight AI agent in Rust with hybrid inference (Ollama / Claude), CLI and Telegram interfaces, and a skills-first architecture.

## Features

- **Hybrid LLM inference** -- local Ollama or cloud Claude via Anthropic API
- **CLI and Telegram** -- interactive terminal by default, Telegram bot when token is set
- **Skills system** -- drop `SKILL.md` files into `skills/` to extend agent capabilities
- **Shell execution** -- agent extracts bash blocks from LLM responses and runs them
- **SQLite memory** -- conversation persistence with configurable history limit
- **Graceful shutdown** -- handles SIGINT/SIGTERM cleanly

## Installation

```bash
cargo build --release
```

The binary is produced at `target/release/zeph`.

## Configuration

Zeph loads `config/default.toml` at startup and applies environment variable overrides.

### Config file

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

## Usage

### CLI mode (default)

```bash
./target/release/zeph
```

Type messages at the `You:` prompt. Type `exit`, `quit`, or press Ctrl-D to stop.

### Telegram mode

Set `ZEPH_TELEGRAM_TOKEN` to your bot token:

```bash
ZEPH_TELEGRAM_TOKEN="123:ABC" ./target/release/zeph
```

Optionally restrict access via `telegram.allowed_users` in config.

## Skills

Create a directory under `skills/` with a `SKILL.md` file containing YAML frontmatter:

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

## License

[MIT](LICENSE)
