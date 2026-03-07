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
| `graph-memory` | SQLite-based knowledge graph with entity-relationship tracking and BFS traversal ([guide](../concepts/graph-memory.md)) |
| `lsp-context` | Automatic LSP context injection: diagnostics after `write_file`, optional hover on `read_file`, references before `rename_symbol`. Hooks into the tool execution pipeline and call mcpls via the existing MCP client. Requires mcpls configured under `[[mcp.servers]]`. Enable with `--lsp-context` or `agent.lsp.enabled = true` ([guide](../guides/lsp.md#lsp-context-injection)) |
| `gateway` | HTTP gateway for webhook ingestion with bearer auth and rate limiting ([guide](../advanced/gateway.md)) |
| `daemon` | Daemon supervisor with component lifecycle, PID file, and health monitoring. Combined with `a2a`, enables `--daemon` headless mode ([guide](../guides/daemon-mode.md)) |
| `scheduler` | Cron-based periodic task scheduler with SQLite persistence, including the `update_check` handler for automatic version notifications ([guide](../advanced/daemon.md#cron-scheduler)) |
| `stt` | Speech-to-text transcription via OpenAI Whisper API ([guide](../advanced/multimodal.md#audio-input)) |
| `orchestration` | Task orchestration with DAG-based execution, failure strategies, and SQLite persistence ([guide](../concepts/task-orchestration.md)) |
| `otel` | OpenTelemetry tracing export via OTLP/gRPC ([guide](../advanced/observability.md)) |
| `pdf` | PDF document loading via [pdf-extract](https://crates.io/crates/pdf-extract) for the document ingestion pipeline |
| `mock` | Mock providers and channels for testing |

## Crate-Level Features

Some workspace crates expose their own feature flags for fine-grained control:

| Crate | Feature | Default | Description |
|-------|---------|---------|-------------|
| `zeph-llm` | `schema` | on | Enables `schemars` dependency and typed output API (`chat_typed`, `Extractor`, `cached_schema`) |
| `zeph-acp` | `unstable-session-list` | on | `list_sessions` RPC handler — enumerate in-memory sessions (unstable, see [ACP guide](../advanced/acp.md#list_sessions)) |
| `zeph-acp` | `unstable-session-fork` | on | `fork_session` RPC handler — clone session history into a new session (unstable, see [ACP guide](../advanced/acp.md#fork_session)) |
| `zeph-acp` | `unstable-session-resume` | on | `resume_session` RPC handler — reattach to a persisted session without replaying events (unstable, see [ACP guide](../advanced/acp.md#resume_session)) |
| `zeph-acp` | `unstable-session-usage` | on | `UsageUpdate` session notification — per-turn token consumption (`used`/`size`) sent after each LLM response; IDEs that handle this event render a context window badge (unstable, see [ACP guide](../advanced/acp.md#usage-tracking-unstable-session-usage)) |
| `zeph-acp` | `unstable-session-model` | on | `set_session_model` handler — IDE model picker support; emits `SetSessionModel` notification on switch (unstable, see [ACP guide](../advanced/acp.md#model-picker-unstable-session-model)) |
| `zeph-acp` | `unstable-session-info-update` | on | `SessionInfoUpdate` notification — auto-generated session title emitted after the first exchange (unstable, see [ACP guide](../advanced/acp.md#session-title-unstable-session-info-update)) |

### ACP session management (unstable)

The `unstable-session-*` flags gate ACP session lifecycle handlers and IDE integration features that depend on draft ACP spec additions. They are enabled by default but the API surface may change before the spec stabilises. Each flag also enables the corresponding feature in `agent-client-protocol` so the SDK advertises the capability during `initialize`.

The root crate provides a composite flag to enable all six at once:

| Feature | Description |
|---------|-------------|
| `acp-unstable` | Enables all `unstable-session-*` flags in `zeph-acp` (list, fork, resume, usage, model, info-update) |

Disable all six to build a minimal ACP server without session management or IDE integration features:

```bash
cargo build -p zeph-acp --no-default-features
```

Disable the `schema` feature to compile `zeph-llm` without `schemars`:

```bash
cargo build -p zeph-llm --no-default-features
```

## Build Examples

```bash
cargo build --release                                      # default build (always-on features included)
cargo build --release --features metal                     # macOS with Metal GPU
cargo build --release --features cuda                      # Linux with NVIDIA GPU
cargo build --release --features tui                       # with TUI dashboard
cargo build --release --features discord                   # with Discord bot
cargo build --release --features slack                     # with Slack bot
cargo build --release --features daemon,a2a                # headless daemon with A2A endpoint
cargo build --release --features tui,a2a                   # TUI with remote daemon support
cargo build --release --features gateway,daemon,scheduler  # with infrastructure components
cargo build --release --features lsp-context               # with LSP context injection
cargo build --release --features full                      # all optional features
```

The `full` feature enables every optional feature except `candle`, `metal`, and `cuda`.

## Build Profiles

| Profile | LTO | Codegen Units | Use Case |
|---------|-----|---------------|----------|
| `dev` | off | 256 | Local development |
| `release` | fat | 1 | Production binaries |
| `ci` | thin | 16 | CI release builds (~2-3x faster link than release) |

Build with the CI profile:

```bash
cargo build --profile ci
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
