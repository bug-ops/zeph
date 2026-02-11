# Quick Start

## CLI Mode (default)

**Unix (Linux/macOS):**
```bash
./target/release/zeph
```

**Windows:**
```powershell
.\target\release\zeph.exe
```

Type messages at the `You:` prompt. Type `exit`, `quit`, or press Ctrl-D to stop.

## Telegram Mode

**Unix (Linux/macOS):**
```bash
ZEPH_TELEGRAM_TOKEN="123:ABC" ./target/release/zeph
```

**Windows:**
```powershell
$env:ZEPH_TELEGRAM_TOKEN="123:ABC"; .\target\release\zeph.exe
```

Restrict access by setting `telegram.allowed_users` in the [config file](configuration.md):

```toml
[telegram]
allowed_users = ["your_username"]
```

## Ollama Setup

When using Ollama (default provider), ensure both the LLM model and embedding model are pulled:

```bash
ollama pull mistral:7b
ollama pull qwen3-embedding
```

The default configuration uses `mistral:7b` for text generation and `qwen3-embedding` for vector embeddings.

## Cloud Providers

For Claude:
```bash
ZEPH_CLAUDE_API_KEY=sk-ant-... ./target/release/zeph
```

For OpenAI:
```bash
ZEPH_LLM_PROVIDER=openai ZEPH_OPENAI_API_KEY=sk-... ./target/release/zeph
```

See [Configuration](configuration.md) for the full reference.
