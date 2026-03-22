# Feature Flags

Zeph uses Cargo feature flags to control optional functionality. The remaining optional features are organized into **use-case bundles** for common deployment scenarios, with individual flags available for fine-grained control.

## Use-Case Bundles

Bundles are named Cargo features that group individual flags by deployment scenario. Use a bundle to get a sensible default for your use case without listing individual flags.

| Bundle | Included Features | Description |
|--------|-------------------|-------------|
| `desktop` | `tui`, `scheduler`, `compression-guidelines` | Interactive desktop agent with TUI dashboard, cron scheduler, and failure-driven compression |
| `ide` | `acp`, `acp-http`, `lsp-context` | IDE integration via ACP (Zed, Helix, VS Code) with LSP context injection |
| `server` | `gateway`, `a2a`, `scheduler`, `otel` | Headless server deployment: HTTP webhook gateway, A2A agent protocol, cron scheduler, OpenTelemetry tracing |
| `chat` | `discord`, `slack` | Chat platform adapters |
| `ml` | `candle`, `pdf`, `stt` | Local ML inference (HuggingFace GGUF), PDF document loading, and Whisper speech-to-text |
| `full` | `desktop` + `ide` + `server` + `chat` + `pdf` + `stt` + `acp-unstable` + `experiments` | All optional features except `candle`, `metal`, and `cuda` (hardware-specific) |

### Bundle build examples

```bash
cargo build --release --features desktop          # TUI agent for daily use
cargo build --release --features ide              # IDE assistant (ACP)
cargo build --release --features server           # headless server/daemon
cargo build --release --features desktop,server   # combined: TUI + server
cargo build --release --features ml               # local model inference
cargo build --release --features ml,metal         # local inference with Metal GPU (macOS)
cargo build --release --features ml,cuda          # local inference with CUDA GPU (Linux)
cargo build --release --features full             # all optional features (CI / release builds)
cargo build --release --features full,ml          # everything including local inference
```

> Bundles are purely additive. All existing `--features tui,scheduler` style builds continue to work unchanged.

> **No `cli` bundle**: the default build (`cargo build --release`, no features) already represents the minimal CLI use case. A separate `cli` bundle would be a no-op alias.

## Built-In Capabilities (always compiled, no feature flag required)

The following capabilities compile unconditionally into every build. They are **not** Cargo feature flags — there is no `#[cfg(feature)]` gate and no way to disable them. They are listed here for reference only.

| Capability | Description |
|------------|-------------|
| OpenAI provider | OpenAI-compatible provider (GPT, Together, Groq, Fireworks, etc.) |
| Compatible provider | `CompatibleProvider` for OpenAI-compatible third-party APIs |
| Multi-model orchestrator | Multi-model routing with task-based classification and fallback chains |
| Router provider | `RouterProvider` for chaining multiple providers with fallback |
| Self-learning | Skill evolution via failure detection, self-reflection, and LLM-generated improvements |
| Qdrant integration | Qdrant-backed vector storage for skill matching and MCP tool registry |
| Age vault | Age-encrypted vault backend for file-based secret storage ([age](https://age-encryption.org/)) |
| MCP client | MCP client for external tool servers via stdio/HTTP transport |
| Mock providers | Mock providers and channels for integration testing |
| Daemon supervisor | Daemon supervisor with component lifecycle, PID file, and health monitoring |
| Task orchestration | DAG-based execution with failure strategies and SQLite persistence |
| Graph memory | SQLite-based knowledge graph with entity-relationship tracking and BFS traversal |

## Optional Features

| Feature | Description |
|---------|-------------|
| `tui` | ratatui-based TUI dashboard with real-time agent metrics |
| `candle` | Local HuggingFace model inference via [candle](https://github.com/huggingface/candle) (GGUF quantized models) and local Whisper STT ([guide](../advanced/multimodal.md#local-whisper-candle)) |
| `metal` | Metal GPU acceleration for candle on macOS — implies `candle` |
| `cuda` | CUDA GPU acceleration for candle on Linux — implies `candle` |
| `discord` | Discord channel adapter with Gateway v10 WebSocket and slash commands ([guide](../advanced/channels.md#discord-channel)) |
| `slack` | Slack channel adapter with Events API webhook and HMAC-SHA256 verification ([guide](../advanced/channels.md#slack-channel)) |
| `a2a` | [A2A protocol](https://github.com/a2aproject/A2A) client and server for agent-to-agent communication |
| `lsp-context` | Automatic LSP context injection: diagnostics after `write_file`, optional hover on `read_file`, references before `rename_symbol`. Hooks into the tool execution pipeline and call mcpls via the existing MCP client. Requires mcpls configured under `[[mcp.servers]]`. Enable with `--lsp-context` or `agent.lsp.enabled = true` ([guide](../guides/lsp.md#lsp-context-injection)). Note: the ACP LSP extension (IDE-proxied LSP via `ext_method`) is part of the `acp` feature, not `lsp-context` |
| `gateway` | HTTP gateway for webhook ingestion with bearer auth and rate limiting ([guide](../advanced/gateway.md)) |
| `scheduler` | Cron-based periodic task scheduler with SQLite persistence, including the `update_check` handler for automatic version notifications ([guide](../advanced/daemon.md#cron-scheduler)) |
| `stt` | Speech-to-text transcription via OpenAI Whisper API ([guide](../advanced/multimodal.md#audio-input)) |
| `otel` | OpenTelemetry tracing export via OTLP/gRPC ([guide](../advanced/observability.md)) |
| `pdf` | PDF document loading via [pdf-extract](https://crates.io/crates/pdf-extract) for the document ingestion pipeline |
| `experiments` | Autonomous self-experimentation engine with benchmark datasets, LLM-as-judge evaluation, and cron-based scheduled runs when combined with the `scheduler` feature ([guide](../concepts/experiments.md)) |

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
cargo build --release                                      # default build (always-on features only)
cargo build --release --features desktop                   # TUI + scheduler + compression-guidelines
cargo build --release --features ide                       # ACP + LSP context injection
cargo build --release --features server                    # gateway + a2a + scheduler + otel
cargo build --release --features desktop,server            # combined desktop and server
cargo build --release --features ml,metal                  # local inference with Metal GPU (macOS)
cargo build --release --features ml,cuda                   # local inference with CUDA GPU (Linux)
cargo build --release --features full                      # all optional features (except candle/metal/cuda)
cargo build --release --features tui                       # individual flag still works
cargo build --release --features tui,a2a                   # combine individual flags freely
```

The `full` feature enables every optional feature except `candle`, `metal`, and `cuda` (hardware-specific, opt-in).

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

Tree-sitter grammars are controlled by sub-features on the `zeph-index` crate (always-on). All are enabled by default.

| Feature | Languages |
|---------|-----------|
| `lang-rust` | Rust |
| `lang-python` | Python |
| `lang-js` | JavaScript, TypeScript |
| `lang-go` | Go |
| `lang-config` | Bash, TOML, JSON, Markdown |
