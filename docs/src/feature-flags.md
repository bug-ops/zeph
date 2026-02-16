# Feature Flags

Zeph uses Cargo feature flags to control optional functionality. Default features cover common use cases; platform-specific and experimental features are opt-in.

| Feature | Default | Description |
|---------|---------|-------------|
| `a2a` | Enabled | [A2A protocol](https://github.com/a2aproject/A2A) client and server for agent-to-agent communication |
| `openai` | Enabled | OpenAI-compatible provider (GPT, Together, Groq, Fireworks, etc.) |
| `mcp` | Enabled | MCP client for external tool servers via stdio/HTTP transport |
| `candle` | Enabled | Local HuggingFace model inference via [candle](https://github.com/huggingface/candle) (GGUF quantized models) |
| `orchestrator` | Enabled | Multi-model routing with task-based classification and fallback chains |
| `self-learning` | Enabled | Skill evolution via failure detection, self-reflection, and LLM-generated improvements |
| `qdrant` | Enabled | Qdrant-backed vector storage for skill matching (`zeph-skills`) and MCP tool registry (`zeph-mcp`) |
| `vault-age` | Enabled | Age-encrypted vault backend for file-based secret storage ([age](https://age-encryption.org/)) |
| `index` | Enabled | AST-based code indexing and semantic retrieval via tree-sitter ([guide](guide/code-indexing.md)) |
| `otel` | Disabled | OpenTelemetry tracing export via OTLP/gRPC ([guide](guide/observability.md)) |
| `gateway` | Disabled | HTTP gateway for webhook ingestion with bearer auth and rate limiting ([guide](guide/gateway.md)) |
| `daemon` | Disabled | Daemon supervisor with component lifecycle, PID file, and health monitoring ([guide](guide/daemon.md)) |
| `scheduler` | Disabled | Cron-based periodic task scheduler with SQLite persistence ([guide](guide/scheduler.md)) |
| `tui` | Disabled | ratatui-based TUI dashboard with real-time agent metrics |
| `metal` | Disabled | Metal GPU acceleration for candle on macOS (implies `candle`) |
| `cuda` | Disabled | CUDA GPU acceleration for candle on Linux (implies `candle`) |

## Build Examples

```bash
cargo build --release                                     # all default features
cargo build --release --features metal                    # macOS with Metal GPU
cargo build --release --features cuda                     # Linux with NVIDIA GPU
cargo build --release --features tui                      # with TUI dashboard
cargo build --release --features gateway,daemon,scheduler  # with infrastructure components
cargo build --release --no-default-features               # minimal binary
```

## zeph-index Language Features

When `index` is enabled, tree-sitter grammars are controlled by sub-features on the `zeph-index` crate. All are enabled by default.

| Feature | Languages |
|---------|-----------|
| `lang-rust` | Rust |
| `lang-python` | Python |
| `lang-js` | JavaScript, TypeScript |
| `lang-go` | Go |
| `lang-config` | Bash, TOML, JSON, Markdown |
