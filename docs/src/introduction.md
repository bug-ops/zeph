# Zeph

Lightweight AI agent with hybrid inference (Ollama / Claude / OpenAI / HuggingFace via candle), skills-first architecture, semantic memory with Qdrant, MCP client, A2A protocol support, multi-model orchestration, self-learning skill evolution, and multi-channel I/O.

Only relevant skills and MCP tools are injected into each prompt via vector similarity — keeping token usage minimal regardless of how many are installed.

**Cross-platform**: Linux, macOS, Windows (x86_64 + ARM64).

## Key Features

- **Hybrid inference** — Ollama (local), Claude (Anthropic), OpenAI (GPT + compatible APIs), Candle (HuggingFace GGUF)
- **Skills-first architecture** — embedding-based skill matching selects only top-K relevant skills per query, not all
- **Semantic memory** — SQLite for structured data + Qdrant for vector similarity search
- **MCP client** — connect external tool servers via Model Context Protocol (stdio + HTTP transport)
- **A2A protocol** — agent-to-agent communication via JSON-RPC 2.0 with SSE streaming
- **Model orchestrator** — route tasks to different providers with automatic fallback chains
- **Self-learning** — skills evolve through failure detection, self-reflection, and LLM-generated improvements
- **Multi-channel I/O** — CLI and Telegram with streaming support
- **Token-efficient** — prompt size is O(K) not O(N), where K is max active skills and N is total installed

## Quick Start

```bash
git clone https://github.com/bug-ops/zeph
cd zeph
cargo build --release
./target/release/zeph
```

See [Installation](getting-started/installation.md) for pre-built binaries and Docker options.

## Requirements

- Rust 1.88+ (Edition 2024)
- Ollama (for local inference and embeddings) or cloud API key (Claude / OpenAI)
- Docker (optional, for Qdrant semantic memory and containerized deployment)
