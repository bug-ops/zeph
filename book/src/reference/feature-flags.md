# Feature Flags

Zeph uses Cargo feature flags to control optional functionality. The remaining optional features are organized into **use-case bundles** for common deployment scenarios, with individual flags available for fine-grained control.

## Use-Case Bundles

Bundles are named Cargo features that group individual flags by deployment scenario. Use a bundle to get a sensible default for your use case without listing individual flags.

| Bundle | Included Features | Description |
|--------|-------------------|-------------|
| `desktop` | `tui`, `session`, `index` | Interactive desktop agent with TUI dashboard, session persistence, and AST-based code indexing |
| `ide` | `acp`, `acp-http` | IDE integration via ACP (Zed, Helix, VS Code) |
| `server` | `gateway`, `a2a`, `otel`, `prometheus`, `session` | Headless server deployment: HTTP webhook gateway, A2A agent protocol, OpenTelemetry tracing, Prometheus metrics, session persistence |
| `chat` | `discord`, `slack` | Chat platform adapters |
| `ml` | `candle`, `pdf` | Local ML inference (HuggingFace GGUF) and PDF document loading |
| `full` | `desktop` + `ide` + `server` + `chat` + `pdf` + `scheduler` + `classifiers` + `profiling` + `sandbox` + `gonka` | Everything intended to ship in a release binary, except hardware-exclusive (`metal`, `cuda`, `postgres`) and dev-only harness features (`bench`, `testing`) |

### Bundle build examples

```bash
cargo build --release --features desktop          # TUI agent for daily use
cargo build --release --features ide              # IDE assistant (ACP)
cargo build --release --features server           # headless server/daemon
cargo build --release --features desktop,server   # combined: TUI + server
cargo build --release --features ml               # local model inference
cargo build --release --features ml,metal         # local inference with Metal GPU (macOS)
cargo build --release --features ml,cuda          # local inference with CUDA GPU (Linux)
cargo build --release --features full             # everything except hardware-exclusive/dev-only features
cargo build --release --features full,ml          # everything including local inference
cargo build --release --features full,testing     # full plus mock LLM providers for testing
```

> Bundles are purely additive. All existing `--features tui,scheduler` style builds continue to work unchanged.

> **No `cli` bundle**: the default build (`cargo build --release`, no features) already represents the minimal CLI use case. A separate `cli` bundle would be a no-op alias.

> **`full` does not imply `testing`**: mock LLM providers are a dev-only test double, not something a release binary should ship with `full` alone. Add `testing` explicitly if you need `zeph-llm`'s mock provider outside `cargo test`.

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
| Daemon supervisor | Daemon supervisor with component lifecycle, PID file, and health monitoring |
| Task orchestration | DAG-based execution with failure strategies and SQLite persistence, including LLM-backed planning/verification |
| Graph memory | SQLite-based knowledge graph with entity-relationship tracking and BFS traversal |
| Guardrail | Content sanitization, PII filtering, exfiltration guard, and quarantine |
| Context compression | Reactive and focus-driven context compaction with summarization |
| Compression guidelines | Failure-driven guideline generation to improve future compaction quality |
| Policy enforcer | Declarative tool policy enforcement with LLM-based adversarial gate |
| LSP context injection | Automatic LSP diagnostics, hover, and reference injection into tool calls |
| Experiments | Autonomous self-experimentation engine with LLM-as-judge evaluation |
| Bundled skills | SKILL.md files compiled into the binary via `include_dir` |
| Speech-to-text | OpenAI Whisper API transcription for audio input |
| `zeph://` deep-link URI dispatch | OS-level `zeph://` scheme registration and prompt injection (spec #066); registration itself remains opt-in at the user level via `--init` or a CLI subcommand |
| Cocoon inference provider | TEE-sidecar confidential-compute provider (spec #055) |
| Skill/plugin registry marketplace | `zeph skill search`/`get` and `zeph plugin search`/`get` against external registries (e.g. skills.sh); opt-in via config, no network calls by default (spec #045-adjacent, #5869) |

## Optional Features

| Feature | Description |
|---------|-------------|
| `tui` | ratatui-based TUI dashboard with real-time agent metrics |
| `candle` | Local HuggingFace model inference via [candle](https://github.com/huggingface/candle) (GGUF quantized models) and local Whisper STT ([guide](../advanced/multimodal.md#local-whisper-candle)) |
| `metal` | Metal GPU acceleration for candle on macOS — implies `candle` |
| `cuda` | CUDA GPU acceleration for candle on Linux — implies `candle` |
| `discord` | Discord channel adapter with Gateway v10 WebSocket and slash commands ([guide](../advanced/channels.md#discord-channel)) |
| `slack` | Slack channel adapter with Events API webhook and HMAC-SHA256 verification ([guide](../advanced/channels.md#slack-channel)) |
| `acp` | ACP (Agent Client Protocol) server over stdio for IDE embedding — includes the stabilised-upstream `unstable-session-*` handlers (Zed, Helix, VS Code) ([guide](../advanced/acp.md)) |
| `acp-http` | ACP server over HTTP+SSE and WebSocket transport — implies `acp` ([guide](../advanced/acp.md#http-transport)) |
| `a2a` | [A2A protocol](https://github.com/a2aproject/A2A) client and server for agent-to-agent communication |
| `gateway` | HTTP gateway for webhook ingestion with bearer auth and rate limiting ([guide](../advanced/gateway.md)) |
| `prometheus` | OpenMetrics `/metrics` endpoint — implies `gateway` |
| `scheduler` | Cron-based periodic task scheduler with SQLite persistence, including the `update_check` handler for automatic version notifications ([guide](../advanced/daemon.md#cron-scheduler)) |
| `session` | Session persistence, event-log replay, and `zeph serve`'s HTTP/SSE session API (spec #068) |
| `otel` | OpenTelemetry tracing export via OTLP/gRPC ([guide](../advanced/observability.md)) |
| `pdf` | PDF document loading via [pdf-extract](https://crates.io/crates/pdf-extract) for the document ingestion pipeline |
| `classifiers` | ML-based content classifiers via local candle inference (implies `candle`) |
| `index` | AST-based code indexing, semantic retrieval, and repo map generation (spec #017) |
| `gonka` | gonka.ai decentralized inference provider (specs #051, #052) |
| `profiling` | Diagnostic tracing spans (Chrome trace format) and system metrics via `sysinfo`; zero overhead when not actively tracing |
| `profiling-alloc` | Per-span heap allocation counters — implies `profiling` |
| `profiling-pyroscope` | Continuous profiling export to Pyroscope — implies `profiling` and `otel` |
| `sandbox` | Linux `landlock`/`seccompiler` and macOS Seatbelt tool-execution sandboxing; runtime-disabled by default (`tools.sandbox.enabled = false`) |
| `testing` | Mock LLM provider test doubles (`zeph-llm/testing`) — dev-only, not in `full` |
| `bench` | Benchmark harness CLI (spec #034) — dev-only, not in `full` |
| `sqlite` | SQLite database backend via `sqlx` (enabled by default) |
| `postgres` | PostgreSQL database backend via `sqlx` — mutually exclusive with `sqlite`; activating both causes a compile error. Use `--no-default-features --features postgres` to switch |

> [!IMPORTANT]
> `--all-features` activates both `sqlite` and `postgres` simultaneously, which triggers a `compile_error!` in `zeph-db`. Use `--features full` for local development instead (it defaults to `sqlite` via the crate's default features; add `postgres` explicitly with `--no-default-features --features full,postgres` for a Postgres build).

## Crate-Level Features

`zeph-acp` exposes its own `unstable-*` flags for ACP protocol surface still marked unstable upstream. The `acp` feature in the root crate enables all of them automatically — there is no separate `acp-unstable` flag.

| Crate | Feature | In `acp`? | Description |
|-------|---------|-----------|-------------|
| `zeph-acp` | `unstable-session-fork` | yes | `session/fork` — clone session history into a new session |
| `zeph-acp` | `unstable-session-usage` | yes | `UsageUpdate` session notification — per-turn token consumption sent after each LLM response |
| `zeph-acp` | `unstable-elicitation` | yes | `elicitation/create` — structured user-input requests mid-turn |
| `zeph-acp` | `unstable-llm-providers` | yes | LLM provider listing/switching extension |
| `zeph-acp` | `unstable-auth-methods` | yes | Auth-methods advertisement extension |
| `zeph-acp` | `unstable-cancel-request` | no | Wires the `$/cancel_request` notification onto the internal cancel signal — deliberate local opt-in, not enabled by `acp` or `default` (#5362) |

Session lifecycle handlers that were previously gated behind `unstable-session-delete`, `unstable-session-resume`, `unstable-logout`, `unstable-session-add-dirs`, and `unstable-message-id` compile unconditionally — the corresponding upstream ACP features stabilised, and the Zeph Cargo features were removed entirely rather than kept as no-op tombstones.

Disable all `unstable-*` handlers to build a minimal ACP server without them:

```bash
cargo build -p zeph-acp --no-default-features
```

## Build Examples

```bash
cargo build --release                                      # default build (scheduler + sqlite + always-on features)
cargo build --release --features desktop                   # TUI dashboard + session + index
cargo build --release --features ide                       # ACP (includes the stabilised unstable-session-* handlers)
cargo build --release --features server                    # gateway + a2a + otel + prometheus + session
cargo build --release --features desktop,server            # combined desktop and server
cargo build --release --features ml,metal                  # local inference with Metal GPU (macOS)
cargo build --release --features ml,cuda                   # local inference with CUDA GPU (Linux)
cargo build --release --features full                      # everything except hardware-exclusive/dev-only features
cargo build --release --features full,testing               # full plus mock LLM providers
cargo build --release --features tui                       # individual flag still works
cargo build --release --features tui,a2a                   # combine individual flags freely
```

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
