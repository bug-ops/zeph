# Feature Flags

Zeph uses Cargo feature flags to control optional functionality. As of M26, eight previously optional features are now always-on and compiled into every build. The remaining optional features are explicitly opt-in.

## Always-On (compiled unconditionally)

| Feature | Description |
|---------|-------------|
| `openai` | OpenAI-compatible provider (GPT, Together, Groq, Fireworks, etc.) |
| `compatible` | `CompatibleProvider` for OpenAI-compatible third-party APIs |
| `orchestrator` | Multi-model routing with task-based classification and fallback chains |
| `router` | `RouterProvider` for chaining multiple providers with fallback |
| `self-learning` | Skill evolution via failure detection, self-reflection, and LLM-generated improvements |
| `qdrant` | Qdrant-backed vector storage for skill matching and MCP tool registry |
| `vault-age` | Age-encrypted vault backend for file-based secret storage ([age](https://age-encryption.org/)) |
| `mcp` | MCP client for external tool servers via stdio/HTTP transport |

## Optional Features

| Feature | Description |
|---------|-------------|
| `tui` | ratatui-based TUI dashboard with real-time agent metrics |
| `candle` | Local HuggingFace model inference via [candle](https://github.com/huggingface/candle) (GGUF quantized models) and local Whisper STT ([guide](../advanced/multimodal.md#local-whisper-candle)) |
| `metal` | Metal GPU acceleration for candle on macOS (implies `candle`) |
| `cuda` | CUDA GPU acceleration for candle on Linux (implies `candle`) |
| `discord` | Discord channel adapter with Gateway v10 WebSocket and slash commands ([guide](../advanced/channels.md#discord-channel)) |
| `slack` | Slack channel adapter with Events API webhook and HMAC-SHA256 verification ([guide](../advanced/channels.md#slack-channel)) |
| `a2a` | [A2A protocol](https://github.com/a2aproject/A2A) client and server for agent-to-agent communication |
| `index` | AST-based code indexing and semantic retrieval via tree-sitter ([guide](../advanced/code-indexing.md)) |
| `gateway` | HTTP gateway for webhook ingestion with bearer auth and rate limiting ([guide](../advanced/gateway.md)) |
| `daemon` | Daemon supervisor with component lifecycle, PID file, and health monitoring ([guide](../advanced/daemon.md)) |
| `scheduler` | Cron-based periodic task scheduler with SQLite persistence, including the `update_check` handler for automatic version notifications ([guide](../advanced/daemon.md#cron-scheduler)) |
| `stt` | Speech-to-text transcription via OpenAI Whisper API ([guide](../advanced/multimodal.md#audio-input)) |
| `otel` | OpenTelemetry tracing export via OTLP/gRPC ([guide](../advanced/observability.md)) |
| `pdf` | PDF document loading via [pdf-extract](https://crates.io/crates/pdf-extract) for the document ingestion pipeline |
| `mock` | Mock providers and channels for testing |

## Build Examples

```bash
cargo build --release                                      # default build (always-on features included)
cargo build --release --features metal                     # macOS with Metal GPU
cargo build --release --features cuda                      # Linux with NVIDIA GPU
cargo build --release --features tui                       # with TUI dashboard
cargo build --release --features discord                   # with Discord bot
cargo build --release --features slack                     # with Slack bot
cargo build --release --features gateway,daemon,scheduler  # with infrastructure components
cargo build --release --features full                      # all optional features
```

The `full` feature enables every optional feature except `metal`, `cuda`, and `otel`.

## zeph-index Language Features

When `index` is enabled, tree-sitter grammars are controlled by sub-features on the `zeph-index` crate. All are enabled by default.

| Feature | Languages |
|---------|-----------|
| `lang-rust` | Rust |
| `lang-python` | Python |
| `lang-js` | JavaScript, TypeScript |
| `lang-go` | Go |
| `lang-config` | Bash, TOML, JSON, Markdown |
