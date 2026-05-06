# zeph-core

[![Crates.io](https://img.shields.io/crates/v/zeph-core)](https://crates.io/crates/zeph-core)
[![docs.rs](https://img.shields.io/docsrs/zeph-core)](https://docs.rs/zeph-core)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.95-blue)](https://www.rust-lang.org)

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
| `agent::feedback_detector` | `FeedbackDetector` (regex) and `JudgeDetector` (LLM-backed) — implicit correction detection from user messages with multi-language support (7 languages: English, Russian, Spanish, German, French, Portuguese, Chinese); `JudgeDetector` runs in background via `tokio::spawn` with sliding-window rate limiter (5 calls / 60 s) and XML-escaped adversarial-defense prompt; adaptive threshold gates judge invocation to the regex uncertainty zone |
| `agent::persistence` | Message persistence and background graph extraction integration; `maybe_spawn_graph_extraction` fires a `SemanticMemory::spawn_graph_extraction` task per user turn with injection-flag guard and last-4-user-messages context window |
| `agent::tool_execution` | Tool call handling, redaction, result processing; both the fenced-block path (`handle_tool_result`) and the structured tool-call path unconditionally emit `LoopbackEvent::ToolStart` (UUID generated per call) before execution and `LoopbackEvent::ToolOutput` (matching UUID, `is_error` flag) after; `call_llm_with_retry()` / `call_chat_with_tools_retry()` — auto-detect `ContextLengthExceeded`, compact context, and retry (max 2 attempts); `prune_stale_tool_outputs` invokes `count_tokens` once per `ToolResult` part |
| `agent::message_queue` | Message queue management |
| `agent::builder` | Agent builder API |
| `agent::commands` | Chat command dispatch (skills, feedback, skill management via `/skill install`, `/skill remove`, `/skill reject <name> <reason>`, sub-agent management via `/agent`, etc.) |
| `agent::utils` | Shared agent utilities |
| `bootstrap` | `AppBuilder` — fluent builder for application startup; split into submodules: `config` (config resolution, vault arg parsing), `health` (health check, provider warmup), `mcp` (MCP manager and registry), `provider` (provider factory functions), `skills` (skill matcher, embedding model helpers) |
| `channel` | `Channel` trait defining I/O adapters; `LoopbackChannel` / `LoopbackHandle` for headless daemon I/O (`LoopbackHandle` exposes `cancel_signal: Arc<Notify>` for session cancellation); `LoopbackEvent::ToolStart` / `LoopbackEvent::ToolOutput` carry per-tool UUIDs and `is_error` flag for ACP lifecycle notifications; `Attachment` / `AttachmentKind` for multimodal inputs |
| `config` | TOML config with `ZEPH_*` env overrides; typed `ConfigError` (Io, Parse, Validation, Vault) |
| `config::migrate` | `ConfigMigrator` — lossless TOML migration using `toml_edit`; compares user config against the embedded canonical `default.toml`, appends missing sections as commented-out blocks with documentation, reorders top-level sections by canonical group order, and deduplicates on re-run (idempotent). `MigrationResult` carries `output`, `changed_count`, and `sections_changed`. Exposed via `zeph migrate-config [--in-place] [--diff]`. |
| `context` | LLM context assembly from history, skills, memory; three-tier compaction pipeline: (1) deferred summary application at `deferred_apply_threshold` (default 70%) — applies pre-computed tool-pair summaries lazily to stabilize the Claude API prompt cache prefix; (2) stale tool output pruning at `compaction_threshold` (default 80%); (3) LLM middle-out compaction on overflow with reactive retry (max 2 attempts), 10/20/50/100% progressive removal tiers, 9-section structured compaction prompt, and LLM-free metadata fallback via `build_metadata_summary()` with safe UTF-8 truncation; parallel chunked summarization; tool-pair summarization via `maybe_summarize_tool_pair()` — when visible pairs exceed `tool_call_cutoff`, oldest pair is LLM-summarized with XML-delimited prompt and originals hidden via `agent_visible=false`; visibility-aware history loading (agent-only vs user-visible messages); durable compaction via `replace_conversation()`; active context compression via `CompressionStrategy` (reactive/proactive) compresses before capacity limits are hit; uses shared `Arc<TokenCounter>` for accurate tiktoken-based budget tracking; `BudgetAllocation.graph_facts` reserves tokens for graph-aware retrieval (4% of remaining budget when graph memory is enabled, 0 otherwise); `ContextSlot::GraphFacts` concurrent fetch slot; `fetch_graph_facts` calls `graph_recall` in parallel with other memory fetchers and injects the resulting facts as a system message; task-aware pruning via `CompactionState` enum for type-safe compaction lifecycle |
| `agent::compaction_strategy` | HiAgent subgoal-aware compaction: `SubgoalRegistry` tracks active and completed subgoals with message spans; `score_blocks_subgoal()` scores tool-output blocks by subgoal tier membership (active=1.0, completed=0.3, untagged=0.1); `score_blocks_subgoal_mig()` combines subgoal relevance with pairwise MIG redundancy scoring; active subgoal messages are protected from eviction |
| `cost` | Token cost tracking and budgeting |
| `daemon` | Background daemon mode with PID file lifecycle (optional feature) |
| `metrics` | Runtime metrics collection; `SecurityEvent` ring buffer (capped at 100) with `SecurityEventCategory` variants (`InjectionFlag`, `ExfiltrationBlock`, `Quarantine`, `Truncation`) for TUI security panel |
| `project` | Project-level context detection |
| `sanitizer` | `ContentSanitizer` — untrusted content isolation pipeline applied to all external data before it enters the LLM context; 4-step pipeline: truncate to `max_content_size`, strip null bytes and control characters, detect 17 injection patterns (OWASP cheat sheet + encoding variants), wrap in spotlighting XML delimiters (`<tool-output>` for local, `<external-data>` for external); `TrustLevel` enum (`Trusted`/`LocalUntrusted`/`ExternalUntrusted`), `ContentSourceKind` enum (with `FromStr`), `SanitizedContent` with `InjectionFlag` list; `ContentIsolationConfig` under `[security.content_isolation]`; optional `QuarantinedSummarizer` (Dual LLM pattern) routes high-risk sources through an isolated, tool-less LLM extraction call — re-sanitizes output via `detect_injections` + `escape_delimiter_tags` before spotlighting; `QuarantineConfig` under `[security.content_isolation.quarantine]`; `ExfiltrationGuard` — 3 outbound guards: markdown image pixel-tracking detection (inline + reference-style), tool URL cross-validation against flagged untrusted sources, memory write suppression for injection-flagged content; `ExfiltrationGuardConfig` under `[security.exfiltration_guard]`; metrics: `sanitizer_runs`, `sanitizer_injection_flags`, `sanitizer_truncations`, `quarantine_invocations`, `quarantine_failures`, `exfiltration_images_blocked`, `exfiltration_tool_urls_flagged`, `exfiltration_memory_guards` |
| `redact` | Regex-based secret redaction (AWS, OpenAI, Anthropic, Google, GitLab, HuggingFace, npm, Docker) |
| `vault` | Secret storage and resolution via vault providers (age-encrypted read/write); secrets stored as `BTreeMap` for deterministic JSON serialization on every `vault.save()` call; scans `ZEPH_SECRET_*` keys to build the custom-secrets map used by skill env injection; all secret values are held as `Zeroizing<String>` (zeroize-on-drop) and are not `Clone` |
| `instructions` | `load_instructions()` — auto-detects and loads provider-specific instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `zeph.md`) from the working directory; injects content into the volatile system prompt section with symlink boundary check, null byte guard, and 256 KiB per-file size cap. `InstructionWatcher` subscribes to filesystem events via `notify-debouncer-mini` (500 ms debounce) and reloads `instruction_blocks` in-place on any `.md` change — no agent restart required |
| `skill_loader` | `SkillLoaderExecutor` — `ToolExecutor` that exposes the `load_skill` tool to the LLM; accepts a skill name, looks it up in the shared `Arc<RwLock<SkillRegistry>>`, and returns the full SKILL.md body (truncated to `MAX_TOOL_OUTPUT_CHARS`); skill name is capped at 128 characters; unknown names return a human-readable error message rather than a hard error |
| `skill_invoke` | `SkillInvokeExecutor` — `ToolExecutor` that exposes the `invoke_skill` tool with trust-aware sanitization; Blocked skills are refused; non-Trusted bodies pass through `sanitize_skill_text`; Quarantined bodies are additionally wrapped with `wrap_quarantined`; exempt from adversarial policy, VIGIL gate, and tool-schema filter |
| `vigil` | `VigilGate` — regex tripwire that runs before `ContentSanitizer` on every tool output; configurable via `[security.vigil]`; `VigilRiskLevel` recorded in audit entries; `vigil_flags_total` / `vigil_blocks_total` counters in `MetricsSnapshot`; fail-open on invalid config; subagent sessions exempt |
| `scheduler_executor` | `SchedulerExecutor` — `ToolExecutor` that exposes three LLM-callable tools: `schedule_periodic` (add a recurring cron task), `schedule_deferred` (add a one-shot task at a specific ISO 8601 UTC time), and `cancel_task` (remove a task by name); communicates with the scheduler via `mpsc::Sender<SchedulerMessage>` and validates input lengths and cron expressions before forwarding; only present when the `scheduler` feature is enabled |
| `debug_dump` | `DebugDumper` — writes numbered `{id:04}-request.json`, `{id:04}-response.txt`, and `{id:04}-tool-{name}.txt` files to a timestamped session directory; request dumps include model, token limit, tools, temperature, cache metadata, and message payloads in both `json` and `raw` formats; enabled via `--debug-dump [PATH]` CLI flag, `[debug] enabled = true` config, or `/debug-dump [path]` slash command; hooks into both streaming and non-streaming LLM paths and before `maybe_summarize_tool_output` |
| `agent::log_commands` | `/log` slash command handler — displays current `LoggingConfig` (file path, level, rotation, max files) and tails the last 20 lines from the active log file |
| `hash` | `content_hash` — BLAKE3 hex digest utility |
| `pipeline` | Composable, type-safe step chains for multi-stage workflows |
| `subagent` | Sub-agent orchestration: `SubAgentManager` lifecycle with background execution, `SubAgentDef` YAML definitions with 4-level resolution priority (CLI > project > user > config) and scope labels, `PermissionGrants` zero-trust delegation, `FilteredToolExecutor` scoped tool access (with `tools.except` additional denylist), `PermissionMode` enum (`Default`, `AcceptEdits`, `DontAsk`, `BypassPermissions`, `Plan`), `max_turns` turn cap, A2A in-process channels, `SubAgentState` lifecycle enum (`Submitted`, `Working`, `Completed`, `Failed`, `Canceled`), real-time status tracking, persistent JSONL transcript storage with resume-by-ID (`TranscriptWriter`/`TranscriptReader`, `TranscriptMeta` sidecar, prefix-based ID lookup, automatic old transcript sweep); CRUD helpers: `serialize_to_markdown()` (round-trip Markdown serialization), `save_atomic()` (write-rename with parent-dir creation and name validation), `delete_file()`, `default_template()` (scaffold for new definitions); `AgentsCommand` enum drives the `zeph agents` CLI subcommands |
| `subagent::hooks` | Lifecycle hooks for sub-agents: `HookDef` (shell command with timeout and fail-open/closed policy), `HookMatcher` (pipe-separated tool-name patterns), `SubagentHooks` (per-agent `PreToolUse`/`PostToolUse` from YAML frontmatter); config-level `SubagentStart`/`SubagentStop` events; `fire_hooks()` executes sequentially with env-cleared sandbox and child kill on timeout |
| `subagent::memory` | Persistent memory scopes for sub-agents: `MemoryScope` enum (`User`, `Project`, `Local`), `resolve_memory_dir()` / `ensure_memory_dir()` for directory lifecycle, `load_memory_content()` reads MEMORY.md (first 200 lines, 256 KiB cap, symlink boundary check, null byte guard), `escape_memory_content()` prevents prompt injection via `<agent-memory>` tag escaping. Memory is auto-injected into the sub-agent system prompt and Read/Write/Edit tools are auto-enabled |
| `experiments` | Autonomous self-experimentation engine (feature-gated: `experiments`): `Variation` config mutations (temperature, top-p, top-k, frequency/presence penalty, system prompt), `ExperimentResult` with LLM-as-judge scoring, `ExperimentStatus` lifecycle; `ExperimentConfig` under `[experiments]` with `max_experiments`, `max_wall_time_secs`, `eval_budget_tokens`, `min_improvement`, optional `eval_model` and `benchmark_file`; `ExperimentSchedule` for cron-based periodic runs (`cron`, `max_experiments_per_run`, `max_wall_time_secs`); the scheduler registers a `TaskKind::Experiment` handler when both `scheduler` and `experiments` features are active; `BenchmarkSet` / `BenchmarkCase` loaded from TOML files via `from_file()` with path traversal protection and file size limit; `Evaluator` with parallel judge scoring via `FuturesUnordered`, per-invocation token budget enforcement via `AtomicU64`, XML boundary tags for prompt injection defense; `EvalReport` with mean score, p50/p95 latency, partial-run detection, error count; **Parameter variation engine**: `SearchSpace` with `ParameterRange` (min/max/step/default per parameter kind), `ConfigSnapshot` for baseline capture and rollback, `VariationGenerator` trait with three strategies — `GridStep` (exhaustive sweep), `Random` (uniform sampling), `Neighborhood` (local search around current best); one-at-a-time constraint isolates each parameter change, `OrderedFloat`-based `HashSet<Variation>` deduplication prevents retesting; **`experiment_cmd`** sub-module dispatches `/experiment` slash commands (`start`, `stop`, `status`, `report`, `best`) with `CancellationToken`-based concurrent session guard |
| `orchestration` | DAG-based task orchestration: `TaskGraph` with `TaskNode` dependency tracking, `GraphId`/`TaskId` typed identifiers, `FailureStrategy` (abort/retry/skip/ask), `GraphStatus`/`TaskStatus` lifecycle enums, `GraphPersistence<S>` typed wrapper over `RawGraphStore`, DAG validation (cycle detection, structural invariants via topological sort), `OrchestrationConfig` under `[orchestration]`; `Planner` trait for goal decomposition with `LlmPlanner<P>` implementation — uses `chat_typed` for structured JSON output, maps string task IDs to `TaskId`, validates agent hints against available `SubAgentDef` set; tick-based `DagScheduler` execution engine with command pattern (`SchedulerAction`), `AgentRouter` trait + `RuleBasedRouter` for task-to-agent routing, `spawn_for_task()` on `SubAgentManager` for orchestrated task spawning, cross-task context injection with `ContentSanitizer` integration, stale event guard preventing timed-out agent completions from corrupting retry state; `Aggregator` trait + `LlmAggregator<P>` — synthesizes completed task outputs into a coherent response via a single LLM call; per-task character budget derived from `aggregator_max_tokens` (default 4096), task results spotlighted via `ContentSanitizer` before inclusion, raw-concatenation fallback on LLM failure; `PlanCommand` enum with `/plan` CLI commands (goal, status, list, cancel, confirm, resume, retry) integrated into the agent loop; `OrchestrationMetrics` (plans_total, tasks_total/completed/failed/skipped) always present in `MetricsSnapshot`; pending-plan confirmation flow with `confirm_before_execute` config |
| `hooks` | `[hooks]` config with `[[hooks.cwd_changed]]` and `[[hooks.file_changed]]` event hooks; `set_working_directory` tool allows the LLM to change the agent's working directory, emitting a `CwdChanged` event; `FileChangeWatcher` via `notify-debouncer-mini` emits `FileChanged` events for watched paths; hook shell commands receive `ZEPH_OLD_CWD` / `ZEPH_NEW_CWD` (cwd hooks) and `ZEPH_CHANGED_PATH` (file hooks) environment variables |
| `lsp_hooks` | LSP context injection hooks (feature-gated: `lsp-context`): `LspHookRunner` integrates with the agent tool loop to automatically inject LSP-derived context before each LLM call; `LspNote` type carries formatted content with estimated token counts; `DiagnosticsOnSave` hook fetches compiler diagnostics from mcpls after `write_file` completes; `HoverOnRead` hook pre-fetches hover info for key symbols (function/struct/enum/trait definitions) after `read_file` completes using concurrent `join_all` MCP calls; `ReferencesOnRename` hook fetches all reference sites before `rename_symbol` executes so the model sees the full impact; notes are injected as `Role::User` messages with `[lsp ...]` prefix, following the established pattern of `[semantic recall]`, `[known facts]`, and `[code context]`; per-turn token budget enforced in `drain_notes()` — notes exceeding the budget are dropped with a debug log; graceful degradation when mcpls is unavailable: `is_available()` checks the `McpManager` client list, individual MCP call failures are swallowed at `debug` level, and the agent loop continues normally |

**Re-exports:** `Agent`, `content_hash`, `DiffData`

## Configuration

Key `AgentConfig` fields (TOML section `[agent]`):

| Field | Type | Default | Env override | Description |
|-------|------|---------|--------------|-------------|
| `name` | string | `"zeph"` | — | Agent display name |
| `max_tool_iterations` | usize | `10` | — | Max tool calls per turn |
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

Key `LspConfig` fields (TOML section `[agent.lsp]`, requires `lsp-context` feature):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable LSP context injection hooks |
| `mcp_server_id` | string | `"mcpls"` | MCP server ID to use for LSP calls |
| `token_budget` | usize | `2000` | Maximum tokens spent on LSP-injected context per turn |
| `diagnostics.enabled` | bool | `true` | Fetch compiler diagnostics after `write_file` |
| `diagnostics.max_per_file` | usize | `20` | Maximum diagnostics per file |
| `diagnostics.max_files` | usize | `5` | Maximum files per diagnostic batch |
| `diagnostics.min_severity` | string | `"error"` | Minimum severity to include: `"error"`, `"warning"`, `"info"`, `"hint"` |
| `hover.enabled` | bool | `false` | Pre-fetch hover info for key symbols after `read_file` |
| `hover.max_symbols` | usize | `5` | Maximum hover entries per file |
| `references.enabled` | bool | `true` | Fetch reference sites before `rename_symbol` |
| `references.max_refs` | usize | `50` | Maximum references to show per symbol |

```toml
[agent.lsp]
enabled = true
mcp_server_id = "mcpls"
token_budget = 2000

[agent.lsp.diagnostics]
enabled = true
max_per_file = 20
max_files = 5
min_severity = "error"

[agent.lsp.hover]
enabled = false
max_symbols = 5

[agent.lsp.references]
enabled = true
max_refs = 50
```

> [!NOTE]
> LSP context injection requires the [mcpls](https://github.com/bug-ops/mcpls) MCP server to be configured. If mcpls is unavailable, hooks degrade silently — the agent continues normally with no LSP context injected. Enable via `--lsp-context` CLI flag or `zeph init` wizard.

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
| `deferred_apply_threshold` | f32 | `0.70` | Context usage ratio at which deferred tool-pair summaries are applied (must be < `compaction_threshold`) |
| `sqlite_pool_size` | u32 | `5` | SQLite connection pool size for memory storage |
| `response_cache_cleanup_interval_secs` | u64 | `3600` | Interval for expiring stale response cache entries |
| `embed_concurrency` | u32 | `4` | Max concurrent embedding requests (0 = unlimited); shared across indexer, backfill, and graph extraction |

Key `CompressionConfig` fields (TOML section `[memory.compression]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `archive_tool_outputs` | bool | `false` | Archive tool output bodies to SQLite (Memex) before compaction; UUID back-references are injected into summaries |

Key `CompressionGuidelinesConfig` fields (TOML section `[memory.compression_guidelines]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `categorized_guidelines` | bool | `false` | Tag ACON failure pairs by category (tool_output / assistant_reasoning / user_context) and maintain per-category guideline blocks |

> [!NOTE]
> `archive_tool_outputs` requires the `compression-guidelines` feature flag. `categorized_guidelines` is also gated behind `compression-guidelines`. Both are disabled by default and must be explicitly opted in.

```toml
[agent]
auto_update_check = true   # set to false to disable update notifications
```

Set `ZEPH_AUTO_UPDATE_CHECK=false` to disable update notifications without changing the config file.

Key `SessionConfig.recap` fields (TOML section `[session.recap]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Show a recap of the previous session on resume when a digest is available |
| `provider` | string | `""` | Provider name (references `[[llm.providers]]`) for recap generation; empty = primary provider |

```toml
[session.recap]
enabled  = true
provider = "fast"   # optional; references [[llm.providers]] name
```

> [!TIP]
> Use `/recap` to request a session recap on demand at any time during a session, regardless of the `enabled` setting.

Key `DebugConfig` fields (TOML section `[debug]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable debug dump at startup — writes all LLM requests, responses, and raw tool output to files |
| `output_dir` | `PathBuf` | user data dir (`.../debug`) | Base directory; each session creates a `{unix_timestamp}/` subdirectory |

> [!TIP]
> Use `--debug-dump` without a path to use `output_dir` from config. Use `--debug-dump /tmp/mydir` to override for one session. The `/debug-dump [path]` slash command enables it mid-session without restarting.

Key `LoggingConfig` fields (TOML section `[logging]`):

| Field | Type | Default | Env override | Description |
|-------|------|---------|--------------|-------------|
| `file` | string | user data dir (`.../logs/zeph.log`) | `ZEPH_LOG_FILE` | Path to the log file. Empty string disables file logging |
| `level` | string | `"info"` | `ZEPH_LOG_LEVEL` | Log level for the file sink (does not affect stderr / `RUST_LOG`) |
| `rotation` | `"daily"` / `"hourly"` / `"never"` | `"daily"` | — | Log file rotation strategy |
| `max_files` | usize | `7` | — | Maximum number of rotated log files to retain |

```toml
[logging]
# Omit to use the default user-data log path.
# file = "/absolute/path/to/zeph.log"
level = "info"
rotation = "daily"
max_files = 7
```

> [!NOTE]
> Use `--log-file <PATH>` at the CLI to override the log file path for one session. The file-level filter is independent of `RUST_LOG` — stderr output and file output can use different levels simultaneously. The `/log` slash command shows the active config and tails recent entries.

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

Key `LlmConfig` fields (TOML section `[llm]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `summary_model` | string? | `null` | Shorthand spec for the summarization provider. Formats: `ollama/<model>`, `claude[/<model>]`, `openai[/<model>]`, `compatible/<name>`, `candle`. Ignored when `[llm.summary_provider]` is set. |
| `summary_provider` | table? | `null` | Structured summarization provider (takes precedence over `summary_model`). Same fields as `[llm.orchestrator.providers.*]`: `type`, `model`, `base_url`, `embedding_model`, `device`. For `compatible` type, `model` is the `[[llm.compatible]]` entry name. |
| `router_ema_enabled` | bool | `false` | Enable per-provider EMA latency tracking and reordering |
| `router_ema_alpha` | f64 | `0.1` | EMA smoothing factor (lower = slower adaptation) |
| `router_reorder_interval` | u64 | `60` | Seconds between provider list reordering |

```toml
# Example: use Claude Haiku for summarization, primary model for inference
[llm.summary_provider]
type = "claude"
model = "claude-haiku-4-5-20251001"
```

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

## Plan Commands

In-session commands for task orchestration (requires `orchestration` feature):

| Command | Description |
|---------|-------------|
| `/plan <goal>` | Decompose goal into a DAG, show confirmation, then execute |
| `/plan confirm` | Confirm and execute the pending plan |
| `/plan status` | Show current graph progress |
| `/plan status <id>` | Show a specific graph by UUID |
| `/plan list` | List recent graphs from persistence |
| `/plan cancel` | Cancel the active graph |
| `/plan cancel <id>` | Cancel a specific graph by UUID |
| `/plan resume` | Resume the active paused graph (Ask failure strategy) |
| `/plan resume <id>` | Resume a specific paused graph by UUID |
| `/plan retry` | Re-run all failed tasks in the active graph |
| `/plan retry <id>` | Re-run failed tasks in a specific graph by UUID |

> [!NOTE]
> When `confirm_before_execute` is enabled (default), `/plan <goal>` stores the plan in a pending state. Run `/plan confirm` to start execution or `/plan cancel` to discard.

> [!NOTE]
> `/plan resume` applies when a graph is paused by the `Ask` failure strategy — the agent waits for user direction before continuing. `/plan retry` re-queues all `Failed` tasks in the graph for re-execution.

Key `OrchestrationConfig` fields (TOML section `[orchestration]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `planner_max_tokens` | u32 | `4096` | Token budget for the LLM goal-decomposition call |
| `dependency_context_budget` | usize | `16384` | Character budget injected as cross-task context |
| `confirm_before_execute` | bool | `true` | Require `/plan confirm` before executing a new plan |
| `aggregator_max_tokens` | u32 | `4096` | Token budget for the `LlmAggregator` synthesis call; divided equally across completed tasks |

## Experiment Commands

In-session commands for autonomous self-experimentation (requires `experiments` feature):

| Command | Description |
|---------|-------------|
| `/experiment start [N]` | Start an experiment session (optional N = max experiments) |
| `/experiment stop` | Stop the running experiment session |
| `/experiment status` | Show current experiment session status |
| `/experiment report` | Print experiment results summary |
| `/experiment best` | Show the best experiment result |

> [!NOTE]
> Only one experiment session can run at a time. Starting a new session while one is active returns an error. Use `/experiment stop` to cancel the current session first.

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

## Speculative tool dispatch

`SpeculationEngine` pre-runs read-only tool calls while the LLM generates its response. Two activation paths are supported:

- **SSE decoding path** — `claude_sse_to_tool_stream` emits `ToolBlockStart` at `content_block_start`; when confidence exceeds `confidence_threshold`, `try_dispatch(Trusted)` fires with a 2 s timeout.
- **PASTE pattern path** — `run_paste_skill_activation` calls `PatternStore::predict` per active skill and dispatches candidates above threshold with per-skill trust; `observe_paste_transition` records transitions for future pattern learning.

`requires_confirmation` defaults to `true` for all executors, making speculative dispatch safe-by-default. Only executors that explicitly opt out can be speculatively dispatched.

Configure via `[tools.speculative]` in `config.toml`:

```toml
[tools.speculative]
mode               = "decoding"   # "off" | "decoding" | "pattern"
confidence_threshold = 0.8
timeout_ms         = 2000
```

> [!NOTE]
> The speculation engine only runs when the agent is not in `--bare` mode. Committed speculative results that carry `ToolError::ConfirmationRequired` trigger a `tracing::error!` in debug builds, making the invariant machine-checkable at zero release cost.

## Goal lifecycle and TACO output compression

`GoalLifecycle` tracks active goals across turns. Tool outputs for completed or stale goals are compressed by the TACO (Tool-Aware Compaction Optimization) pipeline, which archives bodies to SQLite before the LLM compaction call and injects UUID back-references into the resulting summary.

## Reactive hooks

`[hooks]` in `config.toml` defines shell commands that fire on working-directory or file-change events. Hooks are now traced with `tracing` instrumentation and are propagated correctly through `reload_config` — hooks registered after a live config reload fire identically to those present at startup.

```toml
[[hooks.cwd_changed]]
command = "echo changed from $ZEPH_OLD_CWD to $ZEPH_NEW_CWD"
timeout_secs = 5

[[hooks.file_changed]]
command = "cargo check"
timeout_secs = 30
```

The `set_working_directory` tool is exposed to the LLM and updates the agent's cwd at runtime, triggering any registered `cwd_changed` hooks. `FileChangeWatcher` monitors paths declared in `[hooks.file_changed]` entries (500 ms debounce) and triggers `file_changed` hooks on modification. Hook commands run in an env-cleared sandbox and receive:

| Variable | Scope | Description |
|---|---|---|
| `ZEPH_OLD_CWD` | `cwd_changed` | Previous working directory |
| `ZEPH_NEW_CWD` | `cwd_changed` | New working directory |
| `ZEPH_CHANGED_PATH` | `file_changed` | Absolute path of the changed file |

## Features

| Feature | Description |
|---------|-------------|
| `candle` | Local inference via Candle (enables `zeph-llm/candle`) |
| `cuda` | CUDA backend for Candle (implies `candle`) |
| `metal` | Metal backend for Candle on Apple Silicon (implies `candle`) |
| `guardrail` | Advanced content guardrails via `zeph-sanitizer` |
| `lsp-context` | LSP context injection hooks via `LspHookRunner` |
| `compression-guidelines` | LLM-guided compaction guidelines in context assembly; enables `archive_tool_outputs` (Memex) and `categorized_guidelines` (ACON per-category) config options |
| `experiments` | Autonomous self-experimentation engine |
| `policy-enforcer` | Policy enforcement for tool execution |
| `scheduler` | Integration with `zeph-scheduler` for cron-based tasks |
| `context-compression` | Proactive context compression strategy |
| `mock` | `MockVaultProvider` for tests |

## Installation

```bash
cargo add zeph-core
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
