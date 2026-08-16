# zeph-core

[![Crates.io](https://img.shields.io/crates/v/zeph-core)](https://crates.io/crates/zeph-core)
[![docs.rs](https://img.shields.io/docsrs/zeph-core)](https://docs.rs/zeph-core)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

Core agent loop, configuration, context builder, metrics, vault, and sub-agent orchestration for Zeph.

## Overview

Core orchestration crate for the Zeph agent. It owns the main agent turn loop, resolves TOML configuration
with `ZEPH_*` environment overrides and vault-backed secrets, and drives context assembly, tool dispatch,
skill trust gating, and metrics. Most subsystems — sub-agents, orchestration, sanitization, memory,
persistence, experiments — are implemented in dedicated sibling crates and *wired together* here; see
[Subsystems that live in sibling crates](#subsystems-that-live-in-sibling-crates) for the map.

## Key modules

| Module | Description |
|--------|-------------|
| `agent` | `Agent<C>` — main loop driving inference and tool execution; `ToolExecutor` erased via `Box<dyn ErasedToolExecutor>`; supports external cancellation via `AgentBuilder::with_cancel_signal()`. Internal submodules (`context_manager`, `tool_orchestrator`, `learning_engine`, `tool_execution`, `persistence`, `message_queue`, `vigil`, …) are crate-private; the public surface is `agent::error`, `agent::session_config`, `agent::shadow_sentinel`, `agent::slash_commands`, `agent::speculative`, `agent::trajectory`, and `agent::turn` |
| `anchor_store` | `AgeVaultAnchorStore` — concrete `AnchorStore` implementation backing vault-anchor downgrade resistance for transcripts and sessions (`[integrity]`); `run_anchor_sweep` bounds vault growth to `O(max_session_anchors + max_transcript_files)`, evicting oldest anchors by their embedded `written_at`, never filesystem mtime |
| `channel` | `Channel` trait defining I/O adapters; `LoopbackChannel` / `LoopbackHandle` for headless daemon I/O; `LoopbackEvent` carries streaming chunks, status, tool lifecycle (`ToolStart` / `ToolOutput` with per-tool UUIDs and an `is_error` flag), usage, plan state, and `Stop(StopHint)`; `SkillCatalogItem` + `Channel::send_skill_catalog` push the trust-annotated skill catalog to the UI (spec 084); `Attachment` / `AttachmentKind` for multimodal inputs |
| `config` | Re-exports the `zeph-config` data types plus the `SecretResolver` extension trait (vault resolution lives here because `VaultProvider` does). TOML config with `ZEPH_*` env overrides; typed `ConfigError` (Io, Parse, Validation, Vault) |
| `config::migrate` | Re-export of `zeph_config::migrate` — `ConfigMigrator` performs lossless TOML migration using `toml_edit`: compares user config against the embedded canonical `default.toml`, appends missing sections as commented-out documented blocks, reorders top-level sections by canonical group order, and deduplicates on re-run (idempotent). `MigrationResult` carries `output`, `changed_count`, and `sections_changed`. Exposed via `zeph migrate-config [--in-place] [--diff]` |
| `config_watcher` | Filesystem watcher (`notify-debouncer-mini`) that reloads `config.toml` in place without an agent restart |
| `context` | Agent-side context assembly glue over `zeph-context`: re-exports `ContextBudget` / `BudgetAllocation` and layers instruction blocks on top. The stateless budget, assembler, and typed-page machinery live in `zeph-context`; the assembly service itself lives in `zeph-agent-context` |
| `cost` | Token cost tracking and budgeting |
| `daemon` | Background daemon mode with PID file lifecycle |
| `debug_dump` | `DebugDumper` — writes numbered `{id:04}-request.json`, `{id:04}-response.txt`, and `{id:04}-tool-{name}.txt` files to a timestamped session directory; request dumps include model, token limit, tools, temperature, cache metadata, and message payloads. Enabled via `--debug-dump [PATH]` CLI flag, `[debug] enabled = true`, or the `/debug-dump [path]` slash command; hooks into both streaming and non-streaming LLM paths |
| `durable` | Concrete cryptographic backing (key material resolution) for the `zeph-durable` execution layer |
| `file_watcher` | `FileChangeWatcher` — debounced (`notify-debouncer-mini`) path watcher feeding `[hooks.file_changed]` events |
| `goal` | Long-horizon goal lifecycle subsystem: `Goal`, `GoalSnapshot`, `GoalStatus`, `GoalStore`, `GoalAccounting`, `GoalSupervisor`, and the `AutonomousDriver` / `AutonomousRegistry` pair backing autonomous sessions |
| `history_integrity` | Concrete vault-key resolution for the transcript/session-log hash-chain (`zeph-subagent`/`zeph-session` stay vault-free by design); derives history-chain subkeys from `ZEPH_HISTORY_KEY`, decoupled from `ZEPH_DURABLE_KEY` |
| `http` | Shared HTTP client construction for consistent timeout and TLS configuration |
| `instructions` | `load_instructions()` — auto-detects and loads provider-specific instruction files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `zeph.md`) from the working directory; injects content into the volatile system prompt section with symlink boundary check, null byte guard, and 256 KiB per-file size cap. `InstructionWatcher` reloads `instruction_blocks` in place on any `.md` change (500 ms debounce) — no agent restart required |
| `instrumented_channel` | Instrumented wrappers around tokio channels for queue-depth metrics |
| `json_event_sink` / `json_event_layer` | Single stdout writer and tracing layer backing `--json` mode |
| `lsp_hooks` | LSP context injection hooks: `LspHookRunner` accumulates `LspNote` entries after native tool execution and formats them into the next LLM call. Two hooks ship today — diagnostics-on-write (compiler diagnostics from mcpls) and hover-on-read (hover info for key symbols, concurrent MCP calls). Notes are injected as `Role::User` messages with an `[lsp …]` prefix, matching `[semantic recall]` / `[known facts]` / `[code context]`. Per-turn token budget enforced in `drain_notes()`; degrades silently when mcpls is unavailable (`is_available()` is itself timeout-bounded) |
| `memory_tools` | `ToolExecutor` exposing memory search/recall tools to the LLM |
| `metrics` | Runtime metrics collection; `SecurityEvent` ring buffer (`SECURITY_EVENT_CAP = 100`, FIFO eviction) for the TUI security panel. `SecurityEventCategory` itself lives in `zeph-common` and covers injection flags/blocks, exfiltration, quarantine, truncation, rate limiting, memory validation, pre-execution block/warn, response verification, causal IPI, cross-boundary MCP→ACP, VIGIL flags, and goal drift |
| `metrics_bridge` | Tracing layer that derives `metrics::TurnTimings` from span durations |
| `notifications` | Best-effort per-turn completion notifier |
| `overflow_tools` | `ToolExecutor` for retrieving archived/overflowed tool output bodies from SQLite |
| `pipeline` | Composable, type-safe step chains for multi-stage workflows (`builder`, `builtin`, `parallel`, `step`) |
| `project` | Project-level context detection |
| `provider_factory` | Pure factory helpers building `AnyProvider` instances from config entries |
| `redact` | Regex-based secret redaction (AWS, OpenAI, Anthropic, Google, GitLab, HuggingFace, npm, Docker) |
| `runtime_context` | `RuntimeContext` — `Copy` struct of startup mode flags passed by value to subsystem initializers |
| `runtime_layer` | `RuntimeLayer` trait — observe-only middleware hooks around LLM calls and tool dispatch |
| `serve` | `SessionActor` and `LiveSessionRegistry` backing `zeph serve` |
| `session_resume` | Resume-visibility presentation primitive (resume banner content) |
| `skill_loader` | `SkillLoaderExecutor` — `ToolExecutor` exposing the `load_skill` tool; looks a skill name up in the shared `Arc<RwLock<SkillRegistry>>` and returns the SKILL.md body (truncated to `MAX_TOOL_OUTPUT_CHARS`); name capped at 128 characters; unknown names return a human-readable error rather than a hard failure |
| `skill_invoker` | `SkillInvokeExecutor` — `ToolExecutor` exposing the `invoke_skill` tool with trust-aware sanitization; `Blocked` skills are refused, non-`Trusted` bodies pass through `sanitize_skill_text`, `Quarantined` bodies are additionally wrapped with `wrap_quarantined`; exempt from adversarial policy, VIGIL gate, and tool-schema filter |
| `skill_trust_gate` | `SkillTrustGate` / `SkillBodyResolution` — the shared trust-gating pipeline both skill-body executors above run through, so a quarantined or blocked skill is resolved identically on either path |
| `system_metrics` | Periodic system-metrics background task (feature `sysinfo`) |
| `vault` | Re-export facade over `zeph-vault`: `AgeVaultProvider` (age-encrypted read/write), `EnvVaultProvider` (dev/test only), `VaultProvider` trait, `Secret`. Secret values are `Zeroizing<String>` (zeroize-on-drop) and are not `Clone` |

**Re-exports at the crate root:** `Agent`, `AgentError`, `AgentSessionConfig`, `CONTEXT_BUDGET_RESERVE_RATIO`, `DurableKeyMaterial`, `SecurityWiringSnapshot`, `SkillConfigParams`, `AdversarialPolicyInfo`, `ProviderConfigSnapshot`, `ShellOverlaySnapshot`, `Attachment`, `AttachmentKind`, `Channel`, `ChannelError`, `ChannelMessage`, `LoopbackChannel`, `LoopbackEvent`, `LoopbackHandle`, `StopHint`, `ToolStartData`, `ToolStartEvent`, `ToolOutputData`, `ToolOutputEvent`, `Config`, `ConfigError`, `RuntimeContext`, `InvokeSkillParams`, `SkillInvokeExecutor`, `SkillTrustSnapshot`, `SkillLoaderExecutor`, `SkillBodyResolution`, `SkillTrustGate`, `resolve_require_check`, `content_hash` (from `zeph-common`), `DiffData` (from `zeph-tools`), plus the sanitizer and exfiltration-guard types re-exported from `zeph-sanitizer`.

### Subsystems that live in sibling crates

`zeph-core` wires these together but does not define them. Look in the owning crate for their types and config:

| Subsystem | Crate |
|---|---|
| Application bootstrap (`AppBuilder`), `SchedulerExecutor`, `zeph agents` CLI wiring | `zeph` (binary, `src/`) |
| Context budget, assembler, typed-page compaction | `zeph-context` |
| `ContextService`, subgoal-aware compaction (`SubgoalRegistry`, `score_blocks_subgoal*`) | `zeph-agent-context` |
| History load / message persistence / graph extraction | `zeph-agent-persistence` |
| `FeedbackDetector`, `JudgeDetector`, implicit-correction detection | `zeph-agent-feedback` |
| `doom_loop_hash` | `zeph-agent-tools` |
| `ContentSanitizer`, `ExfiltrationGuard`, `QuarantinedSummarizer` | `zeph-sanitizer` |
| `SubAgentManager`, `PermissionGrants`, transcripts, subagent hooks and memory scopes | `zeph-subagent` |
| `TaskGraph`, `Planner`, `DagScheduler`, `LlmAggregator` | `zeph-orchestration` |
| Experiment engine (`Variation`, `Evaluator`, `SearchSpace`) | `zeph-experiments` |
| `SecurityEventCategory`, task supervision, shared text/hash utilities | `zeph-common` |

## Configuration

Key `AgentConfig` fields (TOML section `[agent]`):

| Field | Type | Default | Env override | Description |
|-------|------|---------|--------------|-------------|
| `name` | string | `"Zeph"` | — | Agent display name |
| `max_tool_iterations` | usize | `10` | — | Max tool-call iterations per turn (validated: must be <= 100) |
| `auto_update_check` | bool | `true` | `ZEPH_AUTO_UPDATE_CHECK` | Check GitHub releases for a newer version on startup / via scheduler |

Instruction loading is configured with two flat fields in the same `[agent]` section:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `instruction_auto_detect` | bool | `true` | Auto-detect provider-specific files (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`) |
| `instruction_files` | `Vec<PathBuf>` | `[]` | Additional instruction files always loaded (absolute or relative to cwd) |

> [!NOTE]
> `zeph.md` and `.zeph/zeph.md` are always loaded regardless of `instruction_auto_detect`. Each file is capped at
> 256 KiB by `load_instructions()`; larger files are skipped. Use `--instruction-file <path>` at the CLI to supply
> extra files at startup without modifying the config file.

> [!TIP]
> Instruction files support hot reload — edit any watched `.md` file while the agent is running and the updated content is applied within 500 ms on the next inference turn. The watcher starts automatically when at least one instruction path is resolved.

Key `LspConfig` fields (TOML section `[agent.lsp]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable LSP context injection hooks |
| `mcp_server_id` | string | `"mcpls"` | MCP server ID to use for LSP calls |
| `token_budget` | usize | `2000` | Maximum tokens spent on LSP-injected context per turn |
| `call_timeout_secs` | u64 | `5` | Per-MCP-call timeout; an expired call is dropped and the turn continues |
| `diagnostics.enabled` | bool | `true` | Fetch compiler diagnostics after a write tool completes |
| `diagnostics.max_per_file` | usize | `20` | Maximum diagnostics per file |
| `diagnostics.min_severity` | string | `"error"` | Minimum severity to include: `"error"`, `"warning"`, `"info"`, `"hint"` |
| `hover.enabled` | bool | `false` | Pre-fetch hover info for key symbols after a read tool completes |
| `hover.max_symbols` | usize | `5` | Maximum hover entries per file |

```toml
[agent.lsp]
enabled = true
mcp_server_id = "mcpls"
token_budget = 2000
call_timeout_secs = 5

[agent.lsp.diagnostics]
enabled = true
max_per_file = 20
min_severity = "error"

[agent.lsp.hover]
enabled = false
max_symbols = 5
```

> [!NOTE]
> LSP context injection requires the [mcpls](https://github.com/bug-ops/mcpls) MCP server to be configured. If mcpls is unavailable, hooks degrade silently — the agent continues normally with no LSP context injected. Enable via `--lsp-context` CLI flag or `zeph init` wizard.

Key `DocumentConfig` fields (TOML section `[memory.documents]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `collection` | string | `"zeph_documents"` | Qdrant collection for document chunks |
| `chunk_size` | usize | `1000` | Target characters per chunk |
| `chunk_overlap` | usize | `100` | Overlap between chunks |
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
| `soft_compaction_threshold` | f32 | `0.60` | Context usage ratio at which soft compaction (stale tool-output pruning) begins |
| `hard_compaction_threshold` | f32 | `0.90` | Context usage ratio at which LLM middle-out compaction is forced |
| `sqlite_pool_size` | u32 | `5` | SQLite connection pool size for memory storage |

Key `CompressionConfig` fields (TOML section `[memory.compression]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `archive_tool_outputs` | bool | `false` | Archive tool output bodies to SQLite (Memex) before compaction; UUID back-references are injected into summaries |

Key `CompressionGuidelinesConfig` fields (TOML section `[memory.compression_guidelines]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `categorized_guidelines` | bool | `false` | Tag ACON failure pairs by category (tool_output / assistant_reasoning / user_context) and maintain per-category guideline blocks |

> [!NOTE]
> `archive_tool_outputs` (Memex) and `categorized_guidelines` (ACON per-category) are both disabled by default and must be explicitly opted in via the config keys above.

```toml
[agent]
auto_update_check = true   # set to false to disable update notifications
```

Set `ZEPH_AUTO_UPDATE_CHECK=false` to disable update notifications without changing the config file.

Key `SessionConfig.recap` fields (TOML section `[session.recap]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `on_resume` | bool | `true` | Show a recap of the previous session on resume when a digest is available |
| `max_tokens` | usize | `200` | Maximum tokens for the recap text |
| `provider` | string | `""` | Provider name (references `[[llm.providers]]`) for recap generation; empty = primary provider |
| `max_input_messages` | usize | `20` | Recent messages included when generating a fresh recap (no cached digest) |

```toml
[session.recap]
on_resume          = true
max_tokens         = 200
provider           = "fast"   # optional; references [[llm.providers]] name
max_input_messages = 20
```

> [!TIP]
> Use `/recap` to request a session recap on demand at any time during a session, regardless of the `enabled` setting.

Key `IntegrityConfig` fields (TOML section `[integrity]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `anchor` | `"vault"` / `"none"` | `"vault"` | Vault-anchor downgrade resistance for transcript/session hash-chains. `"vault"` writes a per-file anchor on finalize, closing the gap where a whole-file chain-field strip would otherwise be indistinguishable from legacy pre-feature content; degrades gracefully to chain-only protection if the vault is unreachable at bootstrap. `"none"` opts out |
| `max_session_anchors` | usize | `512` | Upper bound on session anchors retained in the vault; the reconcile sweep evicts the oldest (by embedded `written_at`) once exceeded — an evicted session degrades to chain-only protection, never a brick |

```toml
[integrity]
anchor              = "vault"
max_session_anchors = 512
```

> [!NOTE]
> Vault-anchor protection layers on top of the always-available keyed-BLAKE3 hash-chain (`ZEPH_HISTORY_KEY`), which alone detects in-place edits but not a fully consistent whole-file strip. Manage sealed durable executions and manual verification via the `zeph durable seal-integrity` and `zeph sessions verify` CLI subcommands.

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
| `/skills` | List loaded skills, grouped by category when available |
| `/skills confusability` | Show skill pairs with high embedding similarity (potential disambiguation failures) |
| `/skills injection` | Show skills flagged by the injection scanner |
| `/skills trust` | Show the current trust level of every loaded skill |
| `/skill <name>` | Load and display a skill body |
| `/skill create <description>` | Generate a `SKILL.md` from a natural-language description via LLM |
| `/feedback <skill> <message>` | Submit feedback for a skill |

> [!TIP]
> `/feedback` is the strongest supervision signal for skill ranking — the outcome is persisted and updates the
> Wilson-score posterior used for Bayesian re-ranking on the next match.

## Self-learning configuration

Key `AgentConfig.learning` fields (TOML section `[agent.learning]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `correction_detection` | bool | `true` | Enable `FeedbackDetector` implicit correction capture |
| `correction_confidence_threshold` | f32 | `0.6` | Minimum detector confidence to persist a `UserCorrection` |
| `correction_recall_limit` | u32 | `3` | Max corrections retrieved per context-build turn |
| `correction_min_similarity` | f32 | `0.75` | Minimum embedding similarity for correction recall |
| `detector_mode` | `"regex"` / `"judge"` / `"model"` | `"regex"` | Detection strategy: regex-only, LLM-backed judge with adaptive regex fallback, or `LlmClassifier`-backed classification |
| `judge_model` | string | `""` | Model for the judge detector (e.g. `"claude-sonnet-5"`); empty = use primary provider |
| `feedback_provider` | string | `""` | Provider name from `[[llm.providers]]` used by `detector_mode = "model"`; empty = primary provider |
| `judge_adaptive_low` | f32 | `0.5` | Regex confidence below this value skips judge invocation (treated as "not a correction") |
| `judge_adaptive_high` | f32 | `0.8` | Regex confidence above this value skips judge invocation (high-confidence regex match accepted) |

> [!NOTE]
> The detectors themselves live in [`zeph-agent-feedback`](../zeph-agent-feedback/README.md), which matches
> patterns across 7 languages (English, Russian, Spanish, German, French, Chinese Simplified, Japanese).
> `detector_mode = "model"` falls back to regex-only when the named provider cannot be resolved — it never
> fails startup.

Key `LlmConfig` fields (TOML section `[llm]`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `summary_model` | string? | `null` | Shorthand spec for the summarization provider. Formats: `ollama/<model>`, `claude[/<model>]`, `openai[/<model>]`, `compatible/<name>`, `candle`. Ignored when `[llm.summary_provider]` is set. |
| `summary_provider` | table? | `null` | Structured summarization provider (takes precedence over `summary_model`). Same fields as `[llm.orchestrator.providers.*]`: `type`, `model`, `base_url`, `embedding_model`, `device`. For `compatible` type, `model` is the `[[llm.compatible]]` entry name. |
| `router_ema_enabled` | bool | `false` | Enable per-provider EMA latency tracking and reordering |
| `router_ema_alpha` | f64 | `0.1` | EMA smoothing factor (lower = slower adaptation) |
| `router_reorder_interval` | u64 | `10` | Seconds between provider list reordering |

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

In-session commands for task orchestration (integrates `zeph-orchestration`):

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
| `default_idle_timeout_secs` | `Option<u64>` | `None` | Graph-wide default for `TaskNode::idle_timeout_secs`; kills a task if it emits no progress heartbeat (written once per agent-loop turn) for this many seconds. Opt-in — `None` disables idle enforcement. Only enforced on the normal spawn dispatch path; `RunInline` tasks are exempt. Must be set above the longest expected single-turn duration. Independent of `run_timeout` enforcement, which applies to both spawned and `RunInline` dispatch, defaulting `RunInline` tasks to the graph-global 300s bound instead of running unbounded |

> [!NOTE]
> A task whose verification (plan-level or per-task) judges output incomplete and for which no automatic repair resolves it now surfaces a visible signal to the user instead of only a debug/warn log line. `state_injection` recovery lets the graph continue past a failed node on terminal `Abort`/retry-exhausted failures instead of pausing unrelated work.

## Experiment Commands

In-session commands for autonomous self-experimentation (integrates `zeph-experiments`):

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

`zeph agents` provides CRUD management of sub-agent definition files outside of a running session. The
subcommands are declared in the `zeph` binary crate and back onto `zeph-subagent`'s `AgentsCommand`:

| Command | Description |
|---------|-------------|
| `zeph agents list` | Print all discovered definitions with name, scope, description, and model |
| `zeph agents show <name>` | Print full detail of a single definition |
| `zeph agents create <name> --description <desc> [--dir <path>] [--model <id>]` | Scaffold a new `.md` definition (name must match `[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}`; default dir `.zeph/agents`) |
| `zeph agents edit <name>` | Open the definition file in `$VISUAL` / `$EDITOR` (validates parse on exit) |
| `zeph agents delete <name> [--yes]` | Delete a definition file with interactive confirmation |
| `zeph agents fleet [--status <s>] [--limit <n>]` | List agent sessions recorded in the fleet database |

> [!TIP]
> The same CRUD operations are available interactively in the TUI agents panel — press `a` in the TUI to open the panel, then `c` (create), `e` (edit), `d` (delete), Enter (detail view).

## Speculative tool dispatch

`SpeculationEngine` pre-runs read-only tool calls while the LLM generates its response. Two activation paths are supported:

- **SSE decoding path** — `claude_sse_to_tool_stream` emits `ToolBlockStart` at `content_block_start`; when confidence exceeds `confidence_threshold`, `try_dispatch(Trusted)` fires with a 2 s timeout.
- **PASTE pattern path** — `run_paste_skill_activation` calls `PatternStore::predict` per active skill and dispatches candidates above threshold with per-skill trust; `observe_paste_transition` records transitions for future pattern learning.

`requires_confirmation` and `is_tool_speculatable` are required methods with no trait default (#6067) — every `ToolExecutor`/`ErasedToolExecutor` implementor, including wrappers, must state its policy explicitly rather than inherit a permissive fallback. This closed a recurring wrapper-forwarding defect class where a decorator silently fell back to an inherited default instead of forwarding to its inner executor. Only executors that explicitly return `true` from `is_tool_speculatable` (and `false` from `requires_confirmation`) can be speculatively dispatched.

Configure via `[tools.speculative]` in `config.toml`:

```toml
[tools.speculative]
mode                  = "decoding"   # "off" | "decoding" | "pattern" | "both"
max_in_flight         = 4            # concurrent speculative dispatches
confidence_threshold  = 0.55         # minimum prediction confidence to dispatch
max_wasted_per_minute = 100          # circuit breaker on discarded speculative work
ttl_seconds           = 30           # how long a speculative result stays committable
audit                 = true         # record every speculative dispatch
```

Nested `[tools.speculative.pattern]` and `[tools.speculative.allowlist]` tables tune the PASTE
pattern store and restrict which tools may ever be speculated on.

> [!NOTE]
> The speculation engine only runs when the agent is not in `--bare` mode. Committed speculative results that carry `ToolError::ConfirmationRequired` trigger a `tracing::error!` in debug builds, making the invariant machine-checkable at zero release cost.

## Goal lifecycle and TACO output compression

The `goal` module tracks long-horizon goals across turns: `Goal` / `GoalSnapshot` / `GoalStatus` carry the
state, `GoalStore` persists it, `GoalAccounting` meters spend against it, and `GoalSupervisor` +
`AutonomousDriver` / `AutonomousRegistry` drive autonomous sessions. Tool outputs for completed or stale
goals are compressed by the TACO (Tool-Aware Compaction Optimization) pipeline, which archives bodies to
SQLite before the LLM compaction call and injects UUID back-references into the resulting summary — gated
by `archive_tool_outputs` under `[memory.compression]`.

## ShadowSentinel safety probes

`ShadowSentinel` (`[security.shadow_sentinel]`) intercepts high-risk tool calls before execution and issues a pre-execution LLM safety probe. Probe outcomes are recorded in the `safety_shadow_events` SQLite table, forming a persistent cross-session safety audit trail.

```toml
[security.shadow_sentinel]
enabled            = false   # opt-in (default: false)
max_probes_per_turn = 3      # max LLM probes issued per agent turn
probe_timeout_ms   = 2000   # per-probe timeout; fail-open on expiry
```

> [!NOTE]
> ShadowSentinel is fail-open by default — a timed-out or failed probe does not block execution. Set `deny_on_timeout = true` to block tool calls when the probe cannot complete within `probe_timeout_ms`. Hot-path trajectory/tool-history reads are themselves bounded by `probe_timeout_ms.min(2000)` so a stalled DB connection cannot block dispatch (#6293).

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
| `ZEPH_TOOL_NAME` | `pre_tool_use`, `post_tool_use` | Name of the tool being called |
| `ZEPH_TOOL_ARGS_JSON` | `pre_tool_use`, `post_tool_use` | JSON-encoded tool arguments (truncated at 64 KiB) |
| `ZEPH_SESSION_ID` | `pre_tool_use`, `post_tool_use` | Session ID (main agent only; omitted in sub-agents) |
| `ZEPH_TOOL_DURATION_MS` | `post_tool_use` | Tool execution duration in milliseconds |

`[[hooks.pre_tool_use]]` and `[[hooks.post_tool_use]]` accept `HookMatcher` entries with pipe-separated tool name patterns. `pre_tool_use` fires before the `RuntimeLayer` permission check so observers see all attempted calls.

```toml
[[hooks.pre_tool_use]]
match = "shell|bash"
command = "echo tool=$ZEPH_TOOL_NAME args=$ZEPH_TOOL_ARGS_JSON"
timeout_secs = 5

[[hooks.post_tool_use]]
match = "*"
command = "echo finished $ZEPH_TOOL_NAME in ${ZEPH_TOOL_DURATION_MS}ms"
timeout_secs = 5
```

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | ✓ | SQLite storage backend; propagates to every storage-backed workspace crate |
| `postgres` | — | PostgreSQL storage backend; propagates to the same set of crates (mutually exclusive with `sqlite`) |
| `candle` | — | Local inference via Candle (enables `zeph-llm/candle`) |
| `cuda` | — | CUDA backend for Candle |
| `metal` | — | Metal backend for Candle on Apple Silicon |
| `classifiers` | — | Local NER / classifier models (`zeph-llm/classifiers`, `zeph-sanitizer/classifiers`) |
| `gonka` | — | Gonka distributed-inference provider (`zeph-llm/gonka`) |
| `cocoon` | — | Cocoon encrypted-provider support (`zeph-llm/cocoon`, `zeph-commands/cocoon`) |
| `index` | — | AST-based code indexing and repo map (`zeph-agent-context/index`) |
| `scheduler` | — | Cron-based scheduler integration; exposes the `SchedulerExecutor` tools |
| `sysinfo` | — | System resource metrics via the `sysinfo` crate |
| `profiling` | — | Runtime profiling via `tracing-subscriber` |
| `profiling-alloc` | — | Allocation profiling (implies `profiling`) |
| `mock` | — | `MockVaultProvider` for tests (`zeph-vault/mock`) |

## Installation

```bash
cargo add zeph-core
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
