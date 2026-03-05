# zeph-core

[![Crates.io](https://img.shields.io/crates/v/zeph-core)](https://crates.io/crates/zeph-core)
[![docs.rs](https://img.shields.io/docsrs/zeph-core)](https://docs.rs/zeph-core)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

Core agent loop, configuration, context builder, metrics, vault, and sub-agent orchestration for Zeph.

## Overview

Core orchestration crate for the Zeph agent. Manages the main agent loop, bootstraps the application from TOML configuration with environment variable overrides, and assembles the LLM context from conversation history, skills, and memory. Includes sub-agent orchestration with zero-trust permission grants, background execution, filtered tool/skill access, persistent memory scopes, lifecycle hooks, persistent JSONL transcript storage with resume-by-ID, A2A-based in-process communication channels, and `/agent` CLI commands for runtime management. All other workspace crates are coordinated through `zeph-core`.

## Key modules

| Module | Description |
|--------|-------------|
| `agent` | `Agent<C>` — main loop driving inference and tool execution; ToolExecutor erased via `Box<dyn ErasedToolExecutor>`; supports external cancellation via `with_cancel_signal()`; `EnvironmentContext` cached at bootstrap and partially refreshed (git branch, model name) on skill reload only |
| `agent::context_manager` | `ContextManager` — owns token budget, compaction threshold, and safety margin; `should_compact()` is O(1) — reads `cached_prompt_tokens` set by the LLM response rather than scanning the message list |
| `agent::tool_orchestrator` | `ToolOrchestrator` — owns max iteration limit, doom-loop detection (rolling hash window with in-place hashing, no intermediate `String` allocation), summarization flag, and overflow config |
| `agent::learning_engine` | `LearningEngine` — owns `LearningConfig`, tracks per-turn reflection state; delegates self-learning decisions to `is_enabled()` / `mark_reflection_used()` |
| `agent::feedback_detector` | `FeedbackDetector` (regex) and `JudgeDetector` (LLM-backed) — implicit correction detection from user messages; `JudgeDetector` runs in background via `tokio::spawn` with sliding-window rate limiter (5 calls / 60 s) and XML-escaped adversarial-defense prompt; adaptive threshold gates judge invocation to the regex uncertainty zone |
| `agent::tool_execution` | Tool call handling, redaction, result processing; both the fenced-block path (`handle_tool_result`) and the structured tool-call path unconditionally emit `LoopbackEvent::ToolStart` (UUID generated per call) before execution and `LoopbackEvent::ToolOutput` (matching UUID, `is_error` flag) after; `call_llm_with_retry()` / `call_chat_with_tools_retry()` — auto-detect `ContextLengthExceeded`, compact context, and retry (max 2 attempts); `prune_stale_tool_outputs` invokes `count_tokens` once per `ToolResult` part |
| `agent::message_queue` | Message queue management |
| `agent::builder` | Agent builder API |
| `agent::commands` | Chat command dispatch (skills, feedback, skill management via `/skill install`, `/skill remove`, `/skill reject <name> <reason>`, sub-agent management via `/agent`, etc.) |
| `agent::utils` | Shared agent utilities |
| `bootstrap` | `AppBuilder` — fluent builder for application startup; split into submodules: `config` (config resolution, vault arg parsing), `health` (health check, provider warmup), `mcp` (MCP manager and registry), `provider` (provider factory functions), `skills` (skill matcher, embedding model helpers) |
| `channel` | `Channel` trait defining I/O adapters; `LoopbackChannel` / `LoopbackHandle` for headless daemon I/O (`LoopbackHandle` exposes `cancel_signal: Arc<Notify>` for session cancellation); `LoopbackEvent::ToolStart` / `LoopbackEvent::ToolOutput` carry per-tool UUIDs and `is_error` flag for ACP lifecycle notifications; `Attachment` / `AttachmentKind` for multimodal inputs |
| `config` | TOML config with `ZEPH_*` env overrides; typed `ConfigError` (Io, Parse, Validation, Vault) |
| `context` | LLM context assembly from history, skills, memory; resilient compaction with reactive context-overflow retry (max 2 attempts), middle-out progressive tool response removal (10/20/50/100% tiers), 9-section structured compaction prompt, LLM-free metadata fallback via `build_metadata_summary()` with safe UTF-8 truncation; parallel chunked summarization; tool-pair summarization via `maybe_summarize_tool_pair()` — when visible pairs exceed `tool_call_cutoff`, oldest pair is LLM-summarized with XML-delimited prompt and originals hidden via `agent_visible=false`; visibility-aware history loading (agent-only vs user-visible messages); durable compaction via `replace_conversation()`; active context compression via `CompressionStrategy` (reactive/proactive) compresses before capacity limits are hit; uses shared `Arc<TokenCounter>` for accurate tiktoken-based budget tracking |
| `cost` | Token cost tracking and budgeting |
| `daemon` | Background daemon mode with PID file lifecycle (optional feature) |
| `metrics` | Runtime metrics collection; `SecurityEvent` ring buffer (capped at 100) with `SecurityEventCategory` variants (`InjectionFlag`, `ExfiltrationBlock`, `Quarantine`, `Truncation`) for TUI security panel |
| `project` | Project-level context detection |
| `sanitizer` | `ContentSanitizer` — untrusted content isolation pipeline applied to all external data before it enters the LLM context; 4-step pipeline: truncate to `max_content_size`, strip null bytes and control characters, detect 17 injection patterns (OWASP cheat sheet + encoding variants), wrap in spotlighting XML delimiters (`<tool-output>` for local, `<external-data>` for external); `TrustLevel` enum (`Trusted`/`LocalUntrusted`/`ExternalUntrusted`), `ContentSourceKind` enum (with `FromStr`), `SanitizedContent` with `InjectionFlag` list; `ContentIsolationConfig` under `[security.content_isolation]`; optional `QuarantinedSummarizer` (Dual LLM pattern) routes high-risk sources through an isolated, tool-less LLM extraction call — re-sanitizes output via `detect_injections` + `escape_delimiter_tags` before spotlighting; `QuarantineConfig` under `[security.content_isolation.quarantine]`; `ExfiltrationGuard` — 3 outbound guards: markdown image pixel-tracking detection (inline + reference-style), tool URL cross-validation against flagged untrusted sources, memory write suppression for injection-flagged content; `ExfiltrationGuardConfig` under `[security.exfiltration_guard]`; metrics: `sanitizer_runs`, `sanitizer_injection_flags`, `sanitizer_truncations`, `quarantine_invocations`, `quarantine_failures`, `exfiltration_images_blocked`, `exfiltration_tool_urls_flagged`, `exfiltration_memory_guards` |
| `redact` | Regex-based secret redaction (AWS, OpenAI, Anthropic, Google, GitLab, HuggingFace, npm, Docker) |
| `vault` | Secret storage and resolution via vault providers (age-encrypted read/write); secrets stored as `BTreeMap` for deterministic JSON serialization on every `vault.save()` call; scans `ZEPH_SECRET_*` keys to build the custom-secrets map used by skill env injection; all secret values are held as `Zeroizing<String>` (zeroize-on-drop) and are not `Clone` |
| `instructions` | `load_instructions()` — auto-detects and loads provider-specific instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `zeph.md`) from the working directory; injects content into the volatile system prompt section with symlink boundary check, null byte guard, and 256 KiB per-file size cap. `InstructionWatcher` subscribes to filesystem events via `notify-debouncer-mini` (500 ms debounce) and reloads `instruction_blocks` in-place on any `.md` change — no agent restart required |
| `skill_loader` | `SkillLoaderExecutor` — `ToolExecutor` that exposes the `load_skill` tool to the LLM; accepts a skill name, looks it up in the shared `Arc<RwLock<SkillRegistry>>`, and returns the full SKILL.md body (truncated to `MAX_TOOL_OUTPUT_CHARS`); skill name is capped at 128 characters; unknown names return a human-readable error message rather than a hard error |
| `scheduler_executor` | `SchedulerExecutor` — `ToolExecutor` that exposes three LLM-callable tools: `schedule_periodic` (add a recurring cron task), `schedule_deferred` (add a one-shot task at a specific ISO 8601 UTC time), and `cancel_task` (remove a task by name); communicates with the scheduler via `mpsc::Sender<SchedulerMessage>` and validates input lengths and cron expressions before forwarding; only present when the `scheduler` feature is enabled |
| `hash` | `content_hash` — BLAKE3 hex digest utility |
| `pipeline` | Composable, type-safe step chains for multi-stage workflows |
| `subagent` | Sub-agent orchestration: `SubAgentManager` lifecycle with background execution, `SubAgentDef` YAML definitions with 4-level resolution priority (CLI > project > user > config) and scope labels, `PermissionGrants` zero-trust delegation, `FilteredToolExecutor` scoped tool access (with `tools.except` additional denylist), `PermissionMode` enum (`Default`, `AcceptEdits`, `DontAsk`, `BypassPermissions`, `Plan`), `max_turns` turn cap, A2A in-process channels, `SubAgentState` lifecycle enum (`Submitted`, `Working`, `Completed`, `Failed`, `Canceled`), real-time status tracking, persistent JSONL transcript storage with resume-by-ID (`TranscriptWriter`/`TranscriptReader`, `TranscriptMeta` sidecar, prefix-based ID lookup, automatic old transcript sweep); CRUD helpers: `serialize_to_markdown()` (round-trip Markdown serialization), `save_atomic()` (write-rename with parent-dir creation and name validation), `delete_file()`, `default_template()` (scaffold for new definitions); `AgentsCommand` enum drives the `zeph agents` CLI subcommands |
| `subagent::hooks` | Lifecycle hooks for sub-agents: `HookDef` (shell command with timeout and fail-open/closed policy), `HookMatcher` (pipe-separated tool-name patterns), `SubagentHooks` (per-agent `PreToolUse`/`PostToolUse` from YAML frontmatter); config-level `SubagentStart`/`SubagentStop` events; `fire_hooks()` executes sequentially with env-cleared sandbox and child kill on timeout |
| `subagent::memory` | Persistent memory scopes for sub-agents: `MemoryScope` enum (`User`, `Project`, `Local`), `resolve_memory_dir()` / `ensure_memory_dir()` for directory lifecycle, `load_memory_content()` reads MEMORY.md (first 200 lines, 256 KiB cap, symlink boundary check, null byte guard), `escape_memory_content()` prevents prompt injection via `<agent-memory>` tag escaping. Memory is auto-injected into the sub-agent system prompt and Read/Write/Edit tools are auto-enabled |

**Re-exports:** `Agent`, `content_hash`, `DiffData`

## Configuration

Key `AgentConfig` fields (TOML section `[agent]`):

| Field | Type | Default | Env override | Description |
|-------|------|---------|--------------|-------------|
| `name` | string | `"zeph"` | — | Agent display name |
| `max_tool_iterations` | usize | `10` | — | Max tool calls per turn |
| `summary_model` | string? | `null` | — | Model used for context summarization |
| `auto_update_check` | bool | `true` | `ZEPH_AUTO_UPDATE_CHECK` | Check GitHub releases for a newer version on startup / via scheduler |

Key `InstructionConfig` fields (TOML section `[agent.instructions]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `auto_detect` | bool | `true` | Auto-detect provider-specific files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`) |
| `extra_files` | `Vec<PathBuf>` | `[]` | Additional instruction files (absolute or relative to cwd) |
| `max_size_bytes` | u64 | `262144` | Per-file size cap (256 KiB); files exceeding this are skipped |

> [!NOTE]
> `zeph.md` and `.zeph/zeph.md` are always loaded regardless of `auto_detect`. Use `--instruction-file <path>` at the CLI to supply extra files at startup without modifying the config file.

> [!TIP]
> Instruction files support hot reload — edit any watched `.md` file while the agent is running and the updated content is applied within 500 ms on the next inference turn. The watcher starts automatically when at least one instruction path is resolved.

Key `DocumentConfig` fields (TOML section `[memory.documents]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `collection` | string | `"zeph_documents"` | Qdrant collection for document chunks |
| `chunk_size` | usize | `512` | Target tokens per chunk |
| `chunk_overlap` | usize | `64` | Overlap between chunks |
| `top_k` | usize | `3` | Max chunks injected per context-build turn |
| `rag_enabled` | bool | `false` | Enable automatic RAG context injection from `zeph_documents` |

Key `MemoryConfig` fields (TOML section `[memory]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `vector_backend` | `"qdrant"` / `"sqlite"` | `"qdrant"` | Vector search backend |
| `token_safety_margin` | f32 | `1.0` | Safety multiplier for tiktoken-based token budget (validated: must be >= 1.0) |
| `redact_credentials` | bool | `true` | Scrub secrets and paths before LLM context injection |
| `autosave_assistant` | bool | `false` | Persist assistant responses to semantic memory automatically |
| `autosave_min_length` | usize | `20` | Minimum response length (chars) to trigger autosave |
| `tool_call_cutoff` | usize | `6` | Max visible tool call/response pairs before oldest is summarized via LLM |
| `sqlite_pool_size` | u32 | `5` | SQLite connection pool size for memory storage |
| `response_cache_cleanup_interval_secs` | u64 | `3600` | Interval for expiring stale response cache entries |

```toml
[agent]
auto_update_check = true   # set to false to disable update notifications
```

Set `ZEPH_AUTO_UPDATE_CHECK=false` to disable without changing the config file.

## Skill commands

| Command | Description |
|---------|-------------|
| `/skill list` | List loaded skills with trust level and match count |
| `/skill install <url>` | Install a skill from a remote URL |
| `/skill remove <name>` | Remove an installed skill |
| `/skill reject <name> <reason>` | Record a typed rejection and trigger immediate skill improvement |

> [!TIP]
> `/skill reject` provides the strongest feedback signal. The rejection is persisted with a `FailureKind` discriminant to the `outcome_detail` column and immediately updates the Wilson score posterior for Bayesian re-ranking.

## Self-learning configuration

Key `AgentConfig.learning` fields (TOML section `[agent.learning]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `correction_detection` | bool | `true` | Enable `FeedbackDetector` implicit correction capture |
| `correction_confidence_threshold` | f64 | `0.7` | Minimum detector confidence to persist a `UserCorrection` |
| `correction_recall_limit` | usize | `5` | Max corrections retrieved per context-build turn |
| `correction_min_similarity` | f64 | `0.75` | Minimum embedding similarity for correction recall |
| `detector_mode` | `"regex"` / `"judge"` | `"regex"` | Detection strategy: regex-only or LLM-backed judge with adaptive regex fallback |
| `judge_model` | string | `""` | Model for the judge detector (e.g. `"claude-sonnet-4-6"`); empty = use primary provider |
| `judge_adaptive_low` | f32 | `0.5` | Regex confidence below this value skips judge invocation (treated as "not a correction") |
| `judge_adaptive_high` | f32 | `0.8` | Regex confidence above this value skips judge invocation (high-confidence regex match accepted) |

Key `LlmConfig` fields for EMA routing (TOML section `[llm]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `router_ema_enabled` | bool | `false` | Enable per-provider EMA latency tracking and reordering |
| `router_ema_alpha` | f64 | `0.1` | EMA smoothing factor (lower = slower adaptation) |
| `router_reorder_interval` | u64 | `60` | Seconds between provider list reordering |

## Sub-agent Commands

In-session commands for managing sub-agents:

| Command | Description |
|---------|-------------|
| `/agent list` | List available sub-agent definitions |
| `/agent spawn <name> <prompt>` | Spawn a sub-agent with a task prompt |
| `/agent bg <name> <prompt>` | Spawn a background sub-agent |
| `/agent status` | Show active sub-agents with state, turns, and elapsed time |
| `/agent cancel <id>` | Cancel a running sub-agent by ID prefix |
| `/agent resume <id> <prompt>` | Resume a completed sub-agent session with a new prompt (restores JSONL transcript history) |
| `/agent approve <id>` | Approve a pending secret request |
| `/agent deny <id>` | Deny a pending secret request |
| `@agent_name <prompt>` | Mention shorthand for `/agent spawn` (disambiguated from file references) |

Sub-agents run as independent tokio tasks with their own LLM provider and filtered tool executor. Each sub-agent receives only explicitly granted tools, skills, and secrets via `PermissionGrants`. Conversation history is persisted as JSONL transcripts with `.meta.json` sidecars, enabling session resumption via `/agent resume <id> <prompt>` — the resumed agent inherits the original definition, tools, and full message history.

Lifecycle hooks can be attached at two levels: config-level `SubagentStart`/`SubagentStop` hooks (in `[agents.hooks]`) fire on spawn and completion, while per-agent `PreToolUse`/`PostToolUse` hooks (defined in the agent YAML frontmatter) fire around each tool call, matched by pipe-separated tool-name patterns. All hooks run as shell commands in an env-cleared sandbox with configurable timeout and fail-open/closed policy.

## Agents management CLI

`zeph agents` provides CRUD management of sub-agent definition files outside of a running session:

| Command | Description |
|---------|-------------|
| `zeph agents list` | Print all discovered definitions with name, scope, description, and model |
| `zeph agents show <name>` | Print full detail of a single definition |
| `zeph agents create <name> --description <desc> [--dir <path>] [--model <id>]` | Scaffold a new `.md` definition via `default_template` + `save_atomic` |
| `zeph agents edit <name>` | Open the definition file in `$VISUAL` / `$EDITOR` (validates parse on exit) |
| `zeph agents delete <name> [--yes]` | Delete a definition file with interactive confirmation |

> [!TIP]
> The same CRUD operations are available interactively in the TUI agents panel — press `a` in the TUI to open the panel, then `c` (create), `e` (edit), `d` (delete), Enter (detail view).

## Installation

```bash
cargo add zeph-core
```

## License

MIT
