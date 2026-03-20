# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- feat(memory): semantic response caching (#1521) — extend `ResponseCache` with embedding-based similarity lookup alongside exact-match caching; add `get_semantic()`, `put_with_embedding()`, `invalidate_embeddings_for_model()`, and `cleanup()` methods; single embed() call per cache miss via `CacheCheckResult` enum; configurable similarity threshold and max candidates; tool-call responses excluded from semantic store; stale embeddings NULLed on model change (exact-match entries preserved); new config fields: `llm.semantic_cache_enabled`, `llm.semantic_cache_threshold`, `llm.semantic_cache_max_candidates`; env overrides `ZEPH_LLM_SEMANTIC_CACHE_{ENABLED,THRESHOLD,MAX_CANDIDATES}`; DB migration 037
- feat(memory): add `AnchoredSummary` struct with structured 5-section schema (session_intent, files_modified, decisions_made, open_questions, next_steps) for context compaction; replaces free-form prose when `[memory] structured_summaries = true` (issue #1607)
- feat(core): structured summarization path in context compaction — applies `chat_typed_erased::<AnchoredSummary>()` for both single-pass and chunked consolidation; falls back to prose on any LLM or validation failure (issue #1607)
- feat(core): `DebugDumper::dump_anchored_summary()` writes `{N}_anchored-summary.json` with section completeness metrics, total_items, token_estimate, and fallback flag when `--debug-dump` is active (issue #1607)
- feat(config): `[memory] structured_summaries = false` config field enables opt-in structured compaction summaries (issue #1607)
- feat(tools): dynamic tool schema filtering — sends only relevant tool definitions to the LLM per turn, selected by embedding similarity between user query and tool descriptions; configurable via `[agent.tool_filter]` with `enabled`, `top_k`, `always_on`, and `min_description_words`; disabled by default (#2020)
- feat(channels): register Discord slash commands (`/reset`, `/skills`, `/agent`) at startup via fire-and-forget background task; idempotent via `PUT /applications/{id}/commands` (CHAN-05, epic #1978)
- feat(channels): extract shared `CONFIRM_TIMEOUT` constant (30s) to `zeph-channels` crate; Telegram, Discord, and Slack `confirm()` all reference it (CHAN-02, epic #1978)

- refactor(memory): wrap `ResponseCache::cleanup()` DELETE and UPDATE operations in a single SQLite transaction for atomicity (closes #2032)

### Changed

- refactor(config): add `Config::validate()` check for `llm.semantic_cache_threshold`; rejects values outside [0.0, 1.0] and non-finite values (NaN, Inf) with a descriptive error including the env var override hint (#2036)

- fix(channels): `AnyChannel` and `AppChannel` now forward all 16 `Channel` trait methods; previously `send_thinking_chunk`, `send_stop_hint`, `send_usage`, and `send_tool_start` fell through to trait defaults, silently dropping events (CHAN-01, epic #1978)
- fix(channels): Discord and Slack `confirm()` now deny after 30s timeout, matching the existing Telegram behavior; previously they blocked indefinitely waiting for user input (CHAN-02, epic #1978)

- refactor(core): add state-group accessor methods to `Agent<C>` for all sub-structs (`msg`, `memory_state`, `skill_state`, `runtime`, etc.); migration from direct field access is incremental per file (ABS-04, epic #1977)
- fix(llm): `convert_messages_structured()` now preserves `Recall`, `CodeContext`, `Summary`, and `CrossSession` variants in OpenAI tool-use messages instead of silently dropping them (ABS-05, epic #1977)
- refactor(core): `with_context_budget()` emits `tracing::warn` when `budget_tokens == 0`; `Agent::new()` has `debug_assert` for `max_active_skills > 0` (ABS-07, epic #1977)

- refactor(llm): extract `UsageTracker` struct to consolidate duplicate token usage tracking across Claude, OpenAI, Ollama, and Gemini providers (DRY-01+06, epic #1975)
- refactor(memory): remove duplicate `BoxFuture` type alias from `in_memory_store.rs`; import canonical definition from `vector_store.rs` (DRY-05, epic #1975)
- refactor(channels): add `ChannelError::other()` helper; replace 15 `.map_err(|e| ChannelError::Other(e.to_string()))` sites in telegram, discord, slack, and cli channels (DRY-04, epic #1975)
- refactor: remove dead code: `FOCUS_REMINDER_PREFIX` constant, `FocusState::should_remind()`, `ToolRateLimiter::is_tripped()`, `CorrectionKind::Abandonment` variant, `SidequestState::parse_eviction_response()` (epic #1976)
- ci: expand feature matrix to test intermediate feature combinations: `orchestration`, `orchestration,graph-memory`, `daemon,acp`, `tui,scheduler` (epic #1976)

### Fixed

### Performance

- perf(memory): add `expires_at` to `idx_response_cache_semantic` composite index (migration 038) — `get_semantic()` now filters expired rows within the index scan instead of post-filtering on the heap (#2030)

## [0.16.0] - 2026-03-19
### Added

- refactor(orchestration): extract task orchestration into new `zeph-orchestration` crate (Epic #1973 Phase 1g, #1979)
  - New `zeph-orchestration` crate (5,380 LOC) with 8 modules: `aggregator`, `command`, `dag`, `error`, `graph`, `planner`, `router`, `scheduler`
  - Moved `TaskGraph`, `TaskNode`, `TaskId`, `GraphId`, `TaskStatus`, `GraphStatus`, `FailureStrategy`, `GraphPersistence`, `DagScheduler`, `SchedulerAction`, `TaskEvent`, `TaskOutcome`, `LlmPlanner`, `Planner`, `LlmAggregator`, `Aggregator`, `RuleBasedRouter`, `AgentRouter`, `PlanCommand`, `OrchestrationError` to new crate
  - `zeph-core` reduces from 55,860 to ~50,480 LOC (-5,380 LOC, -9.6%)
  - 169 tests migrated to `zeph-orchestration`; zeph-core 1199→1030 tests; workspace total: 5,917 tests
  - `zeph-core/src/orchestration/mod.rs` replaced with re-export shim preserving all `crate::orchestration::*` import paths
  - Added `TaskId::as_u32()` public accessor replacing `pub(crate)` field access from `zeph-core::metrics`
  - Layer 2 crate: depends on `zeph-config`, `zeph-common`, `zeph-llm`, `zeph-memory`, `zeph-sanitizer`, `zeph-subagent`

- refactor(subagent): extract subagent management into new `zeph-subagent` crate (Epic #1973 Phase 1f)
  - New `zeph-subagent` crate (9,036 LOC) with 11 modules: `command`, `def`, `error`, `filter`, `grants`, `hooks`, `manager`, `memory`, `resolve`, `state`, `transcript`
  - `SubAgentManager`, `SubAgentHandle`, `SubAgentDef`, `SubAgentGrant`, `SubAgentTranscript`, `SubAgentError`, `SubAgentState`, `ToolFilter`, `SkillFilter`, `SubagentHooks` moved to new crate
  - `zeph-core` reduces to 55,860 LOC (from 64,851), -9,028 LOC (13.9% reduction)
  - `spawn_for_task` refactored to accept a generic completion callback (`F: FnOnce`) eliminating orchestration dependency from `zeph-subagent`
  - `zeph-core/src/lib.rs` re-exports `pub mod subagent { pub use zeph_subagent::*; }` preserving all `crate::subagent::*` import paths in orchestration, agent, and test code
  - 299 tests in `zeph-subagent`; total workspace: 5,917 tests passing
  - Phase 1g will extract `zeph-orchestration` (deferred due to resolved circular dependency)

- refactor(sanitizer): extract content sanitization into new `zeph-sanitizer` crate (Epic #1973 Phase 1e)
  - New `zeph-sanitizer` crate at Layer 2 with 6 core modules: `exfiltration`, `guardrail`, `memory_validation`, `pii`, `quarantine`, and lib exports
  - Extracted 4,337 LOC from `zeph-core/src/sanitizer/` including `ContentSanitizer`, `ExfiltrationGuard`, `PiiFilter`, `MemoryWriteValidator`, `QuarantinedSummarizer`, and guardrail logic
  - Clean direct imports throughout `zeph-core` and binary crates: `use zeph_sanitizer::*` (no re-export shim pattern)
  - Feature flag `guardrail` propagated from `zeph-core` to `zeph-sanitizer`
  - `zeph-core` re-exports all public types from `zeph-sanitizer` preserving existing import paths for downstream consumers

- refactor(experiments): extract experiments logic into new `zeph-experiments` crate (Epic #1973 Phase 1d)
  - New `zeph-experiments` crate at Layer 2 with `ExperimentEngine`, `Evaluator`, `BenchmarkSet`, and all experiment-related types
  - Moved: `ExperimentEngine`, `ExperimentSessionReport`, `Evaluator`, `JudgeOutput`, `CaseScore`, `EvalReport`, `EvalError`, `VariationGenerator`, `GridStep`, `Random`, `Neighborhood`, `ParameterRange`, `SearchSpace`, `ParameterKind`, `Variation`, `VariationValue`, `ExperimentResult`, `ExperimentSource`, `BenchmarkCase`, `BenchmarkSet`, `ConfigSnapshot`, `GenerationOverrides`
  - `zeph-core/src/experiments` replaced with thin re-export shim providing `pub use zeph_experiments::*` — zero import path changes for consumers using `crate::experiments::*`
  - Feature flag `experiments` propagated to `zeph-experiments` and remains feature-gated
  - All public API preserved via re-export module in `zeph-core`

- refactor(vault): extract vault logic into new `zeph-vault` crate (Epic #1973 Phase 1c)
  - New `zeph-vault` crate at Layer 1 with `VaultProvider` trait, `EnvVaultProvider`, `AgeVaultProvider`, `ArcAgeVaultProvider`, `AgeVaultError`, `default_vault_dir()`
  - `MockVaultProvider` gated behind `#[cfg(any(test, feature = "mock"))]` — accessible from downstream test code via `zeph-vault/mock` feature
  - `pub use zeph_common::secret::{Secret, VaultError}` re-exported from `zeph-vault` preserving `crate::vault::Secret` paths
  - `zeph-core/src/vault.rs` replaced with thin re-export shim `pub use zeph_vault::*;` — zero import path changes in consumers
  - `age_encrypt_decrypt_resolve_secrets_roundtrip` integration test kept in `zeph-core` (depends on `SecretResolver` trait)
  - `age` and `zeroize` direct dependencies removed from `zeph-core` (now provided transitively via `zeph-vault`)

- refactor(config): extract pure-data configuration types into new `zeph-config` crate (Epic #1973 Phase 1a)
  - New `zeph-config` crate at Layer 1 (no `zeph-core` dependency) with all pure-data config structs
  - Moved: `AgentConfig`, `FocusConfig`, `LlmConfig`, `MemoryConfig`, `SecurityConfig`, `TrustConfig`, `TimeoutConfig`, `RateLimitConfig`, `ContentIsolationConfig`, `QuarantineConfig`, `ExfiltrationGuardConfig`, `PiiFilterConfig`, `CustomPiiPattern`, `MemoryWriteValidationConfig`, `GuardrailConfig`, `GuardrailAction`, `GuardrailFailStrategy`, `PermissionMode`, `MemoryScope`, `ToolPolicy`, `SkillFilter`, `HookDef`, `HookType`, `HookMatcher`, `SubagentHooks`, `DumpFormat`, and all other pure-data config types
  - `zeph-core` re-exports all types from `zeph-config` — no import path changes for downstream crates
  - Feature flags propagated: `guardrail`, `lsp-context`, `compression-guidelines`, `experiments`, `policy-enforcer`
  - `ContentSanitizer::escape_delimiter_tags` and `apply_spotlight` widened from `pub(crate)` to `pub`
  - Added `SubAgentHandle::for_test()` test helper for unit tests
  - `ExperimentConfig::validate()` moved to `zeph-config` returning `Result<(), String>`

### Changed

- refactor(agent): decompose `Agent<C>` struct into named sub-structs (EPIC-02)
  - Extracted `InstructionState`, `ExperimentState`, `CompressionState`, `MessageState`, `SessionState`
  - Moved all sub-struct definitions to `agent/state/mod.rs`; `agent/mod.rs` reduced from 4,396 to 4,163 lines
  - No public API changes; all sub-structs are `pub(crate)` within the agent module hierarchy
- refactor(agent): split large test modules into sibling `tests.rs` files (EPIC-03)
  - `agent/tool_execution/mod.rs`: 4,502 → 622 lines; tests moved to `tool_execution/tests.rs`
  - `agent/context/mod.rs`: 4,320 → 94 lines; tests moved to `context/tests.rs`
- refactor(memory): split large graph and SQLite modules into `mod.rs` + `tests.rs` (EPIC-05)
  - `graph/store.rs` (3,886 lines) → `graph/store/mod.rs` (1,389) + `graph/store/tests.rs` (2,500)
  - `graph/resolver.rs` (2,021 lines) → `graph/resolver/mod.rs` (886) + `graph/resolver/tests.rs` (1,138)
  - `sqlite/messages.rs` (1,559 lines) → `sqlite/messages/mod.rs` (752) + `sqlite/messages/tests.rs` (810)
- refactor(tools): split large filter and shell modules into `mod.rs` + `tests.rs` (EPIC-06)
  - `filter/declarative.rs` (2,852 lines) → `filter/declarative/mod.rs` (1,044) + `filter/declarative/tests.rs` (1,811)
  - `shell.rs` (2,459 lines) → `shell/mod.rs` (957) + `shell/tests.rs` (1,505)
- refactor(common): create `zeph-common` crate with shared utility functions
  - New crate at Layer 0 (no zeph-* dependencies): `text`, `net`, `sanitize` modules
  - Consolidates 3 independent `truncate_to_bytes` implementations into one
  - Consolidates 2 independent `is_private_ip` implementations into one canonical version
  - `zeph-tools/src/net.rs` now re-exports from `zeph-common`
  - `zeph-a2a/src/client.rs` now uses `zeph-common::net::is_private_ip`
  - `strip_control_chars` and `strip_null_bytes` primitives in `zeph-common::sanitize`
- refactor(tools): remove `zeph-index` dependency from `zeph-tools` (fixes same-layer violation)
  - Language detection and grammar/query setup inlined in `search_code.rs` using tree-sitter directly
  - Layered architecture invariant restored: Layer 1 crates no longer import each other
- docs(specs): amend constitution to formalize layered crate DAG (Layer 0–4)
  - Replaces "leaf crates must NOT import each other directly" with explicit layer model
  - Documents which cross-crate dependencies are legitimate (downward-only)
- docs(zeph-common): add `README.md` — describes modules, optional treesitter feature, and usage examples (closes #1969)
- refactor(zeph-core): consolidate `text` module into `zeph-common` — delete duplicate `zeph-core::text`, add `zeph-common` dep, re-export via `pub use` (closes #1967)
- refactor(zeph-common): extract shared tree-sitter symbol query constants and helpers into optional `treesitter` feature — `zeph-tools` and `zeph-index` now import from `zeph-common::treesitter` (closes #1968)
- refactor(agent): group loose `Agent` fields into `FeedbackState` and move `rate_limiter` into `RuntimeConfig` (closes #1971)
  - `feedback_detector` + `judge_detector` → `feedback: FeedbackState { detector, judge }`
  - `rate_limiter` moved from top-level `Agent` field into `RuntimeConfig`
  - All ~30 call sites updated across `mod.rs`, `builder.rs`, `tool_execution/native.rs`
- test(zeph-core): add unit tests for agent state sub-structs in `agent/state/tests.rs` (closes #1970)
  - Covers `InstructionState`, `ExperimentState`, `MessageState`, `SessionState`, `RuntimeConfig`, `FeedbackState`, and `CompressionState`
  - Feature-gated tests: `experiments` and `context-compression` paths verified independently

### Fixed

- fix(llm): OpenAI API 400 Bad Request on skill documentation queries (closes #1952)
  - Root cause: `StructuredApiMessage.content` was `String` instead of `Option<String>`. When LLM called tools without preceding text, empty string `""` was serialized alongside `tool_calls`, but OpenAI API requires `null` (or absent) for messages with `tool_calls`
  - Changed `content: String` → `content: Option<String>` with `#[serde(skip_serializing_if = "Option::is_none")]`
  - Updated `convert_messages_structured` to emit `None` when text content is empty
  - Fixed tool `arguments` JSON fallback: `unwrap_or_default()` → `unwrap_or_else(|_| "{}".to_owned())`
  - Added regression test: `convert_messages_structured_assistant_tool_only_content_is_none`
  - Error was intermittent because it only manifested when prior assistant turns had tool_calls without text and survived compression cycles
- fix(memory): `QdrantOps::ensure_collection` and `ensure_collection_with_quantization` now detect
  vector dimension mismatches on existing collections and automatically recreate them instead of
  silently returning `Ok(())` with stale dimensions (closes #1951)
  - Affects all Qdrant-backed collections: `zeph_conversations`, `zeph_session_summaries`,
    `zeph_key_facts`, `zeph_corrections`, `zeph_graph_entities`, and code-index collections
  - Logs a `WARN`-level message with collection name, existing and required dimensions before
    recreating; data loss is expected and intentional when the embedding model changes
  - Added four `#[ignore]` integration tests covering idempotency (same size) and recreation
    (mismatched size) for both `ensure_collection` and `ensure_collection_with_quantization`

## [0.15.3] - 2026-03-17

### Fixed

- fix(core): `resolve_config_path()` now falls back to `~/.config/zeph/config.toml` when `config/default.toml` is absent relative to CWD (closes #1945) — resolves ACP stdio/HTTP startup failure when launched from an IDE workspace directory; CWD-relative default is still preferred when the file exists (no behavior change for CLI/TUI); each resolution step emits a `tracing::debug!` message with the resolved path and source
- fix(tui): filter metrics (`filter_raw_tokens`, `filter_saved_tokens`, `filter_applications`) always showed zero in the TUI dashboard when tool execution occurred via the native path (closes #1939)
  - Root cause: two "remaining tools" loops in `native.rs` (self-reflection `Ok(true)` and `Err(e)` branches) discarded `FilterStats` from parallel tool outputs without recording metrics
  - `record_filter_metrics` extracted to `agent/utils.rs` as shared helper; called from all four metric-recording sites (3 native + 1 legacy)
  - Added two regression tests: normal native path and self-reflection remaining-tools path

### Added

- test(memory): add integration tests for `store_session_summary` → Qdrant upsert roundtrip (closes #1916) — four `#[ignore]` tests in `crates/zeph-memory/tests/qdrant_integration.rs` using testcontainers: `store_session_summary_roundtrip`, `store_session_summary_multiple_conversations`, `store_shutdown_summary_full_roundtrip`, `search_session_summaries_returns_empty_when_no_data`; each test guards against silent Qdrant disconnection and verifies both the Qdrant vector path and (where applicable) the SQLite content path
- feat(mcp): OAuth 2.1 PKCE support for remote MCP servers (closes #1930)
  - New `McpTransport::OAuth` variant: `url`, `scopes`, `callback_port`, `client_name`
  - New `McpTransport::Http` variant gains optional `headers` map with vault-reference support (`${VAULT_KEY}` syntax)
  - `McpManager::with_oauth_credential_store()` builder for registering per-server credential stores
  - `VaultCredentialStore` in `zeph-core` persists OAuth tokens to the age vault under `ZEPH_MCP_OAUTH_<SERVER_ID>` keys
  - Two-phase `connect_all()`: stdio/HTTP servers connect concurrently (Phase 1), OAuth servers sequentially with callback listener (Phase 2)
  - Callback server: raw `tokio::net::TcpListener` pre-bound to capture the actual port before client registration
  - SSRF validation on all OAuth metadata endpoints (authorization, token, registration, jwks_uri)
  - Config: `[mcp.servers.*.oauth]` section with `enabled`, `token_storage` (vault/memory), `scopes`, `callback_port`, `client_name`; `headers` map for static bearer tokens
  - Config validation: `headers` and `oauth` are mutually exclusive; vault key collision detection for servers with identical normalized IDs
- feat(index): show indexing progress during background code indexing (#1923)
  - Added `IndexProgress` struct to `zeph-index` with `files_done`, `files_total`, `chunks_created` fields
  - `index_project()` now accepts `progress_tx: Option<&watch::Sender<IndexProgress>>` and sends progress after each file
  - CLI mode: prints "Indexing codebase in the background (N files)..." and "Codebase indexed: N files, M chunks (Xs) — code search is ready." to stderr
  - TUI mode: shows "Indexing codebase... N/M files (X%)" in the status bar, then "Index ready (N files, M chunks)" for 3s after completion
- enhancement(core): `LearningEngine` now performs real behavioral learning from interaction history (closes #1913)
  - New SQLite table `learned_preferences` (migration 036) persists inferred user preferences across sessions
  - Scans `user_corrections` incrementally via watermark (no repeated re-scanning); analyzes every 5 turns when `correction_detection` is enabled
  - Regex-based preference inference with word-boundary anchors: verbosity (concise/detailed), response format (plain/markdown/bullets/headers), language preference
  - Persists preferences above a Wilson-score confidence threshold; up to 3 highest-confidence facts injected into the volatile system prompt block (after `<!-- cache:volatile -->` to preserve prompt caching)
  - Sanitizes injected preference values (strips `\n`/`\r`) and enforces length caps (key ≤ 128 B, value ≤ 256 B)
  - Gated on `LearningConfig::correction_detection` (independent of `LearningConfig::enabled` which controls skill auto-improvement)
- feat(context-compression): CLI flags `--focus`/`--no-focus`, `--sidequest`/`--no-sidequest`, and `--pruning-strategy <reactive|task_aware|mig|task_aware_mig>` for per-session context compression overrides (#1904)
- feat(context-compression): `--init` wizard step for Focus Agent and SideQuest configuration with validated interval inputs
- feat(context-compression): debug dump files for pruning scores (`{n}-pruning-scores.json`), focus knowledge (`{n}-focus-knowledge.txt`), and SideQuest eviction (`{n}-sidequest-eviction.json`) when `--debug-dump` is active
- feat(context-compression): TUI status spinners for `extract_task_goal` background task ("Extracting task goal...") and SideQuest eviction scoring ("SideQuest: scoring tool outputs...")
- obs(orchestration): `LlmPlanner::plan()` and `LlmAggregator::aggregate()` now return token usage data; call sites in `agent/mod.rs` increment `api_calls`, `prompt_tokens`, `completion_tokens`, `total_tokens`, cost, and cache stats in the shared `MetricsCollector` (closes #1899)
- obs(orchestration): `tasks_skipped` counter now correctly incremented in both `GraphStatus::Completed` and `GraphStatus::Failed` arms of `finalize_plan_execution`
- obs(orchestration): `/status` command shows an `Orchestration:` block (plans, tasks completed/failed/skipped) when `orchestration.enabled = true` and at least one plan has been executed

### Fixed

- fix(tui): graph metrics panel now shows correct entity/edge/community counts (closes #1938)
  - `App::with_metrics_rx()` now eagerly reads the initial `MetricsSnapshot` value so counts are visible immediately on TUI startup, not skipped because `has_changed() = false`
  - `spawn_graph_extraction()` in `zeph-memory` now returns `JoinHandle<()>`; a follow-up spawn in `persistence.rs` awaits the handle and re-reads graph counts from the DB after extraction completes, replacing the stale-zero read that happened synchronously before the fire-and-forget task finished
- fix(tui): implement `send_tool_start` in `TuiChannel` — native tool calls now emit a `ToolStart` event so the TUI shows a spinner and `$ command` header before tool output arrives (closes #1931); `handle_tool_output_event` now appends output content when finalizing a streaming tool message
- fix(tui): graph memory metrics (entities/edges/communities) now update every turn instead of only when graph extraction fires — `sync_graph_counts()` is now called per-turn in `process_user_message_inner` in addition to at startup (closes #1932)
- fix(context-compression): `extract_task_goal` is now fire-and-forget — spawns a background tokio task and returns immediately; result is applied at the start of the next Soft compaction (#1909). Eliminates the 5-second blocking LLM call on every compaction that made `task_aware`/`mig`/`task_aware_mig` strategies non-functional for cloud LLM providers. Timeout raised from 5s to 30s in the background task. Current compaction uses the cached goal from the previous turn with no latency impact.
- fix(llm): `/model` list no longer returns 404 for standard OpenAI config — `list_models_remote` was constructing `{base_url}/v1/models` when `base_url` already contains `/v1`; corrected to `{base_url}/models` (closes #1903)
- fix(core): corrections now stored even when `LearningConfig::enabled = false` (closes #1910)
- fix(memory): sync session summaries to Qdrant on compact_context happy path (#1911) — `store_session_summary()` was only called in fallback branches; now also called after a successful `replace_conversation()` in both `compact_context` variants
- Wire `[agent.focus]` and `[memory.sidequest]` config to `AgentBuilder` in all bootstrap paths (`runner.rs`, `daemon.rs`, `acp.rs`); previously both configs were parsed but never applied, causing focus and sidequest to always use defaults (`enabled = false`) (closes #1907)
- fix(memory): use deterministic UUID v5 for session summary Qdrant point to prevent duplicates on repeated compaction (#1917)
- fix(tui): clear "saving to graph..." spinner immediately after `spawn_graph_extraction` — spinner was never cleared since the spawn is fire-and-forget; status is now reset to `""` right after scheduling the background task (closes #1924)
- fix(graph-memory): prevent structural noise from polluting `zeph_graph_entities` graph (closes #1912)
  - Skip graph extraction entirely for `Role::User` messages containing `ToolResult` parts — tool outputs (TOML, JSON, command output) are structural data, not conversational content (FIX-1)
  - Exclude `ToolResult` user messages from the context window passed to the extraction LLM call (FIX-2)
  - Add `min_entity_name_bytes = 3` to `MemoryWriteValidationConfig` and enforce it in `validate_graph_extraction`; also added a matching guard in `EntityResolver::resolve()` via `MIN_ENTITY_NAME_BYTES` constant (FIX-3)
  - Revise extraction prompt: restrict entity types to `person`, `project`, `technology`, `organization`, `concept`; add explicit rules against extracting structural data (config keys, file paths, tool names, TOML/JSON keys), short tokens, and raw command output (FIX-4)

### Security

- Suppress CodeQL `rust/cleartext-logging` false positives on intentional debug/trace log sites (closes #1905)
- Pin all GitHub Actions to full commit SHAs to prevent supply chain attacks (closes #1906)

## [0.15.2] - 2026-03-16

### Added

- feat(core): context compression subsystem — Focus Agent, SWE-Pruner/COMI, and SideQuest eviction behind `context-compression` feature flag (closes #1850, #1851, #1885)
  - **Focus Agent** (#1850): two native tools `start_focus(scope)` and `complete_focus(summary)` that bracket a task window; `complete_focus` truncates conversation history back to the focus checkpoint, synthesizes a Knowledge block from the summary, and pins it (survives all compaction); UUID-based checkpoint marker prevents marker-not-found ambiguity (S4); `/focus status` slash command
  - **SWE-Pruner / COMI** (#1851): `PruningStrategy` enum (`Reactive` | `TaskAware` | `Mig` | `TaskAwareMig`); `score_blocks_task_aware()` uses TF-IDF–weighted Jaccard similarity with a Rust/shell stop-word list (S2); `score_blocks_mig()` adds pairwise redundancy scoring (MIG = relevance − redundancy); blocks with negative MIG are evicted first; task goal is cached by last-user-message hash and re-computed only when the hash changes (S5); configurable via `[memory.compression] pruning_strategy`
  - **SideQuest** (#1885): `SidequestState` tracks up to `max_cursors` largest tool outputs; every `interval_turns` user turns a 5-second LLM call selects stale outputs; eviction is capped at `max_eviction_ratio`; guards: focus-active check, compaction-cooldown, pinned-message protection, JSON-parse fallback; configurable via `[memory.sidequest]`; `/sidequest status` slash command
  - **Critic gap fixes**: S1 — `compact_context()` and `prune_tool_outputs()` skip `focus_pinned` messages; S3 — SideQuest disabled when `pruning_strategy != Reactive`
- feat(security): malicious skill trust tier enforcement (#1853) — fixed `QUARANTINE_DENIED` tool list: replaced dead rule `"file_write"` with actual `FileExecutor` IDs (`write`, `edit`, `delete_path`, `move_path`, `copy_path`, `create_directory`) and added `memory_save`; added `SkillContentScanner` in `zeph-skills::scanner` using shared `RAW_INJECTION_PATTERNS` (relocated from `zeph-mcp::sanitize` to `zeph-tools::patterns` as the single source of truth); `SkillRegistry::scan_loaded()` scans all skill bodies at startup when `[skills.trust] scan_on_load = true` (default); scanner is advisory only — results are `WARN` logged, do not downgrade trust or block tools; new `/skill scan` TUI command for on-demand scan; `--scan-skills-on-load` CLI flag to override config; `--init` wizard step in the security section; `--migrate-config` picks up `scan_on_load` automatically from `default.toml`
- feat(security): `--init` wizard step for pre-execution verification (#1880) — "Enable pre-execution verification?" prompt (default: yes) and conditional "Allowed paths for destructive commands" input added to the Security section; informational note lists default shell tools (`bash`, `shell`, `terminal`); whitespace is trimmed from each path segment; results mapped to `config.security.pre_execution_verify`
- feat(security): pre-execution action verification plugin hook in the tool execution pipeline (TrustBench pattern, issue #1630)
  - `PreExecutionVerifier` trait and `VerificationResult` enum in `zeph-tools`
  - `DestructiveCommandVerifier`: blocks destructive shell commands (`rm -rf /`, `dd if=`, `mkfs`, `fdisk`, etc.) outside configured `allowed_paths`; empty `allowed_paths` = deny-all (safe default)
  - `InjectionPatternVerifier`: blocks SQL injection (`' OR '1'='1`, `UNION SELECT`, `DROP TABLE`), command injection (`; rm`, `| curl`), and path traversal (`../../../etc/passwd`) in any tool's arguments; SSRF patterns (localhost, private IPs) produce a `Warn` (not `Block`)
  - Configurable via `[security.pre_execution_verify]` TOML section with per-verifier `enabled`, `allowed_paths`, and `extra_patterns`
  - `--no-pre-execution-verify` CLI escape hatch for trusted environments
  - TUI security panel shows "Verify blocks" and "Verify warnings" counters
  - New `MetricsSnapshot` fields: `pre_execution_blocks`, `pre_execution_warnings`
  - New `SecurityEventCategory` variants: `PreExecutionBlock`, `PreExecutionWarn`
- feat(security): LLM-based guardrail pre-screener for prompt injection detection (closes #1651) — `GuardrailFilter` sends user input and (optionally) tool output through a configurable guard model before it enters the agent context; configurable action (block/warn), fail strategy (closed/open), timeout, and `max_input_chars` truncation; TUI status bar shows `GRD:on` (green) or `GRD:warn` (yellow) when active; enabled via `--guardrail` CLI flag or `[security.guardrail] enabled = true`; `--init` wizard step added; `/guardrail` slash command shows live stats; `scan_tool_output = false` by default to avoid latency on every tool call
- feat(security): declarative policy compiler for tool call authorization (#1695) — `PolicyEnforcer` evaluates TOML-based allow/deny rules before any tool executes; deny-wins semantics; path traversal normalization via `Path::components()` (CRIT-01); tool name normalization (lowercase, CRIT-02); generic LLM error messages (MED-03); `[tools.policy]` config section with `enabled`, `default_effect`, `rules`, `policy_file`; `--policy-file` CLI flag; `/policy status` and `/policy check` slash commands; `--init` wizard step; optional `policy-enforcer` feature flag (included in `full`)
- feat(tui): compression guidelines status line in memory panel (version + last update) and `/guidelines` slash command to display current guidelines text (closes #1803)
- feat(memory): add `load_compression_guidelines_meta()` query returning `(version, created_at)` without fetching full text
- feat(memory): `conversation_id` column added to `compression_guidelines` table (migration 034); guidelines now prefer conversation-specific over global when a conversation is in scope, with global (NULL) guidelines as fallback (closes #1806)
- feat(memory): add `--compression-guidelines` CLI flag to override `memory.compression_guidelines.enabled` at startup (#1802)
- feat(memory,core): session summary on shutdown (#1816) — when no hard compaction fired during a session, `Agent::shutdown()` now generates a lightweight LLM summary and stores it to the vector store for cross-session recall; the LLM call is wrapped in a 5-second timeout so shutdown never hangs; `SemanticMemory::has_session_summary()` is the primary guard (resilient to failed Qdrant writes); `SemanticMemory::store_shutdown_summary()` persists to both SQLite and the vector store with real FK-linked key facts; new config params `memory.shutdown_summary` (default `true`), `memory.shutdown_summary_min_messages` (default `4`, user turns only), `memory.shutdown_summary_max_messages` (default `20`); `--init` wizard prompts for the feature toggle; TUI status indicator shown during summarization
- test(memory): unit tests for `sanitize_guidelines` special-token (`<|system|>`) and role-prefix (`assistant:`, `user:`) patterns (#1807)
- test(policy): additional coverage for `policy_file` external TOML loading (happy-path, FileTooLarge, FileLoad, FileParse), MAX_RULES exact-boundary (256 rules compile), and `execute_tool_call_confirmed` allow path delegation (#1874)

### Changed

- refactor(features): replace flat feature list with named use-case bundles (#1831) — six bundles added: `desktop` (tui + scheduler + compression-guidelines), `ide` (acp + acp-http + lsp-context), `server` (gateway + a2a + scheduler + otel), `chat` (discord + slack), `ml` (candle + pdf + stt), `full` (all bundles except ml/hardware). All individual feature flags are unchanged and continue to work. `metal` and `cuda` now correctly imply `candle` (pre-existing bug fixed). Migration: no action required — existing `--features tui,scheduler` builds are fully backwards-compatible; use `--features desktop` as the idiomatic equivalent going forward.
- deps: upgrade rmcp 0.17 → 1.2 (#1845) — migrated `CallToolRequestParams` struct literal to builder pattern (`::new().with_arguments()`); removed unused `std::borrow::Cow` import
- observability(router): add tracing instrumentation to cascade router (#1825) — `cascade_chat` and `cascade_chat_stream` now emit `debug`/`info`/`warn` events for provider selection (attempt N of M, classifier mode, threshold), judge scoring (score + threshold + pass/fail decision), quality verdict (score, threshold, reason, should_escalate), best-seen updates, escalation (score, threshold, remaining budget), budget exhaustion and fallback returns; `cascade_chat_stream` log fields aligned with `cascade_chat` for consistency

- refactor(acp): centralize ACP session config wiring via `AgentSessionConfig::from_config()` and `Agent::apply_session_config()` (#1812) — replaces ~25 individually-copied scalar fields in `SharedAgentDeps` and redundant builder call blocks in `spawn_acp_agent`, `runner.rs`, and `daemon.rs` with a single struct; eliminates hardcoded `0.20` literal (now `CONTEXT_BUDGET_RESERVE_RATIO`); fixes missing `with_orchestration_config` and `with_server_compaction` in daemon sessions

### Fixed

- fix(policy): `/policy status` now reports the correct total rule count when rules are loaded from an external `policy_file` — previously `handle_policy_command()` used `policy_config.rules.len()` which only counted inline TOML rules; the handler now compiles the enforcer to get the merged count, falling back to the inline count on compile error (closes #1898)

- fix(orchestration): scheduler deadlock no longer emits misleading "Plan failed. 0/N tasks failed" message — non-terminal tasks are now marked `Canceled` at deadlock time (mirrors `cancel_all()` semantics); the done message now distinguishes pure deadlock ("Plan canceled. N/M tasks did not run."), mixed failure+cancellation ("Plan failed. X/M tasks failed, Y canceled:"), and normal failure paths (closes #1879)
- sec(policy): `load_policy_file()` now canonicalizes the path before reading and rejects policy files whose canonical path escapes the process working directory — mirrors the symlink boundary check already present in `load_instructions()`; adds `PolicyCompileError::FileEscapesRoot` variant (closes #1872)
- fix(security): all MCP tools are now denied for quarantined skills — `TrustGateExecutor` tracks registered MCP tool IDs via `mcp_tool_ids_handle()` and blocks any call whose ID appears in the set; `is_quarantine_denied()` suffix matching provides defence-in-depth for MCP tools matching the `QUARANTINE_DENIED` list (fixes #1876)
- fix(policy): accept "shell"/"sh" as aliases for "bash" tool_id in policy rules — `ShellExecutor` registers as `tool_id="bash"` but users write `tool="shell"` in TOML rules; `resolve_tool_alias()` in `PolicyEnforcer` normalizes both sides (compile-time rule names and runtime tool_id) so `tool="shell"`, `tool="bash"`, and `tool="sh"` all match correctly (closes #1877)
- fix(security): `/policy check` no longer leaks process environment variables into trace output — `PolicyContext.env` is now an empty `HashMap` for the diagnostic command (#1873); added optional `--trust-level <level>` argument to simulate non-default trust tiers (`trusted`, `verified`, `quarantined`, `blocked`); `TrustLevel` now implements `FromStr`
- fix(policy): remove `PolicyEffect::AllowIf` variant — it was declared but evaluated identically to `Allow`, creating misleading TOML documentation; conditions are expressed via rule fields directly (closes #1871)
- fix(core): overflow notice no longer embeds `overflow:` prefix — notice format changed from `[full output stored as overflow:{uuid} — ...]` to `[full output stored — ID: {uuid} — ...]` so the LLM does not pass `overflow:<uuid>` to `read_overflow`, which only accepts bare UUIDs; `read_overflow` now also accepts and strips the legacy `overflow:` prefix for backwards compatibility (closes #1868)
- fix(memory): session summary timeout now attempts plain-text fallback instead of silently returning `None` — when the structured LLM call in `call_llm_for_session_summary()` times out, the agent falls back to a plain `chat()` call (same path already used on structured call error); extracted `plain_text_summary_fallback()` helper to avoid code duplication; added `shutdown_summary_timeout_secs` (default: 10) to `[memory]` config to replace the hardcoded 5s limit (closes #1869)
- fix(security): redact JWT Bearer tokens in `redact_sensitive()` — `Authorization: Bearer <token>` headers and standalone JWT strings (`eyJ...`) are now replaced with `[REDACTED]`/`[REDACTED_JWT]` before `compression_failure_pairs` SQLite insert (closes #1847)
- fix(memory): widen soft compaction window — lower `soft_compaction_threshold` default from `0.70` to `0.60`, widening the soft tier firing range from 20% to 30% of the context budget; prevents large tool outputs (10–30k tokens) from jumping directly past soft into hard compaction; add `maybe_soft_compact_mid_iteration()` called after per-tool summarization in native and legacy tool loops so context pressure is relieved without touching turn counters, cooldown, or triggering LLM calls; config validation that `soft < hard` was already enforced and remains in place (closes #1828)
- fix(security): redact secrets and filesystem paths in compression_failure_pairs before SQLite storage (#1801)
- fix(llm): strip URL path in `parse_host_port` — Ollama `base_url` with `/v1` suffix no longer produces 404 on embed calls (#1832)
- Qdrant collection dimension mismatch when switching embedding models on collections with 0 points (#1815)
- fix(debug): trace.json now written inside per-session subdir, preventing overwrites (#1814)
- A-MEM note linking never created `similar_to` edges because `EntityResolver` in `extract_and_store` was constructed without `with_embedding_store()`, leaving `zeph_graph_entities` unpopulated; pass the Qdrant embedding store through to the resolver so entity embeddings are stored and note linking can find semantically similar entities across sessions (#1817)
- graph-memory: entity embeddings now correctly stored in Qdrant — `EntityResolver` was built without a provider in `extract_and_store()`, causing `store_entity_embedding()` to never be called and `zeph_graph_entities` collection to remain empty (fixes #1829)
- fix(core): JIT tool reference injection now works after overflow migration to SQLite — `OVERFLOW_NOTICE_PREFIX` and `extract_overflow_ref()` updated to match the `overflow:{uuid}` format; pruned tool output notices now read `[tool output pruned; use read_overflow {uuid} to retrieve]` instead of a stale file-path reference (closes #1818)

## [0.15.1] - 2026-03-15

### Fixed

- fix(memory): `save_compression_guidelines` now uses a single atomic `INSERT ... SELECT COALESCE(MAX(version), 0) + 1` statement instead of a read-then-write pattern, eliminating the TOCTOU race where two concurrent callers could insert duplicate version numbers; migration 033 adds a `UNIQUE(version)` constraint to the `compression_guidelines` table with row-level deduplication for pre-existing corrupt data (closes #1799)

### Added

- feat(memory,core): ACON failure-driven compression guidelines (#1647) — after a hard compaction, the agent watches subsequent LLM responses for two-signal context-loss indicators (uncertainty phrase + prior-context reference); confirmed failure pairs are stored in SQLite (`compression_failure_pairs`); a background updater wakes periodically, calls the LLM to synthesise updated guidelines from accumulated pairs, sanitizes the output to strip prompt injection, and persists the result; guidelines are injected into every future compaction prompt via a `<compression-guidelines>` block; `CompressionGuidelinesConfig` in `[memory.compression_guidelines]` (disabled by default); addresses all critic findings including two-signal false-positive guard, `enabled` guard ordering, LLM timeout, prompt injection sanitization, field truncation, and cleanup policy
- feat(debug): debug dumps can now emit OpenTelemetry-compatible OTLP JSON traces (`--dump-format trace`); span hierarchy: session → iteration → LLM request / tool call / memory search; `[debug.traces]` config section with `otlp_endpoint`, `service_name`, `redact` options; when `format = "trace"` legacy numbered dump files are NOT written (closes #1343)
- feat(debug): `/dump-format <json|raw|trace>` TUI/CLI command to switch debug dump format at runtime
- feat(cli): `--dump-format <FORMAT>` flag to override debug dump format from the command line
- feat(config): `--init` wizard now prompts for debug dump format when debug dump is enabled
- feat(config): `--migrate-config` auto-populates new `[debug.traces]` section for existing configs
- feat(security): OWASP AI Agent Security 2026 hardening (closes #1650) — three new defenses wired end-to-end:
  - **PiiFilter** (`[security.pii_filter]`): regex-based scrubber (email, phone, SSN, credit card + custom patterns) applied to tool outputs before they enter LLM context and before debug dumps are written; zero-alloc `Cow` fast path; disabled by default (opt-in)
  - **MemoryWriteValidator** (`[security.memory_validation]`): structural checks on `memory_save` content (size cap, forbidden substring patterns) and graph extraction results (entity/edge count caps, entity name length, entity name PII scan); enabled by default with conservative limits
  - **ToolRateLimiter** (`[security.rate_limit]`): sliding-window per-category (shell/web/memory/mcp/other) rate limiter with circuit-breaker cooldown; `check_batch()` atomically reserves slots before parallel tool dispatch to prevent bypass; disabled by default; injects synthetic error `ToolOutput` for blocked calls without interrupting other tools in the tier
- fix(debug): native tool spans in OTLP traces now record `startTimeUnixNano` from the moment the tool was dispatched rather than after it completed; `TracingCollector::begin_tool_call_at` added to support post-hoc span assembly with a pre-recorded `Instant` (closes #1798)
- fix(memory): `edges_created` stat in `link_memory_notes` was inflated when both endpoints of a pair appeared in `entity_ids` — the second normalised `insert_edge(src, tgt)` call returned `Ok` (updating confidence on the existing row), incrementing the counter twice for one physical edge; a `HashSet` of seen `(src, tgt)` pairs now deduplicates within each pass, keeping the stat accurate (closes #1792)
- perf(memory): `link_memory_notes` now embeds all entity texts in parallel via `futures::join_all` instead of N sequential HTTP round-trips, reducing embed latency from O(N) to O(1) round-trips (closes #1793)
- perf(memory): `link_memory_notes` now runs all Qdrant `search_collection` calls in parallel via `futures::join_all`, reducing search latency from O(N) to O(1) round-trips (closes #1794)
- test(memory): add `link_memory_notes_edges_created_not_inflated` — verifies `edges_created == 1` when both endpoints are in `entity_ids`, catching the bidirectional double-count regression (closes #1792)
- test(memory): add `link_memory_notes_secondary_self_skip_guard` — seeds entity A without `qdrant_point_id` in SQLite (primary point-id guard inactive), verifies the secondary `target_id == entity_id` guard prevents self-edges when A appears in its own top-K search results (closes #1790)
- test(memory): add `link_memory_notes_threshold_rejection` — sets `similarity_threshold = 2.0` (above maximum cosine similarity 1.0), verifies zero edges are created, covering the `score < threshold` filter path (closes #1791)
- feat(memory): A-MEM dynamic note linking — fire-and-forget similarity edges on graph write; `NoteLinkingConfig` nested in `[memory.graph.note_linking]`; `link_memory_notes` runs after each successful extraction inside the spawned task, bounded by `timeout_secs`; unidirectional `similar_to` edges (source < target) avoid BFS double-counting; `similarity_threshold` deserialization rejects NaN, Inf, and values outside `[0.0, 1.0]`; disabled by default (closes #1694)

- feat(memory,core): migrate tool overflow storage from filesystem to SQLite (`tool_overflow` table, migration 031); `maybe_summarize_tool_output` now writes to `SqliteStore.save_overflow` instead of disk files; overflow references use opaque `overflow:<uuid>` format (eliminates absolute-path leakage SEC-JIT-03); new `read_overflow` native tool allows LLM to retrieve full content; age-based cleanup via `SqliteStore.cleanup_overflow` on startup; `ON DELETE CASCADE` automatically removes overflow rows when conversation is deleted (closes #1774)
- feat(memory,core): add `/graph history <name>` slash command to display temporal edge history including superseded (expired) facts for a given entity (closes #1693)
- feat(memory): temporal versioning on graph edges (closes #1341) — `edges_at_timestamp()`, `bfs_at_timestamp()`, `edge_history()` on `GraphStore`; optional `at_timestamp` parameter on `graph_recall()` and `SemanticMemory::recall_graph()` for historical graph queries; `valid_from` field on `GraphFact` for recency-aware scoring; `temporal_decay_rate` config knob in `[memory.graph]` (default `0.0`, existing behavior unchanged); migration 030 adds two partial indexes (`idx_graph_edges_src_temporal`, `idx_graph_edges_tgt_temporal`) to accelerate temporal range queries on expired edges

- test(memory): add direct unit tests for `edges_at_timestamp`, `edge_history`, `bfs_at_timestamp` — boundary conditions (valid_from==ts inclusive, valid_to==ts exclusive, open-ended active edges), limit/predicate filtering, BFS traversal blocking on expired edges (closes #1776)
- test(core): add COV-04 unit test for channel-close (`Ok(None)`) → `GraphStatus::Failed` transition in `run_scheduler_loop`; fix implementation to return `Failed` instead of `Canceled` on channel close — channel close is an error condition, not a user-initiated cancel (closes #1614)
- feat(gemini): SSE streaming now handles `functionCall` parts — `StreamChunk::ToolUse` is emitted for tool calls received during Gemini streaming (resolves #1659)
- feat(llm): `cost_tiers` config field for `[llm.router.cascade]` — explicit cheapest-first provider ordering independent of chain order; providers are sorted once at construction time (zero per-request cost); unknown names are silently ignored; empty list is equivalent to `None` (#1724)
- feat(cost): add gpt-5 and gpt-5-mini to default pricing table (closes #1744)
- feat(init): add `hard_compaction_threshold` prompt to `--init` wizard (#1719); prompts for both soft and hard compaction thresholds in sequence with cross-field validation (hard > soft) and `is_finite()` guards
- feat(core): when pruning a tool output that has an overflow file, emit `[tool output pruned; full content at {path}]` instead of clearing the body, preserving the reference across hard compaction, `prune_tool_outputs`, and `prune_stale_tool_outputs` (#1740)
- feat(memory): validate `temporal_decay_rate` in `[memory.graph]` on deserialization — rejects NaN, Inf, negative values, and values outside `[0.0, 10.0]`; invalid configs produce a descriptive error at startup instead of silently producing NaN scores (closes #1777)
- feat(memory): adaptive retrieval dispatch — adds `Episodic` route to `MemoryRoute` enum for time-scoped queries (closes #1629); `HeuristicRouter` now detects temporal cues ("yesterday", "last week", "last night", "tonight", etc.) before relationship patterns (fixes CRIT-01 priority collision); temporal keywords are stripped from the FTS5 query string to prevent BM25 score distortion (fixes CRIT-02); word-boundary checks on single-word tokens like "ago" prevent false positives on words like "Chicago" (fixes MED-01); `resolve_temporal_range()` covers all patterns in `TEMPORAL_PATTERNS` including "last night" and "tonight" (fixes MED-02); `strip_temporal_keywords()` helper is public for use in call sites; `SqliteStore::keyword_search_with_time_range()` adds optional `after`/`before` datetime bounds to FTS5 queries; `resolve_temporal_range` accepts injectable `now: DateTime<Utc>` for deterministic unit tests
- feat(core): hard compaction trajectory-elongation metrics (closes #1739) — `compaction_hard_count` and `compaction_turns_after_hard` added to `MetricsSnapshot`; tracks how many user-message turns elapsed between consecutive hard compaction events; turn counter increments before all early-return guards (exhaustion, server compaction, `compacted_this_turn`) to ensure no turns are silently dropped; last open segment is finalized at `shutdown()` and both fields are logged via `tracing::info!` when at least one hard compaction occurred

### Changed

- perf(llm): `RouterProvider` now stores providers as `Arc<[AnyProvider]>` instead of `Vec<AnyProvider>`; `self.clone()` on every LLM request drops from O(N × provider_size) to O(1) for the providers field across all routing strategies (EMA, Thompson, Cascade) (#1724)
- perf(llm): cascade `chat` and `chat_stream` bypass `ordered_providers()` for the Cascade strategy and pass `&self.providers` slice directly to `cascade_chat`/`cascade_chat_stream`, eliminating an unnecessary `Vec` allocation on the hot path (#1724)
- feat(tui): show `[1M CTX]` badge in the TUI header bar when Claude extended context (`enable_extended_context = true`) is active; also shows `Max context: 1M` in the Resources panel (#1686)
- feat(llm): implement `ClassifierMode::Judge` for cascade routing — calls `summary_model` with a lightweight scoring prompt, parses the 0–10 score and normalises to [0.0, 1.0]; falls back to heuristic on any LLM error; warns at startup when judge mode is configured without `summary_model` (#1723)
- feat(llm): `--extended-context` CLI flag enables Claude 1M context window for the session; overrides `llm.cloud.enable_extended_context` from config and emits a cost warning (tokens above 200K use long-context pricing) (#1685)
- test(llm): add `build_request` integration test for extended context enabled path, asserting `anthropic-beta` header contains `context-1m-2025-08-07` (#1687)

### Changed

- perf(tools): cache leaf string values extracted from each tool call's input JSON in `ToolCallDag`; expose via `string_values_for(idx)` and reuse in `native.rs` tier dispatch to eliminate the redundant `extract_string_values` traversal (closes #1714)
- refactor(mcp,core): extract the 17 injection-detection regexes into `zeph_mcp::sanitize::RAW_INJECTION_PATTERNS` (`pub const`); `zeph-core`'s `ContentSanitizer` now compiles its `INJECTION_PATTERNS` from this single shared slice instead of maintaining a duplicate list — any future pattern change is automatically reflected in both sanitization layers. Also fixes two patterns in `zeph-core` that were missing the `(?i)` case-insensitive flag (`xml_tag_injection`, `markdown_image_exfil`) which existed in the `zeph-mcp` copy but had drifted out (closes #1747)
- `zeph-core`: replace `anyhow` with typed `thiserror` errors in `subagent/` and `config_watcher.rs`; remove `anyhow` dependency from `zeph-core`
- refactor(core): split `config/types.rs` (3331 lines) into domain modules — `agent`, `channels`, `defaults`, `features`, `logging`, `memory`, `providers`, `security`, `ui`, `mod` (Config struct + re-exports), and `tests`; no API changes, TOML format unchanged (#1735)
- refactor(memory): split `semantic.rs` (3335 lines) into sub-modules — `mod` (struct + constructors + accessors), `recall`, `summarization`, `cross_session`, `corrections`, `graph`, and `tests`; public API unchanged (#1736)
- Box large `LoopbackEvent` variants (`ToolStart`, `ToolOutput`) to reduce enum size on the stack; extracted `ToolStartData` and `ToolOutputData` structs with public fields (#1737)
- Replace `async-trait` with native async traits in `zeph-tools` search backends (`SemanticSearchBackend`, `LspSearchBackend`); removed `async-trait` dependency from `zeph-tools` (#1733)

### Removed

- **breaking**: `OverflowConfig.dir` field removed from `[tools.overflow]` config; old configs with `dir = "..."` are silently ignored (unknown field) — no migration needed (closes #1774)
- **breaking**: `zeph_tools::save_overflow` and `zeph_tools::cleanup_overflow_files` removed from public API; replaced by `SqliteStore::save_overflow` and `SqliteStore::cleanup_overflow` (closes #1774)
- Filesystem-based overflow storage (`crates/zeph-tools/src/overflow.rs`) removed; existing `~/.zeph/data/tool-output/` files are not migrated and can be manually deleted (closes #1774)

### Security

- **MCP tool-poisoning injection defense** (closes #1691): `zeph-mcp` now sanitizes all tool definition text fields at registration time before they reach the LLM context. New `sanitize` module applies 17 injection-detection regexes (covering system-prompt override, role injection, jailbreak phrases, data exfiltration, URL execution, and XML/HTML tag escape) plus a Unicode Cf-category strip pass to `tool.description`, `tool.name`, `tool.server_id`, and all string values in `input_schema` (recursively, depth-capped at 10). Fields triggering a pattern are replaced wholesale with `"[sanitized]"` and a structured `WARN` log is emitted. Descriptions are capped at 1024 bytes. Tool registration is never blocked — only text is cleaned. Sanitization runs in both `connect_all()` and `add_server()` paths immediately after `list_tools()` returns.
- **MCP `tools/list_changed` refresh path now sanitizes tool definitions** (closes #1746): MCP servers can push updated tool lists at runtime via the `tools/list_changed` notification. This refresh path previously bypassed `sanitize_tools()`, allowing a malicious server to inject prompt payloads after initial connection. `ToolListChangedHandler` now intercepts notifications and applies the same sanitization pipeline (rate-limited to once per 5 s per server, capped at 100 tools before sanitization) before storing or broadcasting the refreshed list. The agent polls a `watch::Receiver<Vec<McpTool>>` at the start of each turn to pick up updates atomically.

### Refactor

- refactor: eliminate all `#[allow(clippy::too_many_lines)]` suppressions workspace-wide (#1734); extract helper functions from `loopback_event_to_updates`, `prompt`, `new_session`, `load_session`, `fork_session`, `resume_session`, `set_session_config_option` in `zeph-acp`, and `push_event` in `zeph-tui`; zero behavior change

### Fixed

- fix(memory): add `edge_history_limit` config field to `[memory.graph]` (default 100); `GraphStore::edge_history()` already accepted a `limit` parameter but callers had no config-driven default — future TUI/API call sites must read `config.memory.graph.edge_history_limit` instead of hardcoding a value (closes #1778)
- fix(llm): `cascade_chat` and `cascade_chat_stream` no longer store an empty-string provider response as `best_seen`; a provider returning `""` is now skipped for best-seen tracking so the caller receives an explicit error instead of a silent empty response on all-fail fallback (#1754)
- fix(tui): skip ACP stdio/both autostart when `--tui` is active; stdio and TUI are mutually exclusive (both own stdin/stdout); HTTP transport is still allowed alongside TUI when `acp-http` feature is enabled (#1729)
- fix(mcp): suppress MCP child process stderr in TUI mode to prevent ratatui display corruption; `McpManager` gains `with_suppress_stderr` builder method (#1729)
- fix(llm): `cascade_chat_stream` now tracks best-seen response across early providers (#1722); on token budget exhaustion with a would-escalate response the highest-scoring prior response is returned; when the last provider fails and an early provider succeeded, the best-seen response is returned instead of propagating the error — achieving parity with `cascade_chat`
- fix(llm): `cascade_chat` and `cascade_chat_stream` now return the best-seen response when `escalations_remaining == 0` and the current response would have triggered escalation, matching the existing budget-exhaustion behaviour and closing the parity gap with `best_seen` tracking (#1755)

## [0.15.0] - 2026-03-14

### Changed

- **Tiered context compaction** (#1338): replaced single `compaction_threshold` (0.80) with
  two-tier compaction. Soft tier (`soft_compaction_threshold`, default 0.70) prunes tool outputs
  and applies deferred summaries without LLM calls. Hard tier (`hard_compaction_threshold`,
  default 0.90) triggers full LLM-based summarization. Old config field `compaction_threshold`
  is still accepted via serde alias and maps to `hard_compaction_threshold`.
  `deferred_apply_threshold` is removed — absorbed into the soft compaction tier.

### Fixed

- Context compaction loop when budget too tight: added cooldown guard (`compaction_cooldown_turns`, default 2), counterproductive summary guard (marks exhausted when net freed tokens is zero — summary consumed all freed space), exhaustion guard (marks exhausted when context remains above threshold after compaction — further attempts unlikely to help), and user-visible warning when compaction is exhausted (#1708)
- **`ContextManagement` top-level `type` field removed** (closes #1715): the `ContextManagement` struct no longer serializes a `"type": "auto_truncate"` discriminator at the top level. The Claude API rejects requests with `context_management.type: Extra inputs are not permitted` — the correct format contains only `trigger` and `pause_after_compaction`. `--server-compaction` was still non-functional after PR #1709 due to this field.
- llm: `with_server_compaction(true)` on Haiku models now emits a `WARN` and keeps the flag disabled — the `compact-2026-01-12` beta is not supported for Haiku
- llm: extend `is_compact_beta_rejection()` to catch `invalid_request_error` 400s mentioning `context_management` (fixes #1706)
- **`ContextManagement` serialization for Claude server compaction API** (closes #1705): `ContextManagement` struct now serializes to `{ "type": "auto_truncate", "trigger": { "type": "input_tokens", "value": N }, "pause_after_compaction": false }` matching the Claude API spec. Previously serialized as `{ "type": "enabled", "trigger_tokens": N }` which caused a `400 invalid_request_error: context_management.type: Extra inputs are not permitted`, making `--server-compaction` completely non-functional.

- **Skill embedding log noise** (#1387): `SkillMatcher::new()` no longer emits one `WARN` per skill when the provider does not support embeddings. All `EmbedUnsupported` errors are now collected and summarised into a single info-level log message (e.g. `skill embeddings skipped: embedding not supported by claude (14 skills affected)`). Timeout and other per-skill errors are still logged individually.
- **Graceful degradation when `compact-2026-01-12` beta header is rejected** (closes #1698, SEC-COMPACT-03): `ClaudeProvider` now detects 400 responses caused by a rejected beta header (`unknown beta`, `invalid beta`, or explicit `compact-2026-01-12` mention). On detection: the `server_compaction_rejected` flag (shared `Arc<AtomicBool>`) is set, future requests omit the header and `context_management` field, a `WARN`-level log is emitted, and `LlmError::BetaHeaderRejected` is returned. The native tool-use retry loop (`call_chat_with_tools_retry`) catches this error, disables `server_compaction_active` on the agent, and retries the turn with client-side compaction — meaning the user loses at most one turn rather than entering a hard error loop. The `Arc` ensures all `ClaudeProvider` clones (e.g. router replicas) observe the rejection immediately.
- Orchestration: count tasks completed before cancellation in `tasks_completed` metric (fixes #1612)
- Cancel running sub-agents on channel close and shutdown signal in `run_scheduler_loop()` ([#1613](https://github.com/bug-ops/zeph/issues/1613))
- ACP: `session/prompt` no longer hangs indefinitely for slash commands that bypass LLM calls (`/graph`, `/status`, `/plan list`, `/skills`, `/compact`, etc.); `flush_chunks()` is now called after every non-LLM slash command branch in `process_user_message()` and `handle_image_command()`, ensuring the drain loop always receives a termination signal (fixes #1683)
- ACP: agent-loop slash commands (`/plan`, `/graph`, `/status`, `/skills`, `/scheduler`, `/compact`, etc.) now correctly forwarded to the agent loop instead of returning "unknown command" errors (fixes #1672)
- Fix anomaly detector not recording outcomes for native tool_use providers (Claude, OpenAI, Gemini) (#1677)
- OpenAI: tools with no parameters (empty struct schemas) no longer cause `400 Bad Request` in strict mode; `parameters` field is omitted for no-param tools, matching the Gemini provider behavior (fixes #1673)

### Changed

- `zeph-core`: parallel tool dispatch now respects intra-turn `tool_use_id` dependencies — independent calls execute concurrently, dependent calls execute in topological tiers (closes #1646). A lightweight `ToolCallDag` (Kahn's algorithm) partitions tool calls into parallel tiers; when no dependencies exist the existing `join_all` fast path is used with zero overhead. Dependent calls whose prerequisite failed or requires confirmation receive a synthetic error. Cycle detection falls back to sequential execution of all calls.
- **Claude 3 model ID retirement** (#1625): replaced retired Claude 3 model IDs (`claude-3-opus`, `claude-3`, `claude:claude-3-5-sonnet`) with `claude-sonnet-4-6` in test files. `ClaudeProvider::new()` now emits a `tracing::warn!` when the configured model starts with `claude-3`, alerting users with stale configs before the first API call fails.

### Added

- **Integration test for `ConfirmationRequired` dependency propagation in tiered dispatch** (closes #1713): added `confirmation_propagation_tests` module to `zeph-core` agent tests with two tests — `confirmation_required_propagates_to_dependent_tier` verifies that a tier-1 dependent tool receives a synthetic `ToolResult::Error` containing "Skipped: a prerequisite tool failed or requires confirmation" when the tier-0 prerequisite returns `ConfirmationRequired`; `independent_tool_not_affected_by_confirmation_required` verifies that an independent tool in the same dispatch executes normally.
- **Cascade routing strategy** (closes #1339): new `RouterStrategy::Cascade` in `zeph-llm`. When `strategy = "cascade"` is configured, the router tries providers in chain order (cheapest first) and escalates to the next provider only when the response is classified as degenerate (empty, repetitive, incoherent). The heuristic classifier (`ClassifierMode::Heuristic`, default) detects degenerate outputs only — not semantic failures; `ClassifierMode::Judge` (requires `summary_model`) provides LLM-based quality scoring with automatic fallback to heuristic on failure. Key behaviors: network/API errors do not consume the escalation budget; the best-seen response is returned on exhaustion (not `NoProviders`); `max_cascade_tokens` caps cumulative token cost across escalation levels; cascade is intentionally skipped for `chat_with_tools`; Thompson/EMA outcome tracking is not contaminated by quality-based failures. Config: `[llm.router.cascade]` section with `quality_threshold` (default 0.5), `max_escalations` (default 2), `classifier_mode`, `window_size`, `max_cascade_tokens`.

- **Gemini `thinking_level` / `thinking_budget` support** (closes #1652): `GeminiThinkingLevel` enum (`Minimal/Low/Medium/High`, lowercase serde matching Gemini API) and `GeminiThinkingConfig` struct (`thinkingLevel`, `thinkingBudget`, `includeThoughts`, camelCase per API spec) added to `zeph-llm`. `GeminiProvider` gains builder methods `with_thinking_level()`, `with_thinking_budget()` (fallible — validates -1/0/1..=32768, returns `LlmError` on out-of-range), and `with_include_thoughts()`. `GeminiConfig` in `zeph-core` gains `thinking_level`, `thinking_budget`, and `include_thoughts` optional fields. Thinking config is wired at all three `GeminiProvider` construction sites (primary, orchestrator, router). `--init` wizard adds a `thinking_level` select prompt in the Gemini section. Applies to Gemini 3+ (`thinkingLevel`) and Gemini 2.5 (`thinkingBudget`) models.
- **Async parallel dispatch in `DagScheduler`** (closes #1628): `DagScheduler::tick()` now dispatches all ready tasks in a single tick instead of capping at `max_parallel - running_in_graph`. Concurrency is enforced by `SubAgentManager` which returns `ConcurrencyLimit` when capacity is exceeded; tasks revert to `Ready` and are retried on the next tick. Event buffer guard in `wait_event()` changed from `max_parallel * 2` to `graph.tasks.len() * 2` to prevent dropped completion events during parallel bursts. Added `record_batch_backoff(any_success, any_concurrency_failure)` for batch-aware backoff tracking: the `consecutive_spawn_failures` counter now increments once per all-failed tick rather than once per rejected spawn, preventing incorrect exponential backoff after concurrent rejections from the same batch.

- **Claude server-side context compaction** (`compact-2026-01-12` beta, closes #1626): `ClaudeProvider` gains `server_compaction: bool` and sends `context_management: { type: "enabled", trigger_tokens: N }` in all request bodies when enabled. The `compact-2026-01-12` beta header is appended alongside any existing beta headers. SSE parser is now stateful (`ClaudeSseState`) and accumulates `compaction`-typed content blocks across events, emitting `StreamChunk::Compaction(summary)`. Non-streaming path stores the compaction summary via `take_compaction_summary()` on the trait. Agent loop (both native and legacy streaming paths) handles compaction by pruning old messages and inserting a synthetic `MessagePart::Compaction` assistant turn for round-trip fidelity. Client-side `maybe_compact` and `maybe_proactive_compress` return early when server compaction is active. New metric `server_compaction_events` tracks compaction occurrences. Configurable via `[llm.cloud] server_compaction = true`, `--server-compaction` CLI flag, and `--init` wizard.
- **COV-03 scheduler-loop integration test** (#1611): adds `scheduler_loop_queues_non_cancel_message` to `agent/tests.rs`, verifying end-to-end that a non-cancel message delivered via `channel.recv()` during `run_scheduler_loop` is passed to `enqueue_or_merge()` and appears in `agent.message_queue` after the loop exits. Complements the `enqueue_or_merge` unit tests in `message_queue.rs`.
- **Claude 1M extended context window** (#1649): adds `enable_extended_context: bool` to `CloudLlmConfig` (default `false`). When enabled, `ClaudeProvider` injects `anthropic-beta: context-1m-2025-08-07` into all API requests, unlocking the 1M token context window for Opus 4.6 and Sonnet 4.6. `context_window()` now returns `1_000_000` instead of `200_000` when the flag is set, so the auto-budget correctly scales to 1M. All four Claude construction sites in `bootstrap/provider.rs` wire the flag (summary provider intentionally skips it — summaries are capped at 4096 tokens). `--init` wizard adds a Confirm prompt after the thinking mode question. An INFO log is emitted at provider construction when extended context is active.

- **Gemini SSE TODO for Phase 4 (streaming tool use)**: added a TODO comment in `parse_gemini_sse_event()` documenting that `GeminiStreamPart` lacks a `function_call` field and that `functionCall` SSE chunks are silently dropped. `chat_with_tools()` uses the non-streaming endpoint today, so this is safe; the TODO tracks Phase 4 work (extend `GeminiStreamPart` and handle `functionCall` parts in the SSE loop). Closes #1639.
- **Gemini `uppercase_types` test coverage** (#1636): added unit tests for `number`, `boolean`, `array`, and `null` JSON Schema type names in `crates/zeph-llm/src/gemini.rs`. Previously only `string`, `object`, and `integer` were covered; `array` test also verifies recursive `items.type` uppercasing.
- **Gemini schema conversion edge case tests** (#1637): adds 5 unit tests in `zeph-llm` covering previously untested paths: `oneOf` Option&lt;T&gt; pattern, null-first `anyOf` order, unknown `$ref` fallback (→ `OBJECT`/`"unresolved reference"`), nested multi-level `$ref` chain (A→B→C), and parameterless tools declarations guard. Part of #1592.
- **Router debug logging**: `RouterProvider` now emits `tracing::debug!` on every provider selection — Thompson selections include `alpha`, `beta`, and `mode` (exploit/explore); EMA selections include `latency_ema_ms`. Closes #1388.

- **`/scheduler list` command and `list_tasks` tool**: adds `list_jobs_full()` to `JobStore` returning a new `ScheduledTaskInfo` struct with `name`, `kind`, `task_mode`, `cron_expr`, and `next_run` fields. Adds a `list_tasks` LLM tool to `SchedulerExecutor` (fenced block dispatch, registered in `tool_definitions()`). Adds `/scheduler list` slash command in `zeph-core` (dispatches through `tool_executor.execute_tool_call_erased` — no new cross-crate dependency). `/scheduler` with no subcommand also lists tasks; unknown subcommands show help. `/scheduler` entry added to the help registry, feature-gated on `scheduler`. Closes #1423.
- **5-field cron expression support in scheduler**: `normalize_cron_expr()` now accepts standard 5-field cron expressions (e.g. `*/5 * * * *`) by auto-prepending `0` for the seconds field. All three parse sites (`ScheduledTask::periodic`, `SchedulerExecutor::schedule_periodic`, `load_config_tasks`) and the DB persistence path now use the normalized 6-field form. Closes #1422.

- **Chunked edge loading in community detection**: `detect_communities` now loads edges in configurable chunks (keyset pagination via `WHERE id > ?1 LIMIT ?2`) instead of loading all edges at once, reducing peak memory proportional to chunk size on large graphs. Configurable via `GraphConfig.lpa_edge_chunk_size` (default 10,000); `chunk_size = 0` falls back to the legacy full-stream path. Closes #1259.

- **Gemini provider** (Phase 6): real remote model discovery via `GET /v1beta/models`. `GeminiProvider::list_models_remote()` fetches all available Gemini models, filters to `generateContent`-capable ones (excluding embedding-only models such as `text-embedding-004`), maps to `RemoteModelInfo` (strips `models/` prefix, populates `context_window` from `inputTokenLimit`), and persists via `ModelCache`. `AnyProvider::list_models_remote()` now delegates to the real implementation instead of the hardcoded static list. Authentication uses the existing `x-goog-api-key` header; request is retried via `send_with_retry` for transient 429/503 errors; 401/403 return a specific auth error message. Part of epic #1592, closes #1598.
- **Gemini provider** (Phase 5): `embedContent` endpoint for semantic embeddings. `GeminiConfig` gains an optional `embedding_model` field (e.g. `text-embedding-004`); when set, `supports_embeddings()` returns `true`. `embed()` calls `POST /v1beta/models/{model}:embedContent?key=...` with `taskType: RETRIEVAL_QUERY`. Error handling reuses `parse_gemini_error()` — 429 RESOURCE_EXHAUSTED correctly maps to `LlmError::RateLimited`. Empty string is rejected in `with_embedding_model()`. Configured embedding model appears in `list_models()`. Bootstrap wires `embedding_model` at primary provider creation sites (`create_named_provider`, `create_provider_from_config`). Compatible with the existing Qdrant/SemanticMemory pipeline. Part of epic #1592, closes #1597.
- **Gemini provider** (Phase 4): vision / multimodal input via `inlineData` parts. `MessagePart::Image` is now converted to `{ "inlineData": { "mimeType": "...", "data": "<base64>" } }` parts in `contents[].parts[]`. Multiple images per message and mixed text + image parts in a single message are both supported. `supports_vision()` returns `true` for all Gemini 2.0+ models. Part of epic #1592, closes #1596.
- **Gemini provider** (Phase 3): native tool use / function calling via `tools` + `functionDeclarations`. `supports_tool_use()` returns `true`. `chat_with_tools()` converts `ToolDefinition` to Gemini `functionDeclarations` with a schema normalization pipeline: `$ref`/`$defs` inlining (depth 8), allowlist cleanup (`anyOf`/`oneOf` Option<T> → `nullable`), and type name uppercasing. Tool calls parsed from `functionCall` parts into `ChatResponse::ToolUse` with UUID-generated IDs (Gemini provides none). Tool results sent as `functionResponse` parts in a user message with a name lookup from conversation history. `toolConfig.functionCallingConfig.mode` set to `AUTO`. Empty declarations fall back to regular `chat()`. Part of epic #1592, closes #1595.
- **Gemini provider** (Phase 2): SSE streaming via `streamGenerateContent?alt=sse`. `chat_stream()` now produces `StreamChunk::Content` chunks; Gemini 2.5 thinking parts (`thought: true`) are emitted as `StreamChunk::Thinking`. `supports_streaming()` returns `true`. `GeminiProvider` gains `status_tx: Option<StatusTx>` field with `with_status_tx()`/`set_status_tx()` builders; `AnyProvider::set_status_tx()` now propagates the sender to the Gemini arm. Both streaming and non-streaming paths use `status_tx` for retry notifications. API key stays in the `x-goog-api-key` header (never in URL query params). Part of epic #1592, closes #1594.
- **Gemini provider** (Phase 1): new `GeminiProvider` in `crates/zeph-llm/src/gemini.rs` supporting basic `generateContent` chat via the Google Gemini API. Authentication via `x-goog-api-key` header (not URL query param). System prompt extracted to `systemInstruction` top-level field; assistant role mapped to `"model"`. Consecutive same-role messages merged to satisfy Gemini's strict `user`/`model` alternation requirement. First-message guard: if the first content is a `"model"` turn, a synthetic empty user message is prepended. Configurable `base_url` (default `https://generativelanguage.googleapis.com/v1beta`), `model` (default `gemini-2.0-flash`), and `max_tokens`. JSON serialized once before retry loop. HTTP 429 and 503 retried via shared `send_with_retry()`. `ProviderKind::Gemini`, `GeminiConfig`, and `[llm.gemini]` TOML section added; `ZEPH_GEMINI_API_KEY` vault key supported; `--init` wizard updated. Part of epic #1592, closes #1593.
- **Opus 4.6 effort parameter GA**: `ThinkingCapability` gains `prefers_effort: bool` (true for `claude-opus-4-6`). `build_thinking_param()` now auto-converts `ThinkingConfig::Extended { budget_tokens }` to adaptive thinking with an `effort` level for Opus 4.6 models, emitting a `tracing::warn!` deprecation notice. `budget_to_effort()` maps budget values to `ThinkingEffort` levels (`< 5000` → Low, `< 15000` → Medium, `>= 15000` → High). `build_request()` strips trailing assistant messages for Opus 4.6 with thinking enabled (no-prefill constraint). Closes #1627.

### Fixed

- Fix anomaly detector not recording outcomes for native tool_use providers (Claude, OpenAI, Gemini) (#1677)

- **Gemini omit empty `parameters` for no-parameter tools**: `GeminiFunctionDeclaration.parameters` is now `Option<serde_json::Value>` with `skip_serializing_if`. Tools with no parameters (empty `properties` object or absent `properties` key) emit no `parameters` field in the JSON sent to the Gemini API. Closes #1641.
- **Semantic ranking options not wired**: `build_memory()` in `zeph-core` now calls `.with_ranking_options()` after constructing `SemanticMemory`, wiring temporal decay and MMR settings from `[memory.semantic]` config into the memory instance. Previously both features were silently disabled at runtime regardless of user configuration. Closes #1668.
- **ACP slash command pass-through**: `/scheduler`, `/graph`, and `/plan` commands are now forwarded to the core agent loop instead of returning "unknown command". Extracted `slash_command_pass_through()` helper; added unit test covering all three commands and negative cases. Closes #1658.
- **Memory: HeuristicRouter now routes long natural language queries (≥6 words) to semantic search even when they contain snake_case tool names** (fixes #1661)
- **Graph extraction 400 Bad Request with OpenAI strict mode**: `chat_typed` in `zeph-llm` now normalizes the `schemars`-generated JSON Schema before sending it to OpenAI structured output with `strict: true`. Normalization inlines `$ref`/`$defs` references (depth 8) and adds `additionalProperties: false` plus a complete `required` array on every object schema (depth 16). `Option<T>` fields are preserved via `anyOf` and made required as per OpenAI strict mode rules. Closes #1656.
- **Gemini `inline_refs_inner` depth counter**: the depth limit was decremented on every structural recursion step (object key visit, array element visit), not only on `$ref` resolution. Schemas with 9+ levels of plain nesting (no `$ref`s) would hit the depth-8 cap prematurely and corrupt deeply nested schemas. Fixed by decrementing depth only when resolving a `$ref`, leaving structural recursion depth-neutral. Closes #1638.

- **Gemini `parse_tool_response` single-candidate limitation documented**: added code comment and `debug!` log in `parse_tool_response()` noting that only `candidates[0]` is processed. Zeph never requests `candidateCount > 1`, so this path is unreachable in normal operation. Closes #1640.
- **ACP graph memory extraction silently disabled**: `spawn_acp_agent` in `src/acp.rs` now calls `agent.with_graph_config()` with the `[memory.graph]` config section. Previously the `graph_config` field in `MemoryState` defaulted to `GraphConfig { enabled: false }`, causing `maybe_spawn_graph_extraction()` to return early for every ACP session regardless of user configuration. Closes #1633.
- **ACP anomaly detector and orchestration config not wired**: `spawn_acp_agent` in `src/acp.rs` now calls `agent.with_anomaly_detector()` when `[tools.anomaly] enabled = true` and `agent.with_orchestration_config()` unconditionally — mirroring the `runner.rs` pattern. Previously both `debug_state.anomaly_detector` and `orchestration_config` defaulted to their disabled values, silently disabling tool-output anomaly detection and plan orchestration for all ACP sessions (Zed, Helix, VS Code) regardless of TOML configuration. Closes #1643, #1642.

### Tests

- Added regression test `execute_confirmed_blocked_command_rejected` in `zeph-tools`: asserts that `execute_confirmed()` with a blocklisted command returns `ToolError::Blocked`, covering the code path fixed in #1529 (closes #1530).

### Fixed

- **ACP sessions now receive document RAG and graph memory configuration**: `spawn_acp_agent` was not calling `with_document_config()` or `with_graph_config()`, so `DocumentConfig::default()` (`rag_enabled = false`) and `GraphConfig::default()` (`enabled = false`) were silently applied regardless of TOML settings. Both configs are now propagated through `SharedAgentDeps` and applied to every ACP session, matching the behavior in `runner.rs`. Closes #1634 and #1633.
- `ModelOrchestrator` no longer logs `INFO falling back to default provider` on every request when no router chain is configured (the normal orchestrator path). The message is now `DEBUG` when no chain providers were attempted; `INFO` is kept only when a chain was configured but all providers failed — the genuine fallback case. Closes #1484.
- `DagScheduler::wait_event()` busy-spun at 250ms when `SubAgentManager` was saturated. Replaced the fixed `deferral_backoff` sleep with exponential backoff (250ms → 500ms → 1s → 2s → 4s, capped at 5s) that resets on the first successful spawn. Eliminates log flood and CPU waste when the concurrency limit is reached during plan execution (#1618).
- Prevent `DagScheduler` deadlock when `SubAgentManager` concurrency is exhausted during planning phase (#1619): default `max_concurrent` raised from 1 to 5; `SubAgentManager` now supports slot reservation (`reserve_slots` / `release_reservation`); startup warning when `max_concurrent < max_parallel + 1`
- HTTP 503 (`SERVICE_UNAVAILABLE`) responses are now retried by `send_with_retry()` alongside 429, benefiting all LLM providers (#1593)

### Security

- SEC-001: Replace `DefaultHasher` with a process-scoped `RandomState`-seeded SipHash-1-3 in `tool_args_hash()` to prevent adversarial hash collision bypasses of the repeat-detection window (#1399)
- SEC-002: Replace `SystemTime::now().subsec_nanos()` jitter with `rand::rng().random_range()` in `retry_backoff_ms()` to eliminate predictable retry timing that could be exploited by an adversary (#1400)
- SEC-003: Truncate tool names to 256 bytes at UTF-8 boundaries before storing in the `recent_tool_calls` sliding window to prevent unbounded memory growth from adversarially long names (#1401)
- SEC-004: Add `max_retry_duration_secs` (default 30) wall-clock retry budget to `AgentConfig`; the retry loop in `handle_native_tool_calls()` breaks when the budget is exhausted even if attempts remain, preventing indefinite retry loops (#1402)

### Fixed

- `/plan cancel` is now delivered during active plan execution: `run_scheduler_loop()` polls `channel.recv()` concurrently with `scheduler.wait_event()` via `tokio::select!`. Receiving `/plan cancel` calls `cancel_all()`, processes the returned `Cancel` actions to abort sub-agent tasks, and exits the loop with `GraphStatus::Canceled`. Non-cancel messages received during execution are queued for processing after plan completion. Fixes #1603.

- `search_code` tool now displays relative file paths in CLI output, preventing path sanitizer from replacing them with `[PATH]` when `redact_secrets = true`
- Scheduler not initialized in ACP mode — tick loop and scheduler tool now available in ACP sessions (#1599)
- `TrustGateExecutor::check_trust()` was calling `policy.check()` for all tool IDs in Supervised mode, causing `ConfirmationRequired` for any MCP/LSP tool without explicit permission rules (since `PermissionPolicy::from_legacy()` only populates rules for `"bash"`). Non-system tools without explicit rules now return `Allow` by default; system tools (`bash`) always go through `policy.check()` via the new `POLICY_ENFORCED_TOOLS` constant. Fixes #1544.
- `process_response_native_tools()` did not handle `ToolError::ConfirmationRequired` — the error was collapsed into `[error] command requires confirmation: …` and returned to the LLM. The native tool execution loop now calls `channel.confirm()` on `ConfirmationRequired`, re-executes via `execute_tool_call_confirmed_erased()` on approval, and returns `[cancelled by user]` (not an error) on denial. Added `execute_tool_call_confirmed` to `ToolExecutor` and `ErasedToolExecutor` traits; `TrustGateExecutor` overrides it to bypass the permission check while still enforcing `Blocked`/`Quarantined` trust levels. Fixes #1545.

- `McpLspProvider` was sending `"uri"` as the parameter key to all mcpls tool calls, but mcpls 0.3.4 expects `"file_path"`. All six methods (`hover`, `definition`, `references`, `diagnostics`, `document_symbols`, `code_actions`) are fixed. `code_actions` additionally now sends flat `start_line`/`start_character`/`end_line`/`end_character` fields instead of a nested `range` object, matching the mcpls `get_code_actions` schema. Fixes #1533.
- `--init` wizard generated unsupported `--workspace-root` flag for mcpls. The wizard now writes `.zeph/mcpls.toml` (with workspace roots, language extensions, and rust-analyzer LSP server config) and passes `--config .zeph/mcpls.toml` to mcpls instead. Fixes broken LSP setup for all users who configured mcpls via `zeph init`. (#1534)
- Update `deny.toml` suppression comment for RUSTSEC-2025-0134 (`rustls-pemfile` unmaintained) to reference upstream tracking issue qdrant/rust-client#255 (tonic 0.14 upgrade that removes the dependency); no code change possible until upstream ships a release.
- Shell command blocklist (`blocked_commands`, `DEFAULT_BLOCKED`, `allow_network = false`) was silently skipped whenever a `PermissionPolicy` was attached to `ShellExecutor` (i.e., in all normal operation with `autonomy_level` set). `find_blocked_command()` now runs unconditionally before the policy check, making it a hard security boundary that cannot be bypassed by any autonomy level or permission policy configuration.
- OpenAI: assistant tool-call messages with `null` content are now accepted; `ChatResponse::ToolUse` carries `text: None` for tool-only assistant turns instead of failing deserialization (#1561, #1562)
- GPT-5 OpenAI requests now use `max_completion_tokens` instead of deprecated `max_tokens`; non-GPT-5 models retain `max_tokens` (#1558, #1559)
- Claude `cache_control` blocks capped to 4 per request: new helpers limit markers across tools, system blocks, and messages before each request is built, preventing HTTP 400 from the Anthropic API when tool-call sequences accumulate more than 4 markers (#1570, #1572)
- ACP tool-use prompt no longer leaks the literal `[PATH]` placeholder into bash commands during diagnosis sessions (#1569, #1571)
- SQLite `database is locked` errors on concurrent skill-outcome writes resolved by adding `busy_timeout` and per-call retry in the skill recorder (#1563, #1564)
- ACP session now uses the `cwd` provided at session creation for project discovery, environment context assembly, and prompt construction (#1567)
- `apply_code_index()` now starts the tree-sitter `CodeIndexer` and `IndexWatcher` for all providers; Qdrant semantic retrieval is only skipped for native-tool-use providers (Claude, OpenAI), making structural index available to all configurations (#1557, #1589)
- Config default annotations normalized; legacy runtime paths (logs, skills, debug, SQLite) are rewritten to computed user-data defaults in the wizard and `--init` flow (#1582)
- `ZephAcpAgent` and its diagnostics cache refactored to `Send + Sync` via `Arc<RwLock<_>>`; ACP stdio sessions no longer require `LocalSet` and fully utilize the tokio thread pool (#1577, #1587)
- `warm_model_caches()` no longer blocks ACP server startup; model warming is dispatched as a background task and the shared model list is stored in a `RwLock` for multi-session consistency (#1576, #1583)
- ACP `[acp] enabled = true` in config now auto-starts the server without requiring `--acp` CLI flag; `--acp` and `--acp-http` remain functional and bypass the config field (#1574, #1590)
- `apply_code_index()` now starts `CodeIndexer` and `IndexWatcher` for native-tool-use providers so the tree-sitter index is available to the `search_code` tool regardless of provider type (#1556, #1591)

### Added

- **#1515**: Add `SubAgentError::ConcurrencyLimit { active: usize, max: usize }` variant to replace the fragile `Spawn(String)` concurrency message. `record_spawn_failure()` now accepts `&SubAgentError` and uses a typed `matches!` check instead of string matching. Both `spawn()` and `resume()` in `SubAgentManager` emit the new variant. Callers pass `&e` instead of `&e.to_string()`.
- **#1516**: Add three edge-case tests for `DagScheduler` concurrency-deferral: running task is unaffected when a concurrent task defers (`test_concurrency_deferral_does_not_affect_running_task`), `max_parallel=0` stalls the scheduler without triggering deadlock detection (`test_max_concurrent_zero_no_infinite_loop`), and all tasks deferring with `ConcurrencyLimit` keep the graph in `Running` and retry on the next tick (`test_all_tasks_deferred_graph_stays_running`).
- **#1457**: Add `plan_cancel_token: Option<CancellationToken>` to `Agent`. A fresh token is created in `handle_plan_confirm()` and passed into `run_scheduler_loop()`. The tick loop adds a `tokio::select!` branch on `cancel_token.cancelled()` at `wait_event()` (calls `cancel_all()` and breaks) and wraps `RunInline` execution so it can be interrupted. `handle_plan_cancel()` fires the token if a plan is in flight. `plan_cancel_token` is always cleared in both `Ok` and `Err` paths to prevent stale-token bugs. **Known limitation**: the delivery path for `/plan cancel` during active execution requires restructuring the agent message loop (#1603, SEC-M34-002; currently only reachable from concurrent-reader channels such as Telegram).

- **#1551**: Remove the `index` feature flag — `zeph-index` and `tree-sitter` are now always-on base dependencies. All `#[cfg(feature = "index")]` guards are removed from `zeph-core`, `zeph` binary, and `lsp_hooks/hover.rs`. The `index` entry is removed from root `Cargo.toml` `[features]` and `full` feature list, and from `zeph-core/Cargo.toml`. Tree-sitter and code index functionality is always compiled; no feature gating required.
- **#1554**: Decouple repo map injection from Qdrant retriever. `IndexState` now populates `repo_map_tokens`/`repo_map_ttl` independently via `AgentBuilder::with_repo_map()`. The repo map is injected into the system prompt whenever `repo_map_tokens > 0`, regardless of whether a Qdrant-backed `CodeRetriever` is available. Semantic code RAG via Qdrant is unaffected and still requires the retriever. The `apply_code_index()` bootstrap function now configures repo map for all providers (including Claude/OpenAI with native `tool_use`), then skips only the Qdrant retriever setup for tool-use providers. `apply_config()` hot-reload now correctly refreshes both `repo_map_tokens` and `repo_map_ttl`. Fixes silent repo map omission for the most common provider configurations.
- **#1552**: Replace heuristic AST walking in `generate_repo_map()` with tree-sitter ts-query extraction. New public types in `zeph-index`: `SymbolInfo`, `SymbolKind`, `Visibility`, and `extract_symbols()`. `Lang::symbol_query()` and `Lang::method_query()` provide lazily-compiled `LazyLock<Query>` per language (Rust, Python, JS, TS, Go). Visibility is parsed from `visibility_modifier` node text: `pub`→Public, `pub(crate)`→Crate, `pub(super|in …)`→Restricted, absent→Private. Query compilation failures log a warning and return `None` (no panics); heuristic extraction serves as fallback. Repo map output now includes visibility and 1-based line numbers per symbol (e.g. `pub fn:hello(1)`, `impl:Foo(5){pub fn:bar}`). Token budget behaviour is preserved with the new format. `zeph-index::languages` is now a public module.
- **#1553**: Replace regex-based hover pre-filter in `lsp_hooks/hover.rs` with tree-sitter extraction. New `extract_symbol_positions_tsquery()` uses `Lang::symbol_query()` to capture definition node positions at any AST depth (not top-level-only), supporting all languages with grammars. `strip_cat_n_prefix()` strips `cat -n` line number prefixes before parsing, producing a clean source string and a line-number mapping for correct LSP position translation. The `.rs`-only file extension check is removed; language detection via `detect_language()` handles all supported languages. The regex fallback (`extract_symbol_positions_regex()`) is preserved for when tree-sitter cannot parse the file (unknown language or grammar unavailable).

- `zeph migrate-config [--config PATH] [--in-place] [--diff]` command: reads an existing user config, adds all missing parameters as commented-out blocks with descriptions from the canonical reference, and reformats the file by grouping and sorting keys within each section. Existing values are never modified. Running the command twice produces identical output (idempotent). The `--init` wizard now shows a tip about this command.
- `search_code` native tool: unified semantic vector (Qdrant) + structural tree-sitter + LSP symbol/reference resolution in a single agent-callable tool; returns ranked, deduplicated results across all three layers (#1551/#1556, #1591)
- Request metadata (model, token limit, exposed tools, temperature, cache breakpoints) included in debug dumps for both `json` and `raw` formats; `LlmProvider::debug_request_json()` added with provider-specific implementations for Claude, OpenAI, and Ollama; wrapper providers (Router, Orchestrator, Compatible) delegate to the inner provider (#1485, #1560)
- ACP readiness probes: `/health` HTTP endpoint returns `200 OK` when ready and `503` during startup; stdio transport emits `zeph/ready` JSON-RPC notification as the first outbound packet; ready metadata included in the ACP manifest (#1578, #1585)
- MCP server liveness check: `McpLspProvider::is_available()` is now gated on `McpServerManager`'s live-server set via new `is_server_connected()` helper; availability state is updated on client registration and removal (#1586)
- Broadcast channel capacity is now configurable to prevent silent event drops under load; fixes `broadcast_to_mpsc` lagged-receiver silent-drop regression (#1579, #1584)
- ACP startup diagnostic logging: process `cwd` and resolved artifact paths (data, debug, skills, logs) are logged before memory and bootstrap initialization to aid diagnosing read-only filesystem errors in IDE-launched sessions (#1580)
- LSP hook debug tracing: `LspHookRunner::after_tool()`, `fetch_hover()`, and `fetch_diagnostics()` now emit `tracing` events for hook activation, skip reasons, symbol extraction, and MCP call attempts, making hook failures diagnosable from logs without source inspection (#1536, #1588)
- **#1538**: Add `McpCaller` trait to `zeph-mcp` to abstract `McpManager` for unit testing; `MockMcpCaller` stub (feature `mock`) provides configurable FIFO responses and call recording. `fetch_diagnostics` and `fetch_hover_inner` now accept `&impl McpCaller`; 4 regression tests verify `file_path` (not `path`) is passed to `call_tool` for `get_diagnostics` and `get_hover`.

### Changed

- Share a single `QdrantOps` instance (one gRPC channel) across all subsystems at startup: `AppBuilder::new()` constructs `QdrantOps` once when `vector_backend = "qdrant"` and propagates it via clone (O(1) `Arc` bump) to `SemanticMemory`, `QdrantSkillMatcher`, `McpToolRegistry`, and `CodeStore`. Previously 4+ independent gRPC channels were created. Invalid `qdrant_url` when `vector_backend = "qdrant"` is now a hard startup error instead of a silent `None`. URL-based constructors (`QdrantSkillMatcher::new`, `McpToolRegistry::new`, `CodeStore::new`) are replaced by `::with_ops(ops)` variants. (#1337)
- Consolidate `is_private_ip` (SSRF IP check) into `zeph-tools::net::is_private_ip` (canonical superset with CGNAT `100.64.0.0/10`); update `zeph-mcp`, `zeph-acp`, `zeph-tools/scrape` to use it; upgrade A2A's own copy with CGNAT range (DEDUP-01)
- Consolidate `cosine_similarity` into `zeph-memory::math::cosine_similarity` (single-pass loop, length guard); update all callers in `zeph-memory` and `zeph-skills` (DEDUP-02)
- Restore parallel tool execution: `handle_native_tool_calls()` now runs all independent tool calls concurrently via `join_all` bounded by `max_parallel_tools` semaphore (previously serialized by PR #1340). Phase 2 retries only transient failures on executors that explicitly opt in (`WebScrapeExecutor`); `ShellExecutor` is never retried. Self-reflection early-return paths emit actual parallel results instead of synthetic `[skipped]` messages. Fixes PERF-1 (#1403)
- Add `text::truncate_chars(&str, usize) -> &str` to `zeph-core::text`; replace `context/mod.rs::truncate_chars` with a re-export of the canonical version (DEDUP-03)
- Split all four `#[cfg(test)]` blocks from `agent/mod.rs` (~3190 lines) into `agent/tests.rs`; reduce `agent/mod.rs` from 6282 to ~3096 lines (SPLIT-01)
- Split `zeph-acp/agent.rs` into `agent/mod.rs` (2137 lines), `agent/helpers.rs` (547 lines helpers), `agent/tests.rs` (3396 lines tests); reduce main impl file from 6097 to 2137 lines (SPLIT-02)
- Update insta snapshot `config_default_snapshot` to reflect removal of deprecated `[lsp]` config section
- Split `agent/tool_execution.rs` (5426 lines) into `tool_execution/mod.rs`, `tool_execution/legacy.rs`, `tool_execution/native.rs` for improved navigability (ARCH-06)
- Split `agent/context.rs` (5590 lines) into `context/mod.rs`, `context/assembly.rs`, `context/summarization.rs` for improved navigability (ARCH-07)
- Replace 11-parameter `Channel::send_tool_output` signature with `ToolOutputEvent` struct; replace 4-parameter `send_tool_start` with `ToolStartEvent` struct (ARCH-02)
- Extract `SecurityState` struct (sanitizer, quarantine_summarizer, exfiltration_guard, flagged_urls) and `DebugState` struct (debug_dumper, dump_format, anomaly_detector, logging_config) from `Agent` struct; access via `agent.security.*` and `agent.debug_state.*` (ARCH-01)
- Expand `AgentError` with `Shutdown`, `ContextExhausted`, `ToolTimeout`, `SchemaValidation` variants; change `Agent::run` return type from `anyhow::Result<()>` to `Result<(), AgentError>` (ARCH-10)
- Add `AgentTestHarness` builder struct with `new()`, `with_responses()`, `with_registry()`, `with_tool_outputs()`, and `build()` to the test module for cleaner agent unit tests (ARCH-08)

### CI / Docs

- Add weekly external link check via lychee scheduled workflow (Mondays 06:00 UTC); lychee cache and 3-retry resilience enabled for spec sites and auth-gated GitHub URLs
- Add `docs/src/concepts/lsp-context-injection.md` concept page with feature overview, hook table, and enable instructions; fix broken README link
- Add 9 specialized Rust agent profiles (`.zeph/agents/`) and `rust-agent-handoff` skill (`.zeph/skills/`) for multi-agent workflow coordination

## [0.14.3] - 2026-03-10

### Fixed

- DagScheduler: add 250ms backoff in `wait_event()` when all ready tasks are deferred due to concurrency limit, preventing CPU spin-loop (#1519)
- DagScheduler: downgrade concurrency deferral log from INFO to DEBUG (#1519)
- `handle_native_tool_calls()` now pushes `ToolResult` parts for all tool calls in the batch before the self-reflection early return. Previously, when a tool failed and `attempt_self_reflection()` returned `true`, the function exited without emitting any `ToolResult` messages, leaving every `ToolUse` in the assistant message orphaned. Orphaned `ToolUse` blocks caused Claude API 400 errors on subsequent requests and generated spurious WARN logs on every API call. The fix also emits synthetic `[skipped: prior tool failed]` error results for any remaining unexecuted tools in the batch so the invariant "every `ToolUse` has a matching `ToolResult`" is maintained (#1512)
- `DagScheduler::record_spawn_failure` now detects transient concurrency-limit rejections (error contains `"concurrency limit"`) and reverts the task to `TaskStatus::Ready` instead of marking it `Failed`. This prevents spurious graph failure cascades when `SubAgentManager` refuses a spawn because all concurrency slots are occupied by other agents (#1513)
- ACP stdio transport: tracing subscriber now explicitly writes to stderr via `.with_writer(std::io::stderr)`, preventing WARN/ERROR log lines from polluting stdout and breaking NDJSON parsing in IDE clients (Zed, VS Code, Helix) (#1503)
- `persist_message` now receives the correct `has_injection_flags` value derived from `sanitize_tool_output` injection pattern detection, not just URL extraction. Pure text injections (without URLs) now correctly activate `guard_memory_writes` in both legacy and native tool paths (#1491)
- `handle_native_tool_calls()` now routes tool output through `sanitize_tool_output()` before placing it in `MessagePart::ToolResult`. Previously, the native tool-use path (Claude provider) bypassed `ContentSanitizer` entirely: injection detection, exfiltration URL extraction, quarantine summarizer, and security metrics were all silently skipped. `flagged_urls` was never populated, so `validate_tool_call()` and memory-write guarding (`persist_message`) were also effectively disabled for this path (#1490)
- `ShellExecutor` blocklist now detects blocked commands wrapped in backtick substitution (`` `cmd` ``), `$(cmd)`, `<(cmd)`, and `>(cmd)` process substitution. Previously these constructs bypassed `find_blocked_command` because the subshell prefix was attached to the command token during tokenization. `SUBSHELL_METACHARS` in `check_blocklist` extended with `<(` and `>(` (#1483)
- Secret request prompt now truncates `secret_key` to 100 chars (UTF-8 safe) in all confirmation dialogs; input validation in the sub-agent loop rejects keys longer than 100 chars at the source (#1480)
- Delegate `supports_vision()`, `last_usage()`, and `supports_structured_output()` in `SubProvider` to prevent silent capability misreporting when the orchestrator wraps a Claude or OpenAI provider (#1497)
- Delegate `context_window()` in `SubProvider` to fix silent `auto_budget`, semantic recall, and graph recall failures when using the orchestrator provider (#1473)
- `/graph facts <name>` now returns the entity whose name exactly matches the query instead of an entity that merely mentions the name in its summary. `find_entity_by_name` uses a two-phase lookup: exact case-insensitive match on `name`/`canonical_name` first, FTS5 prefix search only as fallback (#1472)
- Graph memory: `insert_edge` now deduplicates active edges on `(source_entity_id, target_entity_id, relation)`; re-extraction no longer creates duplicate rows, and confidence is updated to the higher value on repeat extractions (#1471)
- AgentRouter inline fallback: when no sub-agents are configured, `DagScheduler` now emits `SchedulerAction::RunInline` instead of immediately marking the task as `Failed`. The main agent provider executes the task prompt directly, allowing single-agent setups to use `/plan` without any sub-agent definitions (#1463)
- `/plan status` now reflects actual graph state: messages are matched per `GraphStatus` variant (`Created`, `Running`, `Paused`, `Failed`, `Completed`, `Canceled`) instead of always showing "awaiting confirmation" (#1463)
- `/feedback` command now correctly classifies feedback sentiment: positive or neutral feedback is stored as `user_approval` outcome type instead of always using `user_rejection`, preventing self-learning confidence inversion for praised skills
- `generate_improved_skill()` is now skipped for positive feedback, avoiding unnecessary LLM calls and incorrect skill rewrites when a skill is working correctly
- `skill_metrics()` now excludes `user_approval`/`user_rejection` outcomes from execution-based success rate calculations, preventing explicit user feedback from polluting Wilson score metrics
- `extract_fenced_blocks()` in `zeph-tools` now requires a word-boundary after the language tag: `` ```bashrc `` no longer matches when searching for `bash` (#1461)
- Secret request prompt now truncates reason to 200 chars (UTF-8 safe) to prevent oversized confirmation dialogs (#1456)
- Deduplicate secret prompts in orchestration tick loop: after a timeout or user denial, the `(handle_id, secret_key)` pair is recorded in a plan-scoped `HashSet`; subsequent re-requests from the same sub-agent for the same key are auto-denied without re-prompting the user (#1455)
- Completion token count in metrics now uses API-reported `output_tokens` from OpenAI, Ollama, and Compatible providers instead of the `response.len() / 4` byte-length heuristic. Streaming paths retain the heuristic as a fallback when the provider returns no usage data. `chat_typed()` now stores usage so `eval_budget_tokens` enforcement reflects real token counts in structured-output calls ([#1449](https://github.com/bug-ops/zeph/issues/1449))
- `ModelOrchestrator` now implements `last_usage()` and fixes `last_cache_usage()` to delegate to the last-used provider instead of always reading from the default provider; token metrics are now accurate for all orchestrator users ([#1481](https://github.com/bug-ops/zeph/issues/1481))
- Secret requests from sub-agents are now always processed before the plan scheduler terminates, even when the sole task completes on the first tick (instant completion). Previously, `process_pending_secret_requests()` was skipped when `Done` was emitted before the first `wait_event()` (#1454)
- Anomaly detector now classifies `[stderr]` tool output as `AnomalyOutcome::Error`. Previously the condition checked for dead-code pattern `[exit code` (never emitted by `ShellExecutor`), causing all shell stderr output to be silently classified as `Success` (#1453)
- Shell audit logger (`ShellExecutor`) now classifies `[stderr]` output as `AuditResult::Error`, matching the anomaly detector fix (#1453)
- Add `protocolVersion` field to A2A agent card (`/.well-known/agent.json`); value is set to `A2A_PROTOCOL_VERSION` constant (`"0.2.1"`) and emitted by the default `AgentCardBuilder` (#1442)
- MCP HTTP transport: statically configured servers (from `[[mcp.servers]]`) now bypass SSRF validation, allowing connections to `localhost` and other private IPs. Dynamically added servers (`/mcp add`, ACP) retain full SSRF protection (#1441)
- Wire `graph_config` into agent bootstrap: `runner.rs` and `daemon.rs` now call `with_graph_config(config.memory.graph.clone())` at construction time, matching the existing `with_document_config()` pattern. Previously `graph_config.enabled` was always `false` at startup (despite `[memory.graph] enabled = true` in config), causing `maybe_spawn_graph_extraction()` to return immediately and leaving graph extraction, entity resolution, and BFS recall as dead code in production (#1437)
- Wire `DagScheduler` into `/plan confirm` flow — plan tasks now execute via the tick loop before aggregation (#1434)
- `/plan list` now shows the pending plan summary and status label instead of always returning "No recent plans" (#1434)
- `/plan retry` now resets stale `Running` tasks to `Ready` and clears `assigned_agent` before re-execution to prevent scheduler deadlock (#1434)
- Cross-session history restore no longer produces orphaned `tool_use` blocks that cause Claude API 400 errors (#1383): fix empty-content skip dropping tool-only user messages (RC3), add reverse orphan detection for unmatched `tool_result` parts (RC2), downgrade orphaned `ToolResult` blocks in `split_messages_structured` (RC1), filter system messages from visible index to prevent wrong-neighbor lookups (RC4), persist tombstone `ToolResult` on native tool call cancellation to pair already-persisted `ToolUse` (RC5)
- Store token usage in `chat_typed` so `eval_budget_tokens` is enforced with Claude provider (#1426)
- `/experiment status` now shows the last completed session (session ID, experiment count, accepted count, best delta) when an experiment is not running. Previously it always showed "idle" with no history, making scheduled experiment results invisible (#1425)
- `FilteredToolExecutor::execute_erased()` and `execute_confirmed_erased()` previously returned `Err(ToolError::Blocked)` for every LLM response unconditionally, causing sub-agent loops to exhaust all `max_turns` without producing output (#1432)
- The executor now inspects the response for actual fenced-block tool invocations by matching against registered `InvocationHint::FencedBlock` language tags via `extract_fenced_blocks()`
- Plain text responses and markdown code fences that do not match any registered tool tag now return `Ok(None)`, allowing the agent loop to break normally; SEC-03 policy is preserved for genuine fenced-block tool invocations

## [0.14.2] - 2026-03-09

### Fixed

- `/experiment status` now shows the last completed session (session ID, experiment count, accepted count, best delta) when an experiment is not running. Previously it always showed "idle" with no history, making scheduled experiment results invisible (#1425)
- Shell timeouts in `ShellExecutor` now return `Err(ToolError::Timeout { timeout_secs })` instead of `Ok(ToolOutput)` with an error string. Fixes dead `ToolError::Timeout` code path and enables `max_tool_retries` retry-with-backoff for timed-out shell commands (#1420)
- `/model <id>` now validates the provided model name against the cached model list before switching. If the model is not found in a non-empty list, an error is returned with the list of available models. If the model list is unavailable (cold start or provider does not support listing), a warning is shown and the switch proceeds (#1417)
- `/status` command now shows real API call count, token usage, and cost in CLI mode (non-TUI). `MetricsCollector` watch channel is always initialized in `runner.rs`; in CLI mode the receiver is dropped immediately, in TUI mode it flows to the TUI widget as before (#1415)
- Register SIGTERM handler (`tokio::signal::unix::SignalKind::terminate()`) alongside the existing Ctrl-C handler in the daemon signal task. Both signals now trigger graceful shutdown, ensuring `remove_pid_file()` is always reached on `kill <pid>` (#1414)
- Correct A2A agent card discovery endpoint from `/.well-known/agent-card.json` to `/.well-known/agent.json` per A2A spec (#1412)
- Wire `GraphStore` in the production bootstrap path: `build_memory()` now calls `with_graph_store()` when `[memory.graph] enabled = true`, making all 5 `/graph` slash commands and graph-based BFS recall functional (#1410)
- Experiment engine `SearchSpace` default temperature range capped at `1.0` (was `2.0`); values above `1.0` are rejected by Claude and OpenAI APIs. `ParameterRange::quantize()` now rounds to 2 decimal places to eliminate floating-point accumulation artifacts (e.g. `0.30000000000000004`) (#1408)
- Experiment engine now applies generation parameter variations (temperature, top_p, etc.) to the subject provider before evaluation, fixing all-zero delta scores (#1407). `AnyProvider::with_generation_overrides` clones and patches the provider; each variation is scored with its specific parameters rather than the unmodified baseline provider. `GenerationOverrides` moved to `zeph_llm::provider` and re-exported from `zeph_core::experiments::snapshot` for backwards compatibility.
- Sub-agent transcript sweep no longer logs a spurious `transcript sweep failed` warning on first run when the transcript directory does not exist yet; the directory is now created automatically (#1397)

### Performance

- Parallelize LLM summarization calls across communities in `detect_communities` using `tokio::task::JoinSet` bounded by `Arc<Semaphore>`. New `GraphConfig.community_summary_concurrency` field (default: 4) controls the concurrency limit; `concurrency=1` provides sequential fallback (#1260)
- Incremental community detection: store BLAKE3 fingerprint (sorted entity IDs + intra-community edge IDs) per community in `graph_communities`. On refresh, only re-summarize communities whose membership changed; unchanged partitions skip LLM calls entirely. Adds migration 028 (`fingerprint TEXT` column). Second refresh with no graph changes triggers 0 LLM calls (#1262)

### Added

- Add `ErrorKind::{Transient, Permanent}` enum to `zeph-tools` and `ToolError::kind()` method for typed error classification. `Execution(io::Error)` is sub-classified by `io::ErrorKind`: transient variants (`TimedOut`, `WouldBlock`, `Interrupted`, `ConnectionReset`, `ConnectionAborted`, `BrokenPipe`) are retryable; `NotFound`, `PermissionDenied`, `AlreadyExists`, and all others are permanent (#1340)
- Add retry logic with exponential backoff for transient tool errors in the native `tool_use` path. Default: 2 retries, 500ms base delay, 5s cap, ~12.5% jitter. Configurable via `[agent] max_tool_retries` (default 2, max 5). Backoff sleep uses `tokio::select!` for cancellation-aware waiting. Debug dumps include `dump_tool_error()` with error kind (#1340)
- Add repeat-detection heuristic in `ToolOrchestrator`: tracks recent LLM-initiated tool calls in a sliding window (`VecDeque`); aborts with an error message when the same tool+args hash appears `>= tool_repeat_threshold` times within `2 * threshold` calls. Retry re-executions are excluded from the window. Configurable via `[agent] tool_repeat_threshold` (default 2, 0 to disable) (#1340)
- Rewrite all 19 native and ACP `ToolDefinition` descriptions to contract format with `Parameters / Returns / Errors / Example` sections for improved tool selection accuracy, especially on smaller local models (#1342)

### Changed

- Tool execution in native `tool_use` path is now sequential per call (previously parallel `join_all`). This enables per-call retry state without additional abstractions. Behavioral equivalence is preserved for the common case; parallel execution restoration is tracked in a follow-up issue (#1340)
- Validate `deferred_apply_threshold < compaction_threshold` ordering at config load and in `--init` wizard. Both thresholds also enforce finite (0.0, 1.0) exclusive range. Wizard re-prompts on violation instead of silently accepting. `tui_remote` now calls `Config::validate()` after load (#1302)
- Consolidate all project-level runtime artifacts under `.zeph/` directory. Default paths changed: `data/zeph.db` → `.zeph/data/zeph.db`, `skills/` → `.zeph/skills/`, `.local/debug` → `.zeph/debug`. Startup migration warning logs exact `mv` commands when old paths are detected. Explicit config paths are unaffected (#1353)

### Fixed

- Skill trust system was entirely non-functional: trust DB was never populated on skill load, `TrustGateExecutor` was defined but never wired into the executor chain, and trust commands always returned "not found". Fixed by populating `skill_trust` table after load/reload with source-based level (local→`local_level`, hub→`default_level`) and hash-mismatch detection, wrapping `CompositeExecutor` with `TrustGateExecutor` as the outermost layer, adding `set_effective_trust` to `ErasedToolExecutor` trait with forwarding through `DynExecutor`, overriding `set_effective_trust` in `impl ToolExecutor for TrustGateExecutor` (inherent method was shadowed by trait default no-op), and extending `Quarantined` trust blocking to `execute()`/`execute_confirmed()` paths (#1405)
- Sub-agent LLM call no longer fails with `no route configured` when `model` is omitted in the agent definition. `ModelOrchestrator::chat_with_fallback` and `stream_with_fallback` now fall through to `default_provider` when no matching route chain exists, instead of returning `LlmError::NoRoute` early. Sub-agents with an explicit `model` field now route to the named provider via the new `chat_for_named` method, with fallback to default routing if the named provider fails (#1396)
- `/image` command crash when file does not exist: CLI channel now prints a user-facing error and continues instead of propagating `ChannelError::Io` and exiting (#1391)
- `/image` command silently ignoring valid files: image is now held in `pending_attachments` and attached to the next outgoing message, so the follow-up prompt sees the image (#1391)
- `agent/mod.rs` `/image` handler stored loaded image parts in a local Vec that was immediately dropped; parts are now held in `Agent::pending_image_parts` and merged into the next `process_user_message` call (#1391)
- Cost tracker: add missing Claude 4.5/4.6 model pricing entries; warn on unknown model (#1385)
- `--log-file` now accepts bare flag (without value) to disable file logging, overriding config (#1378)
- Response cache bypassed in native `tool_use` path: `process_response_native_tools()` never called `check_response_cache()` or `store_response_in_cache()`, so cache lookups and stores only worked in the legacy non-tool path. Add cache check before the tool loop and cache store after `ChatResponse::Text` responses (#1377)
- `[memory.compression]` and `[memory.routing]` config sections silently ignored on startup; only applied after config hot-reload. Add `with_compression()` and `with_routing()` builder methods and wire them in agent construction (#1374)
- Add partial index `idx_graph_edges_expired` on `graph_edges(expired_at) WHERE expired_at IS NOT NULL` (migration 027) to accelerate `delete_expired_edges` eviction query, which previously required a full table scan (#1264)
- Response cache is never consulted at runtime: `check_response_cache()` was guarded by `!self.provider.supports_streaming()` but all real providers return `true`. Remove the streaming guard so cache lookups work for all providers. Also store responses in cache from the streaming code path, which was previously missing (#1366)
- `FeedbackDetector` missed "That's wrong" as `ExplicitRejection`: existing pattern required `that's not (right|correct|...)` but not `that's (wrong|incorrect|bad|terrible)`. Add dedicated pattern `^that'?s\s+(wrong|incorrect|bad|terrible|not\s+helpful)\b` to `EXPLICIT_REJECTION_PATTERNS` (#1394)
- Correction detector false positive: user self-corrections (e.g., "I was wrong, the capital is Canberra") no longer penalize all active skills. Add `SelfCorrection` detection kind with dedicated regex patterns, checked before rejection patterns. Self-corrections are stored for analytics but skip `record_skill_outcomes()`. Tighten overly broad `AlternativeRequest` start-of-line regex (#1361)
- Cross-session history restore regression: mid-history orphaned `tool_use` blocks caused Claude API 400 errors when compaction split a `tool_use`/`tool_result` pair across the compaction boundary. Extend `sanitize_tool_pairs()` with `strip_mid_history_orphans()` to scan all messages (not just boundaries), stripping unmatched `ToolUse` parts while preserving text content. Add defense-in-depth in `split_messages_structured()`: unmatched tool_use blocks are downgraded to text before reaching the API. Add `parse_parts_json()` helper with explicit `tracing::warn!` logging on deserialization failure instead of silent fallback (#1383)
- Cross-session history restore could produce orphaned `tool_use`/`tool_result` messages at history boundaries, causing Claude API 400 errors. Add `sanitize_tool_pairs()` post-load sanitization in `load_history()` that removes trailing assistant messages with unmatched `ToolUse` parts and leading user messages with unmatched `ToolResult` parts. Fixes both LIMIT-boundary splits and session-interruption orphans (#1360)
- Tool output overflow: `save_overflow()` now returns the full absolute path instead of just the UUID filename, so the LLM can use the read tool to access saved overflow files. Overflow notice includes byte count. Fallback warning added when disk write fails. Truncation threshold aligned with overflow threshold to close the 30K-50K data loss gap (#1352)
- Correction embedding storage fails with FOREIGN KEY constraint error (SQLite code 787) on a clean database. Add missing `ensure_named_collection()` call before vector store operations in `store_correction_embedding()` and `retrieve_similar_corrections()` (#1348)
- Router provider no longer eagerly initializes all providers in chain at startup. Providers that fail to initialize (e.g. missing API keys) are skipped with a warning instead of aborting the entire chain (#1345)
- Compatible provider API key is now optional for local endpoints (localhost, private networks). Add `api_key` field to `[[llm.compatible]]` config as an alternative to vault secrets (#1345)
- Claude adaptive thinking mode (`--thinking adaptive`) no longer fails with 400 Bad Request. Use correct API type `"adaptive"` instead of `"enabled"` without `budget_tokens`. Add `output_config.effort` support for adaptive effort levels (#1356)

### Breaking Changes

- Remove `daemon`, `mock`, `orchestration`, and `graph-memory` Cargo feature flags. All four are now compiled unconditionally into every build. Remove these flags from any `--features` lists or CI matrix entries. The `full` feature set no longer includes them.

### Added

- Add configurable log file path (`[logging]` config section, `--log-file` CLI flag, `ZEPH_LOG_FILE`/`ZEPH_LOG_LEVEL` env overrides). File logging uses a separate level filter from `RUST_LOG`, supports daily/hourly/never rotation via `tracing-appender`, defaults to `.zeph/logs/zeph.log`. Single unified `init_tracing()` replaces scattered tracing init calls in `runner.rs`. TUI `/log` command shows current log config and recent entries; tail output is redacted via `scrub_content()` and capped at 512 chars/line and 4 KiB total. Init wizard `--init` includes a logging configuration step with level validation (#1355)

- ACP gap closure (SDK v0.10): upgrade `agent-client-protocol` to 0.10; rename `kill_terminal_command` → `kill_terminal` throughout zeph-acp; advertise MCP capabilities with `http=true, sse=false` (SSE deprecated in MCP spec 2025-11-25); implement `ResourceLink` resolution with SSRF defense (post-fetch `remote_addr()` private-IP check eliminating DNS rebinding TOCTOU window, fail-closed on missing remote_addr, CGNAT 100.64.0.0/10 blocked, cwd boundary enforcement, pseudo-filesystem blocklist, binary-file null-byte detection, pre-flight size check, 10s timeout, 1 MiB cap, full XML-injection escaping via `xml_escape()` on both URI attribute and content body); add `StopReason::MaxTokens` / `MaxTurnRequests` mapping via `StopHint` channel event and `MAX_TOKENS_TRUNCATION_MARKER` constant detected in Claude text-only responses and OpenAI (`finish_reason="length"`) responses; add `SessionConfigOptionCategory` annotations to config options; emit fire-and-forget `ConfigOptionUpdate` notification for only the changed option on model/thinking/auto-approve changes.
- Add ACP LSP extension (Phase 3, #1292, #1293): when Zeph runs inside an IDE via ACP, the agent can query the IDE's native LSP sessions for code intelligence — hover, definition, references, diagnostics, document symbols, workspace symbol search, and code actions. New `crates/zeph-acp/src/lsp/` module with `LspProvider` trait, `AcpLspProvider` (IDE via ext\_method), `McpLspProvider` (mcpls fallback), and bounded `DiagnosticsCache` with LRU eviction. Capability negotiation via `meta["lsp"]` in ACP initialize. `[acp.lsp]` config section with configurable limits (max 20 diagnostics/file, 5 files, 100 references, 50 symbols, 10s timeout). Handles `lsp/publishDiagnostics` and `lsp/didSave` notifications from IDE.
- Add LSP context injection (`lsp-context` feature, #1287 Phase 2): automatic diagnostics injection after `write_file`, optional hover pre-fetch after `read_file`, and reference listing before `rename_symbol`. Hooks run inside the tool execution pipeline via `LspHookRunner`, which calls mcpls through the existing `McpManager`. Notes are injected as `[lsp ...]` prefixed user messages into the message history, subject to a configurable `token_budget` (default 2000). Gracefully degrades to no-op when mcpls is unavailable.
- Add `[agent.lsp]` config section with `LspConfig`, `DiagnosticsConfig`, `HoverConfig`, and `ReferencesConfig` types in `zeph-core`. Defaults: diagnostics enabled (errors-only, max 20 per file, max 5 files), hover disabled, references enabled (max 50).
- Add `--lsp-context` CLI flag to enable LSP context injection for a session, overriding `agent.lsp.enabled` in config.
- Add `step_lsp_context()` to `zeph --init` wizard: prompts for context injection after the mcpls step; skipped when mcpls is not configured. Generates `[agent.lsp]` config section with defaults.
- Add `/lsp` interactive command and `lsp:status` TUI command palette entry: shows hook state, MCP server connection status, per-hook injection counts, and token budget usage.
- Add `LspConfig` to `AgentConfig` behind `#[cfg(feature = "lsp-context")]`.
- Add `--experiment-run` and `--experiment-report` CLI flags for headless experiment sessions and result printing without entering the agent loop (#1318).
- Add `/experiment start|stop|status|report|best` TUI and CLI interactive commands with concurrent session guard and background execution via `tokio::spawn`.
- Add `step_experiments()` to `zeph --init` wizard: prompts for experiment enable, judge model, and schedule configuration.
- Add `config/testing.toml` with `[experiments]` section enabled for test environments.
- Propagate `experiments` feature flag to `zeph-tui` crate for experiment engine integration in TUI builds.
- Add autonomous self-experimentation engine (Phase 1): `experiments` feature flag (opt-in), `ExperimentConfig` with `enabled = false` default and numeric bounds validation, `Variation`/`ParameterKind`/`ExperimentResult` types with `ordered-float` for deterministic hashing, SQLite storage with CRUD operations (`insert_result`, `list_results`, `best_result`, `results_since`, `session_summary`), timestamp format validation, safety caps on query results (#1313, #1312)
- Add benchmark dataset and LLM-as-judge evaluator for autonomous experiments engine (`experiments` feature flag): TOML benchmark format with prompt/context/reference/tags fields, `Evaluator` with configurable judge model and parallel scoring via `FuturesUnordered`, `EvalReport` with mean score, p50/p95 latency, partial result indicators, budget enforcement with per-invocation token tracking, XML boundary tags for prompt injection defense, path traversal protection and file size limits on benchmark files (#1314)
- Add parameter variation engine for autonomous experiments (`experiments` feature flag): `SearchSpace` with `ParameterRange` (min/max/step/default, validation, quantization anchored at min), `ConfigSnapshot` for sandboxed parameter snapshots with `apply`/`diff`/`to_generation_overrides`, `VariationGenerator` trait with three pluggable strategies — `GridStep` (systematic sweep), `Random` (uniform sampling with rejection), `Neighborhood` (perturbation around baseline). One-at-a-time constraint ensures each variation changes exactly one parameter. Deduplication via `OrderedFloat`-based `HashSet`. Integer-aware handling for `TopK`/`RetrievalTopK` (#1315)
- Add experiment loop engine for autonomous experiments (`experiments` feature flag): `ExperimentEngine` orchestrates the full vary-evaluate-decide cycle with progressive baseline (greedy hill climbing), `CancellationToken` graceful shutdown via `tokio::select!`, SQLite persistence of all results, `ExperimentSessionReport` with session summary and best config. Consecutive NaN guard (3-strike limit), baseline NaN early exit, cancellation-aware baseline evaluation. Parameter recording mode for Phase 4 MVP (#1316)

## [0.14.1] - 2026-03-07

### Added

- Extend `[agent] summary_model` to support all provider backends: `claude[/<model>]` (requires `ZEPH_CLAUDE_API_KEY`), `openai[/<model>]` (requires `ZEPH_OPENAI_API_KEY`), `compatible/<name>` (named entry from `[[llm.compatible]]`), `candle` (uses `[llm.candle]` config, feature-gated). Previously only `ollama/<model>` was supported.

- Add LSP code intelligence via mcpls: `step_mcpls` wizard step in `zeph --init` with PATH detection, workspace root prompt, and `[mcp.servers.mcpls]` config generation; add `mcpls` to MCP command allowlist in `zeph-mcp`; `docs/src/guides/lsp.md` with full setup guide and all 16 tool descriptions; `skills/code-analysis/SKILL.md` for LLM-guided LSP workflows (Phase 1, #1288, #1287)

### Fixed

- Fix deferred tool pair summaries never being applied: `prepare_context` recomputes `cached_prompt_tokens` to a low post-pruning value each turn, so the token-based threshold (70% of budget) was never reached. Add count-based fallback: apply deferred summaries when `pending >= tool_call_cutoff`, preventing accumulated deferred summaries from being silently discarded as `[pruned]` content.

### Changed

- Deferred tool pair summarization: summaries are computed eagerly during the tool loop but applied lazily (Tier 0) when context usage exceeds `deferred_apply_threshold` (default 0.70), preserving the message prefix for Claude API prompt cache hits; add `deferred_apply_threshold` config option, `--init` wizard support, force-apply safety net before compaction drain (#1294)

## [0.14.0] - 2026-03-06

### Fixed

- Fix tool output pruning racing with summarization: swap execution order so `maybe_summarize_tool_pair` runs before `prune_stale_tool_outputs`, align pruning window with summarization threshold via `2 * tool_call_cutoff + 2` formula, remove hardcoded `TOOL_LOOP_KEEP_RECENT = 4` constant (#1284)
- Fix `persist_message` saving parts from wrong message — `self.messages.last()` returned the previous message's parts instead of the current one, causing 100% parts corruption in SQLite for all tool interactions; now takes explicit `parts` parameter (#1279)
- Fix token counting using flattened `content` instead of structured `parts` — add `count_message_tokens` to `TokenCounter` that estimates tokens per `MessagePart` variant matching API payload structure, update 6 call sites in context budget tracking (#1280)


### Added

- Add `--graph-memory` CLI flag to enable graph memory for the session, overriding `memory.graph.enabled` in config (`src/cli.rs`, `src/runner.rs`) (Phase 6, #1233)
- Add graph memory questions to `zeph init` wizard: "Enable knowledge graph memory? (experimental)" and "LLM model for entity extraction" prompts; results written to `[memory.graph]` config section (`src/init.rs`) (Phase 6, #1233)
- Add five `/graph` TUI slash commands handled in agent loop: `/graph` (stats), `/graph entities`, `/graph facts <name>`, `/graph communities`, `/graph backfill [--limit N]`; all with pre-dispatch status messages (`crates/zeph-core/src/agent/graph_commands.rs`) (Phase 6, #1233)
- Add five graph-memory command palette entries (`graph:stats`, `graph:entities`, `graph:facts`, `graph:communities`, `graph:backfill`) to `extra_command_registry` in `zeph-tui` (Phase 6, #1233)
- Add five graph metrics fields to `MetricsSnapshot` (always present, no `#[cfg]` gate): `graph_entities_total`, `graph_edges_total`, `graph_communities_total`, `graph_extraction_count`, `graph_extraction_failures` (Phase 6, #1233)
- Add `graph_extraction_count` and `graph_extraction_failures` `Arc<AtomicU64>` counters to `SemanticMemory`; incremented in `spawn_graph_extraction` success/failure/timeout paths (Phase 6, #1233)
- Add `sync_graph_extraction_metrics` helper to `AgentUtils` to mirror AtomicU64 counters into `MetricsSnapshot` (Phase 6, #1233)
- Add `GraphStore` backfill SQL methods: `unprocessed_messages_for_backfill(limit)`, `unprocessed_message_count()`, `mark_messages_graph_processed(ids)` (Phase 6, #1233)
- Add `find_entity_by_name` convenience wrapper on `GraphStore` delegating to `find_entities_fuzzy` (Phase 6, #1233)
- Add docs: TUI commands table, CLI flag, configuration wizard, and backfill sections to `docs/src/concepts/graph-memory.md`; Phase 6 marked complete (Phase 6, #1233)
- Add end-to-end orchestration integration tests (plan graph → execute via DagScheduler tick loop → aggregate with LlmAggregator) covering happy path, single-task, abort-on-failure, skip-on-failure, and retry-exhausted scenarios; gated on `orchestration` + `mock` features (#1242)
- Add "Limitations" section to `docs/src/concepts/task-orchestration.md` documenting English-only keyword routing, `max_tasks` cap, no dynamic re-planning, no hot-reload of orchestration config, and reserved `planner_model`/`planner_max_tokens` fields (#1242)
- Add embedding-based entity resolution for graph memory: cosine similarity search via Qdrant `zeph_graph_entities` collection, LLM disambiguation for ambiguous matches, batch resolution with `buffer_unordered(4)`, per-entity-name locking, graceful fallback to exact match on embedding/LLM failures (#1230)
- Add `entity_ambiguous_threshold` field to `[memory.graph]` config (default 0.70) for disambiguation range lower bound (#1230)
- Add `ResolutionOutcome` enum (`ExactMatch`, `EmbeddingMatch`, `LlmDisambiguated`, `Created`) to `EntityResolver::resolve()` return type (#1230)
- Add `GraphStore::find_entity_by_id()` and `GraphStore::set_entity_qdrant_point_id()` methods (#1230)
- Add `EmbeddingStore::upsert_to_collection()` for point-id-stable Qdrant upserts (#1230)
- Add `SemanticMemory::embedding_store()` getter for shared `Arc<EmbeddingStore>` access (#1230)
- Add `PlanView` TUI widget (`crates/zeph-tui/src/widgets/plan_view.rs`): live task graph table with per-row status spinners, status colors (Running=Yellow, Completed=Green, Failed=Red, Pending=White, Cancelled=Gray), goal truncation with ellipsis, 30-second stale-plan auto-dismiss, and `is_stale()` on `TaskGraphSnapshot` (Phase 6, #1241)
- Add `plan_view_active` toggle (`p` key) to `App`: switches right side panel between Sub-agents and Plan View; auto-resets on new plan detected via graph_id comparison in `poll_metrics()` (#1241)
- Add `TaskGraphSnapshot` and `TaskSnapshotRow` to `MetricsSnapshot`: always-compiled snapshot types populated from `TaskGraph` via `From<&TaskGraph>` impl (feature-gated `orchestration`); includes `strip_ctrl()` state machine for CSI sequence stripping on task titles, agent names, error strings, and plan goals (#1241)
- Add five `plan:*` command palette entries: `plan:status`, `plan:confirm`, `plan:cancel`, `plan:list`, `plan:toggle` (#1241)
- Add `step_orchestration()` to `--init` wizard: configures `enabled`, `max_tasks`, `max_parallel` (with `max_parallel > max_tasks` auto-correction), `confirm_before_execute`, `failure_strategy`, and `planner_model` (validated: max 128 chars, `[a-zA-Z0-9:.-]`) (#1241)
- Add `[Plan]`/`[Agents]` mode indicator to TUI status bar when an orchestration graph is active (#1241)
- Add community detection via label propagation (`petgraph::UnGraph`): `detect_communities` groups entities into clusters (max 50 LPA iterations, tie-break by smallest label, min 2 entities per community), generates LLM summaries, and persists to `graph_communities` table (Phase 5, #1228)
- Add incremental community assignment (`assign_to_community`): new entities are placed into the nearest existing community via neighbor majority vote without triggering full re-detection (#1228)
- Add graph eviction policy: `run_graph_eviction` deletes expired edges older than `expired_edge_retention_days` (default 90), orphan entities with no active edges, and enforces optional `max_entities` cap; runs during community refresh cycle (#1228)
- Add community refresh counter persistence via `graph_metadata` SQLite table; `increment_extraction_count` uses atomic `INSERT ON CONFLICT DO UPDATE` for concurrent-safe increments (#1228)
- Add `graph_community_detection_failures: u64` to `MetricsSnapshot`; `Arc<AtomicU64>` in `SemanticMemory` incremented on community detection errors for observability (#1228)
- Add `expired_edge_retention_days` (default 90) and `max_entities` (default 0 = unlimited) fields to `GraphConfig` (#1228)
- Add `petgraph = { version = "0.8", default-features = false, features = ["stable_graph"] }` as optional workspace dependency; included in `graph-memory` feature (#1228)
- Add prompt injection protection in `generate_community_summary`: entity names and edge facts sanitized via `scrub_content()` before LLM prompt construction (#1228)
- Add docs: community detection, graph eviction, and configuration sections to `docs/src/concepts/graph-memory.md`; Phase 5 marked complete (#1228)
- Add entity canonicalization with alias table for graph memory: `canonical_name` column on `graph_entities`, `graph_entity_aliases` lookup table, alias-first resolution in `EntityResolver`, deterministic first-registered-wins semantics, canonical-name deduplication in `graph_recall`, migration 024 with FK pragma guards (#1231)
- Add `Aggregator` trait and `LlmAggregator` implementation: synthesizes completed task outputs into a single coherent response via LLM call with per-task character budget (`aggregator_max_tokens / num_completed_tasks`), `ContentSanitizer` spotlighting on task outputs, skipped-task descriptions, and raw-concatenation fallback when LLM call fails (Phase 5, #1240)
- Add `/plan resume [id]` command: resumes a graph paused by the `ask` failure strategy via `DagScheduler::resume_from()`; reconstructs running-task map from graph state and sets status to `Running` before re-entering the tick loop (#1240)
- Add `/plan retry [id]` command: re-runs a failed graph by resetting `Failed` tasks to `Ready` and `Skipped`/`Canceled` tasks to `Pending` via `dag::reset_for_retry()` BFS traversal; graph-id validation rejects mismatched IDs (#1240)
- Add `DagScheduler::resume_from()` constructor: accepts `Paused` or `Failed` graphs, reconstructs `running` HashMap from tasks with `Running` status, and sets `graph.status = Running` (#1240)
- Add `dag::reset_for_retry()`: BFS-based algorithm resetting `Failed` tasks to `Ready` and `Skipped`/`Canceled` dependents to `Pending` for re-evaluation (#1240)
- Add `aggregator_max_tokens` field to `OrchestrationConfig` (default: 4096) for controlling the aggregation LLM call token budget (#1240)
- Add FTS5 full-text search index for graph entities (`graph_entities_fts`), replacing `LIKE '%query%'` with FTS5 MATCH + bm25 ranking in `find_entities_fuzzy`; migration `023_graph_entities_fts5.sql` with unicode61 tokenizer, content-sync triggers, and backfill (#1232)

### Changed

- **Breaking**: `EntityResolver::resolve()` now returns `Result<(i64, ResolutionOutcome)>` instead of `Result<i64>` (#1230)
- Add `/plan` CLI commands: `PlanCommand` enum with Goal, Status, List, Cancel, Confirm variants; `/plan <goal>` decomposes goals via LlmPlanner with pending-confirmation flow (`confirm_before_execute`), `/plan status`/`list`/`cancel` for graph management (Phase 4, #1239)
- Add `OrchestrationMetrics` (plans_total, tasks_total, tasks_completed, tasks_failed, tasks_skipped) always present in `MetricsSnapshot` — no `#[cfg]` gating (#1239)
- Add agent loop integration for `/plan` dispatch with feature-gated handlers, `pending_graph` confirmation state, `format_plan_summary()` display, and overwrite guard (#1239)
- Add DAG scheduler (`DagScheduler`) with tick-based execution loop, command pattern (`SchedulerAction`), mpsc event channel (`TaskEvent`/`TaskOutcome`), task timeout monitoring, and cross-task context injection with char-safe truncation (Phase 3, #1238)
- Add `AgentRouter` trait and `RuleBasedRouter` with 3-step fallback chain (agent_hint, tool keyword matching, first available) for task-to-agent routing (#1238)
- Add `spawn_for_task()` to `SubAgentManager` with JoinHandle wrapping for orchestration event delivery (#1238)
- Add stale event guard in scheduler: rejects completion events from timed-out-then-retried agents (#1238)
- Add `ContentSanitizer` integration in `build_task_prompt()` for cross-task dependency output sanitization (#1238)
- Add `dependency_context_budget` (default 16384) and `confirm_before_execute` (default true) to `OrchestrationConfig` (#1238)
- Add graph-aware retrieval with BFS traversal: `graph_recall` function with fuzzy word-split entity matching, depth-tracked BFS expansion, composite scoring, and deduplication (#1226)
- Add `MemoryRoute::Graph` variant to memory router with `RELATIONSHIP_PATTERNS` heuristic for relationship-style queries (#1226)
- Add `BudgetAllocation.graph_facts` (4% when graph-memory enabled) and `ContextBudget.graph_enabled` for graph-aware context budget allocation (#1226)
- Add `recall_graph` method to `SemanticMemory` with `graph_store: Option<Arc<GraphStore>>` field (#1226)
- Add graph facts context injection with `[known facts]` prefix, fact-by-fact token budget enforcement, and sanitization via `sanitize_memory_message` (#1226)
- Add `GraphConfig` to `MemoryState` for runtime access to `recall_limit`/`max_hops` configuration (#1226)
- Add `bfs_with_depth` to `GraphStore` returning per-entity hop distances, `MAX_FRONTIER=300` guard against SQLite bind variable limit (#1226)
- Add LLM-based task planner: `Planner` trait and `LlmPlanner<P>` implementation for goal decomposition into validated `TaskGraph` via `chat_typed` structured output, string-to-`TaskId` mapping, kebab-case task_id validation, agent hint matching against `SubAgentDef` catalog (Phase 2, #1237)
- Add `planner_model` and `planner_max_tokens` fields to `[orchestration]` config section (#1237)
- Add task orchestration core types (`TaskGraph`, `TaskNode`, `TaskId`, `GraphId`, `TaskStatus`, `GraphStatus`, `FailureStrategy`, `TaskResult`), DAG algorithms (`validate`, `toposort`, `ready_tasks`, `propagate_failure`), `OrchestrationConfig`, `OrchestrationError`, and SQLite persistence via `RawGraphStore`/`GraphPersistence` (Phase 1, #1236)
- Add `orchestration` feature flag in root, `zeph-core`, and `zeph-memory` crates (included in `full`) (#1236)
- Add `[orchestration]` TOML config section: `enabled`, `max_tasks`, `max_parallel`, `default_failure_strategy`, `default_max_retries`, `task_timeout_secs` (#1236)
- Add migration `022_task_graphs.sql` with JSON blob persistence for task graphs (#1236)
- Add TUI security integration (Phase 5, #1195): security indicator in status bar (yellow "SEC: N flags", red "N blocked"), security side panel widget with aggregate metrics and recent events, `security:events` command palette entry for full event history (#1195)
- Add `SecurityEvent` and `SecurityEventCategory` types in `zeph-core::metrics` with ring buffer (VecDeque, cap 100) in `MetricsSnapshot` for security event transport via existing watch channel (#1195)
- Add `SecurityEvent` emission at Agent call sites (context.rs, tool_execution.rs, persistence.rs) for injection flags, truncations, quarantine successes/failures, exfiltration blocks, and memory write guards (#1195)
- Add time-based Security/Subagents panel toggle (60s recency window) to avoid permanently hiding subagent visibility after a single security event (#1195)
- Add UTF-8-safe truncation for `SecurityEvent` detail (128 chars) and source (64 chars) fields with ASCII control character stripping (#1195)
- Add background graph extraction to agent loop: fire-and-forget `tokio::spawn` with configurable timeout, injection-flag guard, last-4-user-messages context window (`maybe_spawn_graph_extraction` in `zeph-core::agent::persistence`) (#1227)
- Add `recall_graph` method to `SemanticMemory`: fuzzy entity match + BFS edge traversal, composite-score sort, and token-budget formatting with `[knowledge graph]` prefix (#1227)
- Add `spawn_graph_extraction` fire-and-forget method to `SemanticMemory` with per-task timeout wrapping `extract_and_store` (#1227)
- Add `graph_facts` slot to `prepare_context` via `ContextSlot::GraphFacts` in `FuturesUnordered` concurrent fetch pipeline (#1227)
- Add `graph_facts: usize` field to `BudgetAllocation`; budget split is runtime-conditional: graph enabled → semantic_recall=5%, graph_facts=3%; disabled → semantic_recall=8%, graph_facts=0% (#1227)
- Add `with_graph_config()` builder method to `AgentBuilder` for setting `GraphConfig` (feature-gated) (#1227)
- Add `GRAPH_FACTS_PREFIX` constant (`[knowledge graph]\n`) in `zeph-core::agent` for context injection prefix (#1227)
- Add extraction attempt counter increment before LLM call so `extraction_count` reflects every non-empty attempt regardless of parse success (#1227)
- Add entity name/relation structural-char escaping (`\n`, `\r`, `<`, `>`) in `fetch_graph_facts` to prevent graph-stored content from injecting into system prompt (R-IMP-02) (#1227)
- Add PII/redaction security doc comment on `GraphConfig` and startup `tracing::warn!` in `with_graph_config` when graph is enabled (R-IMP-03) (#1227)
- Add entity name cache in `recall_graph` BFS to eliminate N+1 `find_entity_by_id` calls for edge endpoints (R-SUG-01) (#1227)
- Add SQLite 999-bind-parameter cap in `GraphStore::bfs` frontier (333 IDs/hop) and visited_ids (499) (R-SUG-03) (#1227)
- Add `GraphExtractor` with LLM-powered entity/relation extraction via `chat_typed_erased`, system prompt with 10 extraction rules, truncation guards, and graceful parse-failure degradation (#1225)
- Add `EntityResolver` with exact name+type entity resolution (`resolve`) and edge deduplication/supersession (`resolve_edge`), case-insensitive entity matching, unknown-type coercion to `Concept` (#1225)
- Add `ExtractionResult`, `ExtractedEntity`, `ExtractedEdge` types with `JsonSchema` derivation for structured LLM output (#1225)
- Add `GraphStore::edges_exact` for unidirectional edge queries (performance optimization) (#1225)
- Add entity name sanitization: control-character stripping (ASCII controls + BiDi overrides), 512-byte length cap, empty-name rejection (#1225)
- Add relation/fact string sanitization: control-character stripping, length caps (256/2048 bytes) (#1225)
- Add graph memory schema with SQLite tables (`graph_entities`, `graph_edges`, `graph_communities`, `graph_metadata`) and `messages.graph_processed` flag (migration 021) (#1224)
- Add `GraphStore` CRUD with 18 methods: entity/edge/community upsert, BFS traversal with cycle-safe iterative algorithm, metadata persistence (#1224)
- Add `EntityType` enum (8 variants), `Entity`, `Edge`, `Community`, `GraphFact` types in `zeph-memory::graph` module (#1224)
- Add `GraphConfig` to `[memory.graph]` TOML section: `enabled`, `extract_model`, `max_hops`, `recall_limit`, and 7 more tuning knobs (#1224)
- Add `graph-memory` feature flag in root, `zeph-core`, and `zeph-memory` crates (included in `full`) (#1224)

### Changed

- Arc-wrap `EmbeddingStore` in `SemanticMemory` for shared access in future background tasks (#1223)
- Replace dual cfg-gated `try_join!` blocks in `prepare_context` with `FuturesUnordered` + `ContextSlot` enum for extensible concurrent context fetching (#1223)

### Security

- Add `ExfiltrationGuard` in `zeph-core::sanitizer::exfiltration` (Phase 4, #1195): three independently toggleable guards under `[security.exfiltration_guard]` — `block_markdown_images` strips external-URL markdown images from LLM output before channel send and persistence (inline `![alt](url)` and reference-style `![alt][ref]` with percent-decode); `validate_tool_urls` cross-references tool call arguments (JSON-parsed for unescaping) against URLs flagged in untrusted content, emitting warnings (flag-only, no blocking); `guard_memory_writes` skips Qdrant embedding for messages with injection flags to prevent semantic-search poisoning while preserving SQLite history
- Add `exfiltration_images_blocked`, `exfiltration_tool_urls_flagged`, `exfiltration_memory_guards` counters to `MetricsSnapshot`
- Apply exfiltration output scan to native tool-use text path, ToolUse text field, legacy non-streaming path, accumulated streaming response, and response cache hits
- Add `ExfiltrationGuardConfig` to `SecurityConfig`; all three guards default to enabled
- Clear `flagged_urls` per-turn (at start of `process_response`) to prevent false-positives from previous turns
- Pass `has_injection_flags` explicitly to `persist_message` parameter instead of mutable agent state to avoid stale-flag bugs (critic finding M2)

- Add `ContentSanitizer` pipeline in `zeph-core` that wraps untrusted content (tool results, web scrape, MCP responses, A2A messages, memory retrieval) in spotlighting XML delimiters before it enters the LLM message history, defending against indirect prompt injection (#1196, #1197, #1198, #1199)
- Add 17 compiled injection detection patterns covering common prompt injection techniques; detected patterns are flagged (not removed) and trigger a `[WARNING]` addendum in the spotlighting wrapper (#1197)
- Apply sanitizer to both `Ok(Some(output))` and `ConfirmationRequired` branches of `handle_tool_result`, and to all memory retrieval messages in `prepare_context` (recall, cross-session, corrections, document RAG, summaries) (#1196)
- Escape delimiter tag names (`<tool-output>`, `<external-data>`) from untrusted content before wrapping to prevent wrapper escape injection (#1197)
- Add system prompt security note in `BASE_PROMPT_TAIL` instructing the LLM to treat `<tool-output>` and `<external-data>` content as untrusted data, not instructions (#1199)
- Add `[security.content_isolation]` TOML config section: `enabled`, `max_content_size` (64 KiB default), `flag_injection_patterns`, `spotlight_untrusted` (#1198)
- Add `sanitizer_runs`, `sanitizer_injection_flags`, `sanitizer_truncations` counters to `MetricsSnapshot` (#1197)
- Differentiate `ContentSourceKind` in `sanitize_tool_output`: MCP tools use `McpResponse` (ExternalUntrusted), web-scrape/fetch use `WebScrape` (ExternalUntrusted), others remain `ToolResult` (LocalUntrusted) (#1200, #1201)
- Sanitize A2A inbound messages as `ExternalUntrusted` in `AgentTaskProcessor` before they enter the agent loop; add `all_text_content()` to collect all `Part::Text` entries (#1202)
- Sanitize code RAG text from `zeph-index` before injection into context with metrics tracking and injection flag logging (#1203)
- Sanitize tool error messages before `self_reflection` context using `ExternalUntrusted` as conservative default (#1200)
- Add `QuarantinedSummarizer` — Dual LLM pattern that routes high-risk external content (web scrape, A2A) through an isolated, tool-less LLM extraction call before it enters the main agent context (#1204)
- Add `[security.content_isolation.quarantine]` config section: `enabled` (default false), `sources`, `model` (#1204)
- Re-sanitize quarantine LLM output: run `detect_injections` and `escape_delimiter_tags` on extracted facts before spotlighting (#1204)
- Guard quarantine step behind `sanitizer.is_enabled()` to prevent unnecessary LLM calls when sanitizer is disabled (#1204)
- Add `quarantine_invocations`, `quarantine_failures` counters to `MetricsSnapshot` (#1204)
- Refactor `sanitizer.rs` to `sanitizer/mod.rs` + `sanitizer/quarantine.rs` module structure (#1204)

## [0.13.0] - 2026-03-05

### Security

- SEC-M22-001: fix bearer token timing side-channel in `zeph-gateway` auth middleware — both the submitted token and the expected token are now hashed with BLAKE3 (32-byte fixed-length output) before comparison via `subtle::ConstantTimeEq`, preventing length leaks and timing attacks; expected token hash is pre-computed at startup to eliminate per-request rehashing (#1173)

### Performance

- Pad Block 1 system prompt to exceed 2048-token cache minimum: when the base prompt is below the Sonnet threshold, `split_system_into_blocks()` appends a stable agent identity preamble (~3300 tokens) so Block 1 consistently receives `cache_control: ephemeral` (#1083)

### Changed

- Sub-agent definition format migrated from `+++` TOML frontmatter to `---` YAML frontmatter (Claude Code spec compatible); `+++` TOML remains supported as a deprecated fallback with a `tracing::warn!` log (#1146)
- TUI command palette: `AgentStatus` now correctly dispatches `/agent status` (was `/agent list`)
- Telegram `confirm()` timeout increased to 30s with distinct `tracing::warn!` logs for channel-close vs timeout (#1147)

### Added

- `/agents` management UI: interactive CLI subcommand (`zeph agents list|show|create|edit|delete`) and TUI panel with 5-state FSM (List, Detail, Create form, Edit form, ConfirmDelete) for full CRUD of sub-agent definitions; CLI `edit` opens `$VISUAL`/`$EDITOR` with fallback to `vi`; TUI wizard covers name, description, model, permission_mode, max_turns, background fields; atomic file writes via `tempfile::NamedTempFile::persist()`; `AGENT_NAME_RE` validation on all create paths; extra confirmation for non-project scope delete in TUI (#1154)
- `SubAgentDef::serialize_to_markdown()` round-trip serialization via `WritableRawDef` struct with correct `tools.except` nesting (avoids serde asymmetry); `save_atomic()`, `delete_file()`, `default_template()` core API additions
- `SubAgentDef.file_path: Option<PathBuf>` field populated during `load_with_boundary()` for edit/delete file location
- `AgentsCommand` enum in `zeph-core::subagent::command` for `/agents` CRUD commands (separate from runtime `/agent` commands)
- `SubAgentError::Io` variant for file operation errors
- Sub-agent transcript persistence: JSONL append-only transcripts written per session under `.zeph/subagents/`; each session gets a UUID-named `.jsonl` file and a `.meta.json` sidecar with status, turn count, and lineage (`resumed_from`) (#1153)
- `/agent resume <id> <prompt>` command: resumes a completed, failed, or cancelled sub-agent session by loading its full message history and spawning a new foreground loop with a fresh UUID; supports 8-char prefix matching (#1153)
- `TranscriptWriter` / `TranscriptReader` / `TranscriptEntry` / `TranscriptMeta` types in `zeph-core::subagent::transcript` (#1153)
- `SubAgentManager::resume()` method for loading transcript history and spawning resumed agent loops (#1153)
- `SubAgentError::Transcript`, `SubAgentError::AmbiguousId`, `SubAgentError::StillRunning` error variants (#1153)
- `SubAgentConfig` transcript fields: `transcript_enabled` (default: `true`), `transcript_dir` (default: `.zeph/subagents`), `transcript_max_files` (default: `50`); cooperative sweep-on-access deletes oldest `.jsonl` files when limit exceeded (#1153)
- Query-aware memory routing (`zeph-memory`): `MemoryRouter` trait with `HeuristicRouter` implementation that classifies queries as Keyword (SQLite FTS5), Semantic (Qdrant), or Hybrid based on query structure; code-like patterns route to keyword search, natural language questions route to semantic search; configurable via `[memory.routing] strategy = "heuristic"` (#1162)
- Active context compression (`zeph-core`): proactive compression fires before hitting capacity limits; `CompressionStrategy` enum (`reactive`/`proactive`) with configurable `threshold_tokens` and `max_summary_tokens`; mutual exclusion guard prevents double-compaction per turn; `compression_events` and `compression_tokens_saved` metrics; configurable via `[memory.compression]` (#1161)
- `PermissionMode` enum in sub-agent YAML frontmatter (`permissions.permission_mode`): `default`, `accept_edits`, `dont_ask`, `bypass_permissions`, `plan`; `bypass_permissions` emits a `tracing::warn!` at load time
- `tools.except` list in sub-agent YAML frontmatter: additional denylist applied on top of `tools.allow`/`tools.deny`; deny wins over allow
- `PlanModeExecutor` in `zeph-core`: wraps the real executor to expose tool catalog but block all execution; used when `permission_mode: plan`
- `FilteredToolExecutor::with_disallowed()` constructor: accepts an extra denylist alongside the base `ToolPolicy`
- `background` agents now auto-deny secret requests without blocking on the parent channel (CRIT-01 fix)
- `SubAgentConfig.default_permission_mode` and `SubAgentConfig.default_disallowed_tools` global defaults in `[agents]` config section; both are now applied at spawn time — `default_permission_mode` overrides `Default` mode agents, `default_disallowed_tools` is merged into per-agent denylist (#1180)
- `SubAgentConfig.allow_bypass_permissions: bool` (default: `false`) config gate — spawning a sub-agent with `permission_mode: bypass_permissions` is rejected with an error unless explicitly enabled (#1182)
- Sub-agent lifecycle hooks: `SubagentStart` and `SubagentStop` events in `[agents.hooks]` config section fire shell commands at spawn and termination (fire-and-forget via `tokio::spawn`); per-agent `hooks.PreToolUse` and `hooks.PostToolUse` in YAML frontmatter with pipe-separated matcher patterns (e.g. `"Edit|Write"`); user-level definitions (`~/.zeph/agents/`) have hooks stripped for security; hooks run in a clean env (`env_clear()`) with explicit child kill on timeout (#1150)
- `#[serde(deny_unknown_fields)]` on `RawSubAgentDef`: YAML frontmatter typos (e.g. `permisions:`) are now rejected with a clear parse error instead of being silently ignored (#1183)
- Doc comment on `FilteredToolExecutor::is_allowed()` clarifying that tool ID matching is exact string equality and MCP compound IDs (e.g. `mcp__server__tool`) must be listed in full in `tools.except` (#1181)
- `PermissionMode` re-exported from `zeph-core::subagent` public API
- Agent-as-a-Judge feedback detector (`zeph-core`): `JudgeDetector` sends user messages to a configurable LLM judge for semantic correction detection; adaptive threshold invokes judge only on borderline regex confidence (`[adaptive_low, adaptive_high)`); background `tokio::spawn` decouples judge latency from response pipeline; sliding-window rate limiter (5 calls/60s); XML-escaped prompt template with adversarial content defense; config: `detector_mode = "judge"`, `judge_model`, `judge_adaptive_low`, `judge_adaptive_high` in `[skills.learning]`; defaults to `"regex"` (no behavior change); `--init` wizard integration (#1157)
- MCP declarative policy layer (`zeph-mcp`): per-server `McpPolicy` with allowlist, denylist, and sliding-window rate limiting; `PolicyEnforcer` (backed by `DashMap` per-server mutexes) enforces policy before each `call_tool()` invocation; policy configured via `[mcp.servers.policy]` TOML sub-table; no-policy servers allow all tools (backward compatible)
- Thompson Sampling router strategy in `zeph-llm`: `router/thompson.rs` adds `ThompsonState` with per-provider `BetaDist` (alpha/beta updated on each response); `RouterProvider` now supports `RouterStrategy::Thompson` via `with_thompson()`; state persisted atomically to `~/.zeph/router_thompson_state.json` with 0o600 permissions (Unix); enabled via `strategy = "thompson"` in `[llm.router]` config; `rand_distr::Beta` used for numerically stable sampling with 1e-6 clamping; orphan distributions from removed providers are pruned on load; `SmallRng` seeded once per state instance for efficient sampling (#1156)
- `RouterStrategyConfig` typed enum replaces raw `String` for `[llm.router] strategy` — invalid values now fail at config parse time with a descriptive error
- `zeph router stats` CLI subcommand: displays alpha, beta, and mean selection probability (Mean%) per provider from current Thompson state
- `zeph router reset` CLI subcommand: deletes the Thompson state file, resetting all distributions to uniform priors; uses atomic `remove_file` with `NotFound` matching (no TOCTOU race)
- `/router stats` TUI command palette entry: displays Thompson distribution snapshot (alpha/beta/Mean%) in the TUI panel
- `--init` wizard: router strategy selection step (step 10/12) — choose between EMA and Thompson Sampling, configure state file path
- Daemon mode shutdown now calls `agent.shutdown().await` before PID file removal, ensuring Thompson state is persisted on daemon exit
- Alpha/beta values are validated on state file load: non-finite values reset to 1.0, out-of-range values clamped to [0.5, 1e9] to prevent sampling failures from crafted state files
- Thompson Sampling router fixes (Epic #1156 critic review): state now saved on graceful shutdown via `AnyProvider::save_router_state()` called in `Agent::shutdown()` (CRIT-1); stale provider entries pruned from state file on load via `ThompsonState::prune()` (CRIT-3); `RouterConfig.strategy` migrated from `String` to `RouterStrategyConfig` enum with `#[serde(rename_all = "lowercase")]` for compile-time validation (IMP-2); `tracing::warn!` added on corrupt state file parse (GAP-1/SUG-5); `ThompsonState::rng` stored in state to avoid per-select OS entropy syscall (SUG-1); `PartialEq` derived on `BetaDist` and `ThompsonState` (SUG-4); `zeph router stats` and `zeph router reset` CLI subcommands (IMP-5); `/router stats` TUI command palette entry (IMP-6); `--init` wizard router strategy selection step (IMP-4); `MetricsSnapshot.router_thompson_stats` field updated after each LLM call; streaming false-positive documented in code (CRIT-2 deferred)
- Sub-agent scope & priority system: agents loaded from four scopes with explicit priority — CLI (`--agents` flag) → project (`.zeph/agents/`) → user (`~/.config/zeph/agents/`) → config `extra_dirs`; first definition wins on name collision (#1145)
- `--agents <path>` CLI flag: one or more `.md` files or directories for session-scoped sub-agent definitions; non-existent paths are a hard error
- `SubAgentConfig.user_agents_dir`: configurable user-level agents directory; empty string disables user scope
- Persistent memory scopes for sub-agents: `memory` field in YAML frontmatter with `user`, `project`, and `local` scopes; memory directory created at spawn time; first 200 lines of `MEMORY.md` injected into sub-agent system prompt after behavioral prompt; Read/Write/Edit tools auto-enabled for AllowList agents when memory is set; `default_memory_scope` config in `[agents]` section; `/agent list` shows memory scope, `/agent status` shows memory dir path; `--init` wizard includes memory scope prompt (#1152)
- `/agent list` now shows scope labels: `(cli)`, `(project)`, `(user)`, `(config)` per agent
- `SubAgentDef.source`: scope label field on every loaded definition for diagnostics
- `load_with_boundary()`: canonicalizes paths, enforces directory boundaries (symlink escape prevention), caps at 100 entries per directory
- `--init` wizard: new prompt for user-level agents directory path
- `serde_norway = "0.9.42"` dependency for YAML parsing in sub-agent definitions (replaces TOML-only parsing)
- `FrontmatterFormat` enum in `zeph-core` routes sub-agent definitions to the correct deserializer based on detected delimiter
- 256 KiB file size cap in `SubAgentDef::load()` to prevent DoS via oversized definition files
- Control character validation (ASCII < 0x20 excluding tab, plus DEL 0x7F) for `name` and `description` fields in sub-agent definitions
- TUI command palette entries for sub-agent management: `AgentList`, `AgentStatus`, `AgentCancelPrompt`, `AgentSpawnPrompt` (#1147)
- Sub-agent secret requests now automatically route to `channel.confirm()` in the foreground spawn poll loop, enabling interactive approval via TUI or Telegram (#1147)
- Secret key name validation against `[a-zA-Z0-9_-]+` before `SecretRequest` creation to block prompt-injection via malformed key names (#1147)
- Telegram bot command menu registration via `set_my_commands()` on startup: `/start`, `/reset`, `/skills`, `/agent` (#1147)
- E2E integration tests for sub-agent lifecycle: background spawn+collect and foreground spawn+secret-bridge (#1147)
- Memory eviction subsystem with Ebbinghaus forgetting curve policy, two-phase SQLite+Qdrant sweep, and configurable retention (`[memory.eviction]`) (1.1)

### Fixed

- Telegram `confirm()` was blocking indefinitely on `rx.recv().await` with no timeout — now denied after 30s (#1147)

## [0.12.6] - 2026-03-04

### Added

- Hot reload for instruction files: `InstructionWatcher` in `zeph-core` subscribes to filesystem events via `notify-debouncer-mini` (500ms debounce) and reloads `instruction_blocks` in-place on `.md` file changes without agent restart (#1124)
- `InstructionReloadState` carries reload parameters (base dir, provider kinds, explicit files, auto-detect flag) through the agent select loop
- Explicit instruction file paths are boundary-checked against project root before being added to the watcher; TOCTOU-free load via canonicalize-before-open

### Fixed

- PERF-SC-04: `Scheduler::tick()` `Ok(None)` branch now computes and persists `next_run` via the cron schedule instead of treating missing `next_run` as "due now" — cron expressions are now respected at runtime (#1133)
- `tick_interval_secs` from `[scheduler]` config and `--scheduler-tick` CLI flag now control the actual tick interval; previously hardcoded to 60s; zero/sub-1s values are clamped to 1s (#1136)

### Added

- `TaskMode` enum (`Periodic`/`OneShot`) and `TaskDescriptor` + `SchedulerMessage` mpsc channel: `Scheduler::new()` returns `(Self, Sender<SchedulerMessage>)` eliminating `Arc<Mutex>` deadlock risk; oneshot tasks are removed from the task list after execution (#1134)
- `CustomTaskHandler`: injects `config["task"]` as a new agent turn via a dedicated mpsc channel at the scheduled time (same pattern as update notifications) (#1134)
- `SchedulerExecutor` in `zeph-core`: LLM-facing `ToolExecutor` exposing three tools — `schedule_periodic` (6-field cron), `schedule_deferred` (ISO 8601 UTC future timestamp), `cancel_task`; all `send` paths use `try_send` to avoid blocking agent turns (#1135)
- `zeph_scheduler::sanitize::sanitize_task_prompt`: shared sanitization function — truncates at 512 chars (char boundary safe), strips control characters; prevents prompt injection via the `task` field (#1135)
- `JobStore` extended: `mark_done`, `job_exists`, `delete_job`, `upsert_job_with_mode`; new SQLite migration adding `task_mode` and `run_at` columns; `max_tasks` enforced in `register_descriptor` (#1134)
- `SchedulerConfig` extended with `tick_interval_secs` (default 60) and `max_tasks` (default 100) fields (#1136)
- `ScheduledTaskConfig` extended with `run_at: Option<String>` for one-shot tasks; exactly one of `cron` or `run_at` must be set — invalid entries are skipped at bootstrap with a warning (#1136)
- `--scheduler-tick <secs>` and `--scheduler-disable` CLI flags (#1136)
- `--init` wizard scheduler section: enable/disable, tick interval, max tasks (#1136)
- TUI `/schedule` input command and scheduler status line in footer (#1136)
- `skills/scheduler/SKILL.md`: teaches the agent to create periodic and deferred tasks with `schedule_periodic`, `schedule_deferred`, and `cancel_task` tools; includes cron format reference, built-in kinds, validation rules, and trigger words (#1137)
- `parse_run_at` in `SchedulerExecutor`: `run_at` field now accepts relative shorthand (`+2m`, `+1h30m`, `+3d`), natural language (`in 5 minutes`, `today 14:00`, `tomorrow 09:30`), and naive ISO 8601 (treated as UTC); overflow-safe with `checked_mul`/`checked_add`; single `Utc::now()` snapshot eliminates TOCTOU race (#1141)
- `SchedulerExecutor`: fenced-block dispatch (`InvocationHint::FencedBlock`) for Ollama legacy text-extraction path — model can now invoke scheduler tools without native function calling (#1141)
- Scheduler enabled by default (`SchedulerConfig::default().enabled = true`); updated `config/default.toml` and snapshot (#1141)
- Scheduler `custom` task injection prefixed with `[Scheduled task]`; `SKILL.md` documents reminder-for-user vs agent-action patterns (#1141)
- TUI `scheduler:list` command palette entry: displays all active scheduled tasks (name, kind, mode, next run) from `MetricsSnapshot.scheduled_tasks`; `JobStore::list_jobs()` queries non-done jobs; a 30-second background refresh task populates the metrics when both `tui` and `scheduler` features are enabled (#1141)

## [0.12.5] - 2026-03-02

### Added

- `load_skill` tool in `zeph-core`: LLM can call `load_skill(skill_name)` at inference time to retrieve the full body of any registered skill by name. Non-TOP skills appear in the system prompt as metadata-only catalog entries; this tool enables on-demand access to their full instructions without expanding the system prompt (#1125)

- Provider instruction file loader (`InstructionLoader`) in `zeph-core`: auto-detects `CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, and `zeph.md` from the working directory and injects them into the system prompt with path-traversal protection (symlink boundary check, null byte guard, 256 KiB size cap) (#1122)

### Fixed

- `zeph.md` and `.zeph/zeph.md` are now loaded unconditionally regardless of provider or `auto_detect` setting; previously the early-return on `!auto_detect` skipped them when auto-detection was disabled and no explicit files were configured (#1122)
- `[agent.instructions]` TOML config section: `auto_detect` (default `true`), `extra_files` list, and `max_size_bytes` cap (#1122)
- `--instruction-file <path>` CLI flag for supplying additional instruction files at startup (#1122)
- Claude extended thinking support: `ThinkingConfig` enum (`Extended { budget_tokens }` / `Adaptive { effort? }`) with model capability map; `ClaudeProvider::with_thinking()` builder (returns `Result` with validated 1024–128000 range) (#1089)
- Claude API request serialization for thinking: `thinking` and `output_config` fields on all four request body variants; conditional `interleaved-thinking-2025-05-14` beta header for extended mode on Sonnet 4.6 with tools (#1090)
- Claude response and SSE streaming: `AnthropicContentBlock::Thinking` and `RedactedThinking` variants; `thinking_delta`/`signature_delta` SSE events parsed and suppressed from user stream (#1091)
- Multi-turn tool use: `MessagePart::ThinkingBlock`/`RedactedThinkingBlock` variants preserve thinking blocks verbatim across tool call turns in correct API ordering (#1092)
- `--thinking extended:<budget>` and `--thinking adaptive[:<effort>]` CLI arguments with range validation in `parse_thinking_arg()` (#1089)
- `--init` wizard thinking mode selection prompt (#1089)
- `[llm.claude] thinking = { mode = "extended", budget_tokens = 16000 }` TOML config support (#1089)
- `max_tokens` auto-bumped to 16000 when thinking is enabled and configured value is below threshold (#1090)
- `CacheType` enum (`Ephemeral` variant) replaces bare `String` in `CacheControl` — compile-time safety for cache type construction (#1082, #1088)
- Tool definitions now carry `cache_control: ephemeral` on the last tool entry, enabling tools to be cached independently of the system prompt (#1084)
- Top-level `cache_control` added to all Claude request body structs; activated automatically for multi-turn sessions (`messages.len() > 1`) (#1086)
- Message-level `cache_control` breakpoint placed on user message at position `max(0, total - 20)` to cover the 20-block lookback window (#1087)
- `bash_stdin` ACP tool: writes UTF-8 data to stdin of a running IDE terminal via `ext_method("terminal/write_stdin")`; bounded to 64 KiB (REQ-P23-1); only registered when a permission gate is present (REQ-P23-2); shell interpreter terminals include an explicit warning in the permission prompt (REQ-P23-5); `CancellationToken` per terminal_id cancels pending writes on release/kill (REQ-P23-4); `BrokenPipe` and `ClientError` fast-fail subsequent writes (REQ-P23-3) (#1073)
- `write_file` diff preview and approval pipeline: reads current file content via `ReadForDiff`, emits `ToolCallContent::Diff` for user review, requires explicit permission approval (REQ-P31-2), applies TOCTOU guard via hash comparison after approval (REQ-P31-3); new validation: 10 MiB content limit (REQ-P31-5) and null-byte binary detection (REQ-P31-6) (#1075)
- `AcpError::StdinTooLarge` and `AcpError::BrokenPipe` error variants for stdin forwarding (#1073)
- ACP P1.1: `list_sessions` now populates `title` field for in-memory sessions via async title-gen callback with SQLite caching (#1065)
- ACP P1.2: `set_session_config_option` handles `thinking` (on/off) and `auto_approve` (suggest/auto-edit/full-auto) config keys; `build_config_options` returns all third option groups (#1065)
- ACP P1.3: `send_tool_start` captures `Instant::now()`; `send_tool_output` propagates `started_at` through loopback channel; `tool_call_update` metadata now emits `startedAt` (ISO 8601) and `elapsedMs` (u64 ms) (#1065)
- `Channel::send_tool_output` trait extended with `started_at: Option<Instant>` parameter; all implementations updated (#1065)
- ACP session title fallback now uses `Session <8-char session ID prefix>` instead of raw truncated user text, eliminating exposure of unvalidated input as a visible session identifier (#1099)
- ACP P2.1: `StreamChunk` enum (`Content` / `Thinking`) replaces `String` in `ChatStream`; Claude `thinking_delta` and OpenAI `reasoning_content` SSE events now flow as `LoopbackEvent::ThinkingChunk` → `acp::SessionUpdate::AgentThoughtChunk` (#1065)
- ACP P2.2: `LoopbackEvent::ToolOutput` `diff` field now maps to `ToolCallContent::Diff` in `loopback_event_to_updates`, providing structured diff content in ACP tool call updates (#1065)
- ACP P2.4: `/review [path]` slash command added to ACP agent; injects read-only constrained prompt; arg sanitized against `^[a-zA-Z0-9_./ -]{0,512}$` allowlist (SEC-P24-1); appears in `/help` and `build_available_commands` (#1065)

### Fixed

- Context compaction (tier-1 pruning) now emits `compacting context...` status in TUI; tier-2 compaction no longer clears status prematurely before the next phase overwrites it (#1101)
- Context build status changed from `building context...` to `recalling context...` for better clarity (#1100)
- Skill reload now emits `syncing skill index...` before Qdrant backend sync and `rebuilding search index...` before BM25 index rebuild (#1103)
- Tool output summarization now shows `summarizing output...` status while the LLM compresses long tool outputs (#1105)
- MCP and named tool calls now show `running {tool_name}...` instead of the generic `running tool...` (#1102)
- `FileIndex::build` in file picker moved to `spawn_blocking`; TUI shows `indexing files...` status while repository is being indexed, preventing render loop stalls (#1104)

- Block 1 of system prompt now includes skills prompt, tool catalog, and catalog prompt so its token count exceeds the model-aware minimum threshold (Sonnet 4.6: 2048 tokens, Opus/Haiku: 4096 tokens) — previously ~377 tokens caused caching to be silently skipped (#1083)
- Removed outdated `anthropic-beta: prompt-caching-2024-07-31` request header; prompt caching is GA and no longer requires the beta header (#1085)
- Model-aware `cache_min_tokens` check added to `split_system_into_blocks` to prevent `cache_control` from being attached to blocks below the minimum cacheable threshold (#1083)
- Restructured `rebuild_system_prompt` block ordering so cache markers align with content stability: Block 1 (base prompt) is stable across all turns, Block 2 (skill catalog, MCP, project context) is semi-stable per session, Block 3 (env, tools, active skills) is volatile per turn — fixes near-zero cache hit rate in multi-turn Claude sessions (#1079)
- TUI Skills panel now shows Wilson score confidence bars immediately after skill match, not only after the first LLM outcome is recorded (`context.rs`: call `update_skill_confidence_metrics()` at skill resolution time) (#1077)
- TUI event loop redraws on every tick unconditionally; previously the dirty-flag was never set by the tick arm, causing confidence bars to stay stale between user keypresses (#1077)

### Added

- `zeph-core::testing` module (feature `mock`): reusable `MockChannel`, `MockToolExecutor`, `AgentTestHarness` builder — wires `MockProvider` + `MockChannel` + `MockToolExecutor` + `InMemoryVectorStore` into a ready-to-use agent for unit tests (#1113)
- `zeph-llm::testing` module: wiremock fixture helpers for OpenAI (`/v1/chat/completions` happy path, 429, 401, 500, SSE stream with `finish_reason: stop`) and Claude (`/v1/messages` serde roundtrip, SSE stream, 429/529 overload) (#1109)
- `zeph-memory::testing` module (feature `mock`): `mock_semantic_memory()` using `:memory:` SQLite + `InMemoryVectorStore` — no Docker required (#1110)
- `zeph-mcp::testing` module: `MockMcpServer` at `ToolExecutor` level with configurable tool definitions, canned responses, error injection, and call recording (#1111)
- HTTP-level wiremock tests for `OpenAiProvider`: health check, chat completion, 429 rate limit, 401 auth error, 500 server error (#1109)

### Tests

- Added `skill_confidence_populated_before_first_outcome` regression test (`zeph-core`) to guard against confidence data being absent at skill match time (#1077)
- Added `tick_arm_sets_dirty` regression test (`zeph-tui`) to verify `poll_metrics()` is called on each loop iteration (#1077)
- Total test count: 3218 (+20 new mock infrastructure tests)

## [0.12.4] - 2026-03-01

### Added

- `zeph ingest <path>` CLI subcommand: recursively ingests `.txt`, `.md`, `.pdf` files into Qdrant `zeph_documents` collection via `DocumentPipeline` (#1028)
- Agent RAG context injection: when `memory.documents.rag_enabled = true` and `zeph_documents` is non-empty, top-K relevant chunks are injected into the context window for each conversation turn (#1028)
- `AnomalyDetector` integrated into agent tool execution pipeline: failure rate exceeding configurable threshold triggers `Severity::Critical` alert and auto-blocks the tool via trust system; controlled by `[tools.anomaly]` config section (#1027)
- `GatewayServer` wired into daemon startup and `--daemon` CLI mode: the HTTP webhook ingestion server now starts automatically when `gateway` feature is enabled and `[gateway]` section is configured (#1026)
- `/gateway status`, `/ingest`, `ViewFilters` entries added to TUI command palette (#1026, #1028, #1029)
- `FilterMetrics` surfaced in TUI status bar: real-time filter savings percentage shown alongside existing metrics (#1029)
- Integration test stubs for gateway webhook ingestion and document RAG pipeline (`tests/gateway_integration.rs`, `tests/ingest_integration.rs`) with `#[ignore]` annotation (#1026, #1028)
- `list_directory` native tool in `FileExecutor`: returns sorted entries with `[dir]`/`[file]`/`[symlink]` labels, sandbox-validated (#1053)
- `create_directory`, `delete_path`, `move_path`, `copy_path` tools in `FileExecutor`: structured file system mutation ops, all paths sandbox-validated; `copy_dir_recursive` uses lstat to prevent symlink escape (#1054)
- `fetch` tool in `WebScrapeExecutor`: plain URL-to-text without CSS selector requirement, SSRF protection applied (#1055)
- `DiagnosticsExecutor` with `diagnostics` tool: runs `cargo check --message-format=json` or `cargo clippy`, returns structured error/warning list (file, line, col, severity, message), capped output, graceful degradation if cargo absent (#1056)

### Changed

- Renamed `FileExecutor` tool id `glob` → `find_path` to align with Zed IDE native tool surface (#1052)
- `READONLY_TOOLS` allowlist updated to current tool IDs: `read`, `find_path`, `grep`, `list_directory`, `web_scrape`, `fetch`; removed legacy `file_glob` (#1052)
- `DiagnosticsExecutor` uses `tokio::process::Command` instead of blocking `std::process::Command`
- Migrate dependency automation from Dependabot to self-hosted Renovate: adds `renovate.json` with MSRV-aware `constraintsFiltering: strict`, grouped minor/patch automerge, and a dedicated workflow at `.github/workflows/renovate.yml`; removes `dependabot.yml` and the `dependabot-automerge.yml` workflow (which used the insecure `pull_request_target` trigger)

### Security

- ACP tool notifications: `raw_response` (file content for `read_file`, stdout for `bash`) is now passed through `redact_json` before forwarding to `claudeCode.toolResponse`; prevents secrets from bypassing the `redact_secrets` pipeline when content reaches the IDE (SEC-ACP-001)

### Added
- `FailureKind` enum on `SkillOutcome::ToolFailure` with 7 variants and `from_error()` heuristic classifier
  (`ExitNonzero`, `Timeout`, `PermissionDenied`, `WrongApproach`, `Partial`, `SyntaxError`, `Unknown`) (#1020)
- `/skill reject <name> <reason>` command: records `user_rejection` outcome and immediately triggers
  skill improvement with user feedback, bypassing cooldown/threshold gates (#1020)
- `outcome_detail` column in `skill_outcomes` table (migration 018) for structured failure classification (#1020)
- `FeedbackDetector`: classifies user messages as corrections using regex patterns
  (`ExplicitRejection`, `AlternativeRequest`) and Jaccard token overlap (`Repetition`),
  detects dissatisfaction without requiring explicit commands (#1021)
- `UserCorrection` semantic memory: detected corrections stored in SQLite (`user_corrections` table,
  migration 019) and `zeph_corrections` Qdrant collection; top-3 similar past corrections
  (cosine ≥ 0.75) injected into system prompt for cross-session personalization (#1021)
- `posterior_weight()` and `posterior_mean()` functions (Wilson score) for skill re-ranking:
  final score = cosine × `cosine_weight` + posterior × (1 − `cosine_weight`) (#1022)
- `check_trust_transition()`: auto-promotes skills to `trusted` (≥50 uses, posterior > 0.95)
  and auto-demotes to `quarantined` (≥30 uses, posterior < 0.40); never overrides `blocked` status (#1022)
- TUI skills widget confidence bars: `[████░░░░] 73% (42 uses)` with color coding
  (green > 0.75 / yellow ≥ 0.40 / red < 0.40, aligned with auto-demote threshold) (#1022)
- `Bm25Index` in `zeph-skills`: in-memory BM25 inverted index (k1=1.2, b=0.75) rebuilt on
  skill hot-reload; `rrf_fuse()` Reciprocal Rank Fusion (k=60) combines BM25 and vector results;
  enabled via `[skills] hybrid_search = true` (default: true) (#1023)
- Skill health attributes in system prompt: matched skills with ≥5 recorded uses emit
  `reliability="N%"` and `uses="N"` XML attributes on `<skill>` tags (#1023)
- `EmaTracker` in `zeph-llm`: per-provider exponential moving average of success rate and latency;
  `RouterProvider` reorders providers by EMA score every N calls;
  enabled via `[llm] router_ema_enabled = true` (default: false), alpha default 0.1 (#1023)

### Performance

- Parallelize agent startup initialization: `build_memory` + `build_tool_setup` run concurrently via `tokio::join!` (est. 1-5s savings); `build_skill_matcher` + `build_cli_history` also parallelized; `warmup_provider` spawned as background task on CLI path overlapping with agent assembly (#1031)

### Fixed
- ACP tool notifications: `claudeCode.toolName` is now always included in `_meta.claudeCode` for every `tool_call` and `tool_call_update`, regardless of whether `parentToolUseId` is present (#1037)
- ACP tool notifications: `locations` field is now populated on the initial `tool_call` for Read-kind tools by extracting the path from `params["file_path"]` or `params["path"]` at `ToolStart` time (#1040)
- ACP tool notifications: an intermediate `tool_call_update` (without `status`) carrying `_meta.claudeCode.toolResponse` is now emitted before the final status update for non-terminal tools (`AcpFileExecutor`), allowing IDEs to display structured file content (#1038)
- ACP tool notifications: an intermediate `tool_call_update` carrying `_meta.claudeCode.toolResponse` with `stdout`/`stderr`/`interrupted` fields is now emitted before `terminal_exit` for bash tools (`AcpShellExecutor`) (#1039)
- `version_id` always `NULL` in `skill_outcomes`: `record_skill_outcomes_batch()` now resolves
  the active version ID before insert, enabling per-version metrics and accurate rollback (#1020)
- Panic on `/skill reject` without arguments: byte-slice guard replaced with safe path (#1020)
- Skill auto-promote skipped skills with no prior trust record in DB (early `Ok(None)` return) (#1022)
- XML injection: `skill.name()` and `skill.description()` are now escaped (`&`, `<`, `>`, `"`)
  before interpolation into XML system prompt in all 4 prompt functions (pre-existing vulnerability,
  fixed in scope of this epic) (#1023)

### Changed
- `tool_kind_from_name`: `"glob"` now maps to `ToolKind::Search` (was `ToolKind::Other`) — consistent with other search-oriented tools (GAP-02)
- `ToolOutput` struct: added `raw_response: Option<serde_json::Value>` field for structured ACP intermediate notification payloads; all existing construction sites default to `None`
- `LoopbackEvent::ToolOutput` variant: added `raw_response: Option<serde_json::Value>` field; propagated through `Channel::send_tool_output` trait and all implementations
- `Channel::send_tool_output` signature extended with `raw_response: Option<serde_json::Value>` parameter (`AnyChannel`, `TuiChannel`, `LoopbackChannel` all updated)
- `zeph-tui`: added `serde_json` as explicit dependency (required by updated `Channel` trait signature)
- `cosine_weight` (default 0.7) and `hybrid_search` (default true) added to `[skills]` config section (#1022, #1023)
- `router_ema_alpha` and `router_reorder_interval` added to `[llm]` config section (#1023)
- `correction_detection`, `correction_confidence_threshold`, `correction_recall_limit`,
  `correction_min_similarity` added to `[agent.learning]` config section (#1021)

## [0.12.3] - 2026-02-27

### Fixed
- Skill matching fallback: when `QdrantSkillMatcher` returns an empty result set (embed error or Qdrant unavailable), the agent now falls back to all registered skills instead of running with an empty active-skill list
- Orchestrator context window detection: `build_provider` now calls `auto_detect_context_window` for `AnyProvider::Orchestrator` so that `auto_budget_tokens` returns a correct value and `prepare_context` injects semantic recall, summaries, and cross-session memories

### Added
- `docs/src/guides/ide-integration.md` — IDE integration guide covering ACP stdio setup, Zed and VS Code configuration, and subagent visibility features (nesting, terminal streaming, agent following) (#1011)
- ACP context window usage widget: `unstable-session-usage` feature enabled in `zeph-acp` by default; `UsageUpdate` (`used`/`size` tokens) now emitted after each LLM response, populating the Context badge in Zed IDE (#1002)
- ACP project rules widget: `project_rules` field on `AcpServerConfig` and `ZephAcpAgent`; session start sends `_meta.projectRules` with basenames of loaded `.claude/rules/*.md` and skill files, populating the "N project rules" badge in Zed IDE (#1002)
- `collect_project_rules` helper in `src/acp.rs` aggregates rule file paths from `cwd/.claude/rules/*.md` and `AgentDeps::skill_paths` (#1002)
- `ZephAcpAgent::with_project_rules()` builder method for supplying rules list to the ACP agent (#1002)
- ACP session history: `GET /sessions` and `GET /sessions/{id}/messages` HTTP endpoints expose persisted session list and event log to ACP clients (#1004)
- Session resume: sending an existing `session_id` reconstructs conversation context from SQLite before the first LLM turn (#1004)
- Session title auto-inference: title truncated from the first user message (`title_max_chars`, default 60) and persisted after the first assistant reply (#1004)
- `[memory.sessions]` config section (`max_history`, `title_max_chars`) in `MemoryConfig` and `config/default.toml` (#1004)
- `sessions list/resume/delete` CLI subcommands (gated behind `acp` feature) (#1004)
- TUI session browser panel (`H` keybind) with `session:history` command palette entry (#1004)
- `SqliteStore::get_acp_session_info()` — single-session lookup with `title`, `updated_at`, `message_count` (#1004)
- `SqliteStore::list_acp_sessions(limit)` enriched with `title`, `updated_at`, `message_count`; `limit=0` returns all (#1004)
- Migration `017_acp_session_updated_at_trigger.sql` — auto-updates `updated_at` on every event insert (#1004)
- `zeph-core::text::truncate_to_chars()` Unicode-aware helper, replaces duplicated truncation in agent and CLI (#1004)
- `created_at` field in `AcpSessionEvent` and `SessionEventDto` REST response (#1004)
- `max_history` wired through `AcpServerConfig` and `ZephAcpAgent`; used in both HTTP handler and agent `list_sessions` (#1004)
- UUID validation on `session_id` path parameter in `session_messages_handler` — returns 400 on invalid input (#1004)
- Startup `tracing::warn!` when `auth_bearer_token` is None and HTTP transport is active (#1004)
- `--init` wizard prompts for `max_history` and `title_max_chars` (#1004)
- `zeph-acp`: `parent_tool_use_id` propagation through `LoopbackEvent::ToolStart/ToolOutput` → `AcpContext` → `loopback_event_to_updates`; subagent events carry `_meta.claudeCode.parentToolUseId` so IDEs can nest subagent output under the parent tool call card (#1008)
- `zeph-core`: `Agent::with_parent_tool_use_id()` builder method; `AgentBuilder` injects the parent tool call UUID when spawning subagents via `SubAgentManager` (#1008)
- `zeph-acp`: `AcpShellExecutor` terminal streaming — `stream_until_exit` helper polls output every 200 ms via `tokio::select!` and emits `ToolCallUpdate` with `_meta.terminal_output` per chunk and `_meta.terminal_exit` on completion; IDEs receive real-time bash output inside tool cards (#1009)
- `zeph-tools`: `locations: Option<Vec<String>>` field on `ToolOutput`; `AcpFileExecutor` populates it with the absolute file path for `read_file`/`write_file` operations; `loopback_event_to_updates` forwards it as `ToolCall.location` for IDE file-following (#1010)
- Unit tests: `loopback_tool_start_parent_tool_use_id_injected_into_meta`, `loopback_tool_output_parent_tool_use_id_injected_into_meta`, `streaming_mode_emits_terminal_exit_notification`, `read_file_returns_location`, `write_file_returns_location` (#1008, #1009, #1010)

### Fixed
- ACP terminal release deferred until after `tool_call_update` notification: IDE now receives `ToolCallContent::Terminal` while the terminal is still alive, enabling tool output display in Zed ACP panel (#1013)
- `TerminalMessage` enum (`Execute`/`Release`) decouples terminal lifecycle from execution in `zeph-acp`; `AcpShellExecutor::release_terminal()` signals the background handler instead of calling the ACP method inline (#1013)
- `SessionEntry` retains a cloned `AcpShellExecutor` so the `prompt()` event loop can trigger deferred `terminal/release` after all `tool_call_update` notifications are dispatched (#1013)
- `ModelInfo` struct (`id`, `display_name`, `context_window`, `created_at`) in `zeph-llm` for dynamic model discovery (#992)
- `ModelCache` in `zeph-llm/src/model_cache.rs`: disk-backed per-provider model list with 24h TTL, atomic writes, `~/.cache/zeph/models/{slug}.json` (#992)
- `LlmProvider::list_models_remote()` async trait method with default fallback to `list_models()` (#992)
- `OllamaProvider::list_models_remote()` via `ollama_rs::list_local_models`; maps parameter size and quantization into `display_name` (#993)
- `ClaudeProvider::list_models_remote()` via paginated `GET /v1/models`; 401/403 errors do not overwrite valid cache (#994)
- `OpenAiProvider::list_models_remote()` via `GET {base_url}/v1/models` with Bearer auth; cache slug derived from sanitized hostname (#995)
- `CompatibleProvider::list_models_remote()` delegates to inner `OpenAiProvider` (#995)
- `AnyProvider::list_models_remote()` dispatches to active inner variant (#996)
- `RouterProvider::list_models_remote()` aggregates models from all fallback providers, deduplicating by `id` (#996)
- `ModelOrchestrator::list_models_remote()` aggregates across all registered sub-providers (#996)
- `Agent::set_model(model_id)` validates input (non-empty, max 256 ASCII printable chars) and hot-swaps provider model (#997)
- `/model` command lists all discovered models with display names and cache age indicator (#997)
- `/model <id>` switches the active model and confirms in chat (#997)
- `/model refresh` clears all provider caches in `~/.cache/zeph/models/` and re-fetches (#997)
- ACP `AvailableCommandsUpdate` populated with model list on session start (#997)

### Fixed
- `SubAgentConfig` in `zeph-core` config with `enabled`, `max_concurrent` (default 1), `extra_dirs` fields; wired into bootstrap via `with_subagent_manager()` on `AgentBuilder` (#973, #964)
- Sub-agent definition discovery from `.zeph/agents/` (project scope) and `~/.config/zeph/agents/` (user scope) with priority-based deduplication (#964)
- Skill injection into sub-agent system prompt: filtered skills prepended as fenced `skills` block at spawn time (#967)
- Foreground sub-agent execution mode: `AgentCommand::Spawn` and `@mention` block the agent loop and stream status updates until the sub-agent completes (#970)
- Secret request/approval protocol via in-process `mpsc` channel: sub-agent emits `[REQUEST_SECRET: key]` marker, main agent prompts user for approval, delivers via `PermissionGrants` without serializing the secret value into message history (#969)
- `tokio::select!` around secret-wait in sub-agent loop to honour `CancellationToken` during approval polling (#969)
- `deny_secret()` sends `None` over the secret channel to immediately unblock a waiting sub-agent (#969)
- `MockProvider::with_recording()` builder in `zeph-llm` for call-inspection in tests (#967)
- Tests for `SubAgentConfig` deserialization, skill injection with and without skills, secret approval and deny flows (#973, #967, #969)
- `zeph-acp`: LSP diagnostics content block (#962): `ContentBlock::Resource` with MIME `application/vnd.zed.diagnostics+json` formatted as `<diagnostics>file:line: [SEVERITY] message</diagnostics>` before the prompt; unknown MIME types logged and skipped
- `zeph-acp`: `AvailableCommandsUpdate` (#961): emitted after `new_session` and `load_session`; slash commands (`/help`, `/model`, `/mode`, `/clear`, `/compact`) dispatched without entering agent loop
- `zeph-acp`: `/compact` command (#979): triggers `AgentContext::compact_context()` via agent-loop sentinel; responds with compaction status or no-op message when history is below minimum threshold; emits `UsageUpdate` after compaction
- `zeph-acp`: `/model` fuzzy matching (#980): case-insensitive multi-token substring match against `available_models` after exact match fails; returns error with candidate list on ambiguous input
- `zeph-acp`: provider model auto-discovery (#983): `LlmProvider::list_models()` default method added; `discover_models_from_config()` auto-populates `available_models` at session start when the config list is empty; static config override takes precedence
- `zeph-core`: `AgentContext::clear_history()` clears in-memory conversation window and deletes session events from SQLite via `memory.delete_conversation()`
- `zeph-acp`: `UsageUpdate` via `unstable-session-usage` feature (#957): token usage emitted after each LLM turn via `LoopbackEvent::Usage`; `LlmProvider::last_usage()` added with `ClaudeProvider` implementation
- `zeph-acp`: `SetSessionModel` via `unstable-session-model` feature (#958): `set_session_model` implemented; validates model against allowed list and swaps provider override
- `zeph-acp`: `SessionInfoUpdate` via `unstable-session-info-update` feature (#959): title generated after first agent response; persisted to SQLite via migration `016_acp_session_title.sql`
- `zeph-acp`: `Plan` session updates (#960): `LoopbackEvent::Plan` variant; `SessionUpdate::Plan` emitted from `loopback_event_to_updates`
- `zeph-core`: `LoopbackEvent::Usage`, `SessionTitle`, `Plan` variants; `PlanItemStatus` enum; `Channel::send_usage` method
- New `zeph-acp` feature flags: `unstable-session-usage`, `unstable-session-model`, `unstable-session-info-update`; all enabled by default

### Fixed
- `zeph-acp`: tool output content now always appears in ACP tool call blocks (Zed IDE); removed `if !already_streamed` guard so `LoopbackEvent::ToolOutput` is emitted unconditionally for all channels including ACP (#1003)
- `zeph-acp`: fenced-block tool execution path now generates a stable UUID `tool_call_id`, emits `ToolStart` before output, and passes the ID to `send_tool_output` — eliminating orphaned `ToolCallUpdate` events with empty ID (#1003)
- `AcpShellExecutor`: `terminal_timeout_secs` config value was silently ignored; now correctly passed to `with_timeout` (#956)
- `tests/integration.rs`: added missing `llm_request_timeout_secs` field in `TimeoutConfig` initializer (#956)
- `zeph-acp`: XML-escape `path`, `severity`, and `message` fields in diagnostics context block to prevent prompt injection (#962)
- `zeph-acp`: trim leading whitespace before slash-command prefix check to prevent bypass via `\n/command` input (#961)
- `zeph-acp`: `/clear` now sends a sentinel to the agent loop to also clear in-memory `AgentContext` state and reset the token counter (#981)

## [0.12.2] - 2026-02-26

### Added
- `MemoryToolExecutor` in `zeph-core` exposes `memory_search` and `memory_save` as native tools the model can invoke explicitly
- `memory_search` queries SemanticMemory recall, key facts, and session summaries; `memory_save` persists content to long-term memory
- `MemoryToolExecutor` registered conditionally — only when memory backend is configured
- `MemoryState.memory` refactored to `Option<Arc<SemanticMemory>>` for shared access
- WebSocket connection lifecycle hardening: `AtomicUsize` slot reservation before upgrade handshake eliminates TOCTOU between capacity check and `DashMap` insertion; 30s ping / 90s pong-timeout keepalive; binary frame rejection with close code 1003; graceful disconnect with 1s write-task drain window to ensure close frame delivery per RFC 6455 (#936)
- Bearer token authentication middleware for ACP HTTP and WebSocket transports (`auth.rs`): constant-time token comparison via `subtle::ConstantTimeEq`, configurable via `acp.auth_bearer_token` / `ZEPH_ACP_AUTH_TOKEN` env var; no-auth open mode when token is unset (#936)
- Agent discovery manifest endpoint `GET /.well-known/acp.json`: returns agent name, version, supported transports, and authentication type; publicly accessible (exempt from bearer auth), controlled by `acp.discovery_enabled` (default `true`) / `ZEPH_ACP_DISCOVERY_ENABLED` env var (#936)
- `AcpConfig` fields: `auth_bearer_token: Option<String>`, `discovery_enabled: bool` (#936)
- `--acp-auth-token` CLI flag for runtime bearer token injection (#936)
- `zeph-core`: `LoopbackEvent::ToolStart { tool_name, tool_call_id }` variant emitted before tool execution; `LoopbackEvent::ToolOutput` extended with `tool_call_id` and `is_error` fields (#926)
- `zeph-core`: `Channel::send_tool_start` method; `LoopbackChannel` emits `ToolStart` events; tool UUIDs generated per execution and threaded through the pipeline (#926)
- `zeph-acp`: ACP tool call lifecycle now emits `SessionUpdate::ToolCall(InProgress)` before execution and `SessionUpdate::ToolCallUpdate(Completed|Failed)` with content after, per protocol spec G5 (#926)
- `zeph-acp`: Configurable terminal command timeout (default 120s) in `AcpShellExecutor`; on timeout calls `kill_terminal_command`, collects partial output, and returns `AcpError::TerminalTimeout` per G6 (#926)
- `zeph-acp`: three unstable ACP session features gated behind cargo feature flags:
  - `unstable-session-list`: implements `session/list` — returns active in-memory sessions with optional `cwd` filter
  - `unstable-session-fork`: implements `session/fork` — clones an existing session (history copied via `import_acp_events`) and spawns a new agent loop
  - `unstable-session-resume`: implements `session/resume` — restores a persisted session without history replay (unlike `session/load`)
- Root `acp-unstable` feature activates all three unstable features for the `zeph` binary; included in `full`
- `initialize()` advertises `SessionCapabilities` (list/fork/resume) when corresponding features are enabled
- `McpToolExecutor` now implements `tool_definitions()` and `execute_tool_call()` — MCP tools are exposed as native `ToolDefinition`s and dispatched via structured tool_use when provider supports it
- `McpToolExecutor` accepts `Arc<RwLock<Vec<McpTool>>>` at construction; shared reference is kept in `McpState.shared_tools` and updated on `/mcp add`/`/mcp remove`
- `append_mcp_prompt()` skips text-based MCP tool injection when `provider.supports_tool_use()` is true, preventing duplicate tool descriptions
- `OllamaProvider` supports native tool calling via `chat_with_tools()` and `supports_tool_use()` when `llm.ollama.tool_use = true` in config
- `OllamaConfig` struct with `tool_use: bool` field (default false) in `LlmConfig`
- `AgentBuilder::with_mcp_shared_tools()` method to wire the shared tool list into the agent
- ACP session modes support: `set_session_mode` method (ask/architect/code), `current_mode_update` notification emission on mode switch, and `availableModes` field in `new_session`/`load_session` responses (#920)
- ACP: `ext_notification` handler logs method name and returns `Ok(())` instead of `method_not_found` (#930)
- ACP: MCP bridge now supports HTTP and SSE server transports — both are mapped to `McpTransport::Http` since rmcp's `StreamableHttpClientTransport` handles both; previously HTTP and SSE servers were silently skipped (#930)
- ACP `AgentCapabilities` now advertises `session_capabilities` with list/fork/resume support (G3) (#922)
- ACP tool call lifecycle: `loopback_event_to_updates` emits `InProgress` then `Completed` `ToolCall` updates per turn (G5) (#922)
- ACP terminal command timeout with `kill_terminal_command` on expiry; configurable via `AcpServerConfig.terminal_timeout_secs` (default 120s) (G6) (#922)
- ACP `ToolCallContent::Terminal` emitted for bash tool calls routed through IDE terminal (G7) (#922)
- ACP `UserMessageChunk` echo notification after user prompt is sent to agent (G10) (#922)
- ACP `list_sessions` implementation (unstable, behind `unstable_session_list` feature) (G12) (#922)
- ACP `fork_session` implementation — copies event history from source session; enforces `max_sessions` with LRU eviction (unstable, behind `unstable_session_fork` feature) (G13) (#922)
- ACP `resume_session` implementation — restores session from SQLite without event replay; enforces `max_sessions` with LRU eviction (unstable, behind `unstable_session_resume` feature) (G14) (#922)

### Changed
- `ToolDef.id` and `ToolDef.description` changed from `&'static str` to `Cow<'static, str>` to support dynamic MCP tool names without memory leaks
- `AgentCapabilities` in `initialize()` now advertises `PromptCapabilities` with `image=true` and `embedded_context=true`, reflecting actual Image and Resource content block support (#917)
- ACP: `AgentCapabilities` in `initialize` response now advertises `config_options` and `ext_methods` support via meta fields (#930)
- ACP unsupported content blocks (`Audio`, `ResourceLink`) now log structured `warn!` with block type/URI instead of silent drop (G9) (#922)
- `ToolOutput` struct gained `terminal_id: Option<String>` field; all call sites updated with `None` (#922)
- `LoopbackEvent::ToolOutput` gained `terminal_id: Option<String>` field (#922)

### Security
- `AcpConfig` now uses custom `impl std::fmt::Debug` that redacts `auth_token` as `[REDACTED]`, consistent with `A2aServerConfig` and `TelegramConfig` (#936)

## [0.12.1] - 2026-02-25

### Security
- Enforce `unsafe_code = "deny"` at workspace lint level; existing unavoidable unsafe blocks (mmap via candle, `std::env` in tests) annotated with `#[allow(unsafe_code)]` (#867)
- Replace `HashMap` with `BTreeMap` in `AgeVaultProvider` to produce deterministic JSON key ordering on `vault.save()` (#876)
- `WebScrapeExecutor`: redirect targets now validated against private/internal IP ranges to prevent SSRF via redirect chains (#871)
- Gateway webhook payload: per-field length limits (sender/channel <= 256 bytes, body <= 65536 bytes) and ASCII control char stripping to prevent prompt injection (#868)
- ACP permission cache: null bytes stripped from tool names before cache key construction to prevent key collision (#872)
- Config validation: `gateway.max_body_size` bounded to 10 MiB (10485760 bytes) to prevent memory exhaustion (#875)
- Shell sandbox: added `<(`, `>(`, `<<<`, `eval ` to default `confirm_patterns` to mitigate process substitution, here-string, and eval bypass vectors; documented known `find_blocked_command` limitations (#870)

### Performance
- `ClaudeProvider` caches pre-serialized `ToolDefinition` slices as `serde_json::Value`; cache is keyed by tool names and invalidated only when the set changes, eliminating per-call JSON construction overhead (#894)

### Added
- `sqlite_pool_size: u32` field in `MemoryConfig` (default 5) — pool size configurable via `[memory] sqlite_pool_size` in config.toml; `SqliteStore::with_pool_size()` wires the value into the connection pool builder (#893)
- Background `tokio::spawn` cleanup task for `ResponseCache::cleanup_expired()` — interval configurable via `[memory] response_cache_cleanup_interval_secs` (default 3600s), first tick skipped to avoid startup overhead (#891)
- 6 new unit tests for `unsummarized_count` counter logic and `sqlite_pool_size` config defaults/deserialization

### Changed
- Removed 4 `channel.send_status()` calls from `persist_message()` in `zeph-core` — each Telegram status update is a blocking API call; SQLite WAL inserts < 1ms don't warrant status reporting (#889)
- `check_summarization()` no longer issues a `COUNT(*)` SQL query on every message save; replaced with in-memory `unsummarized_count: usize` counter on `MemoryState` — incremented on persist, reset on summarization (#890)
- `tui_loop()` in `zeph-tui` skips `terminal.draw()` when no events occurred in the 250ms tick — reduces idle CPU usage (#892)
- `.cargo/config.toml` with sccache `rustc-wrapper` for workspace build caching (#877)
- `[profile.ci]` build profile with thin LTO and 16 codegen-units for faster CI release builds (#878)
- `schema` feature flag in `zeph-llm` gating `schemars` dependency and typed output API (#879)

### Performance
- Replace `should_compact()` O(N) message scan with direct comparison against `cached_prompt_tokens` (#880)
- Cache `EnvironmentContext` on Agent; refresh only `git_branch` on skill reload instead of spawning a full git subprocess each time (#881)
- Hash doom-loop content in-place by feeding stable segments directly into the hasher, eliminating the intermediate normalized `String` allocation (#882)
- Fix double `count_tokens` call in `prune_stale_tool_outputs` for `ToolResult` parts; compute once and reuse (#883)
- Added composite covering index `(conversation_id, id)` on `messages` table (migration 015); replaces single-column index for filter+order access patterns in `oldest_message_ids` and `load_history_filtered` (#895)
- Replaced double-sort subquery in `load_history_filtered` with a CTE — eliminates redundant `ORDER BY` on the derived table (#896)
- Eliminate redundant `Vec<Message>` clone in `remove_tool_responses_middle_out` by taking ownership instead of borrowing; replace `HashSet` with `Vec::with_capacity` for small-N index tracking (#884, #888)
- Fast-path empty `parts_json == "[]"` deserialization in `load_history`, `load_history_filtered`, `message_by_id`, `messages_by_ids` to skip serde parse on the common empty case (#886)
- Replace `collect::<Vec<_>>().join()` in `consolidate_summaries` with `String::with_capacity` + `write!` loop to eliminate intermediate allocation (#887)

### Changed
- Replace default Ollama model `mistral:7b` with `qwen3:8b` across config defaults, tests, snapshots, and `--init` wizard; add `"qwen3"/"qwen"` as `ChatML` aliases in `ChatTemplate::parse_str` (#897)
- Split 3177-line `src/main.rs` into focused modules: `runner.rs` (dispatch), `agent_setup.rs` (tool/MCP/feature setup), `tracing_init.rs`, `tui_bridge.rs`, `channel.rs`, `tests.rs` — `main.rs` reduced to 26 LOC (#839)
- Split 1791-line `crates/zeph-core/src/bootstrap.rs` into submodule directory: `config.rs`, `health.rs`, `mcp.rs`, `provider.rs`, `skills.rs`, `tests.rs` — `bootstrap/mod.rs` reduced to 278 LOC (#840)
- Replace `source_kind: String` in `SkillTrustRow` with `SourceKind` enum (`Local`, `Hub`, `File`) with serde DB serialization; invalid values fail at parse time (#848)
- Replace `kind: String` in `ScheduledTaskConfig` with `ScheduledTaskKind` enum (`MemoryCleanup`, `SkillRefresh`, `HealthCheck`, `UpdateCheck`, `Custom`); invalid values fail at parse time (#850)
- Replace unjustified `#[allow(dead_code)]` with `#[expect(dead_code, reason = "...")]` or remove suppression and add doc comments across zeph-a2a, zeph-tools, zeph-core, zeph-acp (#849)
- `A2aServer::serve()` emits `tracing::warn!` when `auth_token` is `None`, signalling unauthenticated exposure (#869)
- `GatewayServer::serve()` emits `tracing::warn!` when `auth_token` is `None`, signalling unauthenticated exposure (#873)
- Moved `TrustLevel` enum to `zeph-tools::trust_level`; `zeph-skills` re-exports it, breaking the `zeph-tools → zeph-skills` reverse dependency (#841)
- Removed duplicate `ChannelError` from `zeph-channels::error`; all channel adapters use `zeph_core::channel::ChannelError` (#842)
- Replaced `zeph_a2a::types::TaskState` in `zeph-core` with a local `SubAgentState` enum; removed `zeph-a2a` from `zeph-core` dependencies (#843)
- Consolidated Qdrant access in `zeph-index` through `zeph-memory::VectorStore` trait; removed direct `qdrant-client` dependency from `zeph-index` (#844)
- Added `content_hash(data: &[u8]) -> String` utility in `zeph-core::hash` backed by BLAKE3 (#845)
- Removed `zeph-core::diff` re-export module; `zeph_core::DiffData` is now a direct re-export of `zeph_tools::executor::DiffData` (#846)
- Extract ContextManager, ToolOrchestrator, LearningEngine from Agent god object into standalone structs with pure delegation (#830, #836, #837, #838)
- Secret type wraps inner value in `Zeroizing<String>` for memory zeroization on drop; `Clone` removed (#865)
- AgeVaultProvider secrets HashMap uses `Zeroizing<String>` values (#866)
- Age private key reads, decrypt plaintext buffer, and encrypt JSON buffer wrapped in `Zeroizing` (#874)

## [0.12.0] - 2026-02-24

### Added
- ACP custom methods framework via `ext_method` dispatch — `_session/list`, `_session/get`, `_session/delete`, `_session/export`, `_session/import`, `_agent/tools`, `_agent/working_dir/update` (#787)
- Session export/import with SQLite transaction-backed atomic event replay (#787)
- Auth hints in ACP `initialize` response meta (#787)
- `validate_session_id` guard (len≤128, `[a-zA-Z0-9_-]`) on all session methods (#787)
- Path traversal protection in `_agent/working_dir/update` (#787)
- `MAX_IMPORT_EVENTS` cap (10,000) to prevent unbounded import DoS (#787)
- `list_acp_sessions` and `import_acp_events` methods in `SqliteStore` (#787)
- Tool-pair summarization — `maybe_summarize_tool_pair()` summarizes oldest tool call/response pairs when visible count exceeds `tool_call_cutoff` (default 6) (#793)
- XML-delimited prompt in `build_tool_pair_summary_prompt()` to prevent prompt injection from tool output
- `[memory] tool_call_cutoff` config option with validation (`>= 1`)
- Reactive compaction on `ContextLengthExceeded` — auto-compact and retry LLM calls up to 2 times (#792)
- `ContextLengthExceeded` error variant in `LlmError` with provider-specific pattern detection (Claude, OpenAI, Ollama)
- Middle-out progressive tool response removal fallback during summarization (10/20/50/100% tiers)
- Structured 9-section compaction prompt (User Intent, Technical Concepts, Files & Code, Errors & Fixes, Problem Solving, User Messages, Pending Tasks, Current Work, Next Step)
- `build_metadata_summary()` — LLM-free final fallback with safe UTF-8 truncation
- `MessageMetadata` struct in `zeph-llm` with `agent_visible`, `user_visible`, `compacted_at` fields; default is both-visible for backward compat (#M28)
- `Message.metadata` field with `#[serde(default)]` — existing serialized messages deserialize without change
- SQLite migration `013_message_metadata.sql` — adds `agent_visible`, `user_visible`, `compacted_at` columns to `messages` table
- `save_message_with_metadata()` in `SqliteStore` for saving messages with explicit visibility flags
- `load_history_filtered()` in `SqliteStore` — SQL-level filtering by `agent_visible` / `user_visible`
- `replace_conversation()` in `SqliteStore` — atomic compaction: marks originals `user_only`, inserts summary as `agent_only`
- `oldest_message_ids()` in `SqliteStore` — returns N oldest message IDs for a conversation
- `Agent.load_history()` now loads only `agent_visible=true` messages, excluding compacted originals
- `compact_context()` persists compaction atomically via `replace_conversation()`, falling back to legacy summary storage if DB IDs are unavailable
- Multi-session ACP support with configurable `max_sessions` (default 4) and LRU eviction of idle sessions (#781)
- `session_idle_timeout_secs` config for automatic session cleanup (default 30 min) with background reaper task (#781)
- `ZEPH_ACP_MAX_SESSIONS` and `ZEPH_ACP_SESSION_IDLE_TIMEOUT_SECS` env overrides (#781)
- ACP session persistence to `SQLite` — `acp_sessions` and `acp_session_events` tables with conversation replay on `load_session` per ACP spec (#782)
- `SqliteStore` methods for ACP session lifecycle: `create_acp_session`, `save_acp_event`, `load_acp_events`, `delete_acp_session`, `acp_session_exists` (#782)
- `TokenCounter` in `zeph-memory` — accurate token counting with `tiktoken-rs` cl100k_base, replacing `chars/4` heuristic (#789)
- DashMap-backed token cache (10k cap) for amortized O(1) lookups
- OpenAI tool schema token formula for precise context budget allocation
- Input size guard (64KB) on token counting to prevent cache pollution from oversized input
- Graceful fallback to `chars/4` when tiktoken tokenizer is unavailable
- Configurable tool response offload — `OverflowConfig` with threshold (default 50k chars), retention (7 days), optional custom dir (#791)
- `[tools.overflow]` section in `config.toml` for offload configuration
- Security hardening: path canonicalization, symlink-safe cleanup, 0o600 file permissions on Unix
- Wire `AcpContext` (IDE-proxied FS, shell, permissions) through `AgentSpawner` into agent tool chain via `CompositeExecutor` — ACP executors take priority with automatic local fallback (#779)
- `DynExecutor` newtype in `zeph-tools` for object-safe `ToolExecutor` composition in `CompositeExecutor` (#779)
- `cancel_signal: Arc<Notify>` on `LoopbackHandle` for cooperative cancellation between ACP sessions and agent loop (#780)
- `with_cancel_signal()` builder method on `Agent` to inject external cancellation signal (#780)
- `zeph-acp` crate — ACP (Agent Client Protocol) server for IDE embedding (Zed, JetBrains, Neovim) (#763-#766)
- `--acp` CLI flag to launch Zeph as an ACP stdio server (requires `acp` feature)
- `acp` feature gate in root `Cargo.toml`; included in `full` feature set
- `ZephAcpAgent` implementing SDK `Agent` trait with session lifecycle (new, prompt, cancel, load)
- `loopback_event_to_update` mapping `LoopbackEvent` variants to ACP `SessionUpdate` notifications, with empty chunk filtering
- `serve_stdio()` transport using `AgentSideConnection` over tokio-compat stdio streams
- Stream monitor gated behind `ZEPH_ACP_LOG_MESSAGES` env var for JSON-RPC traffic debugging
- Custom mdBook theme with Zeph brand colors (navy+amber palette from TUI)
- Z-letter favicon SVG for documentation site
- Sidebar logo via inline data URI
- Navy as default documentation theme
- `AcpConfig` struct in `zeph-core` — `enabled`, `agent_name`, `agent_version` with `ZEPH_ACP_*` env overrides (#771)
- `[acp]` section in `config.toml` for configuring ACP server identity
- `--acp-manifest` CLI flag — prints ACP agent manifest JSON to stdout for IDE discovery (#772)
- `serve_connection<W, R>` generic transport function extracted from `serve_stdio` for testability (#770)
- `ConnSlot` pattern in transport — `Rc<RefCell<Option<Rc<AgentSideConnection>>>>` populated post-construction so `new_session` can build ACP adapters (#770)
- `build_acp_context` in `ZephAcpAgent` — wires `AcpFileExecutor`, `AcpShellExecutor`, `AcpPermissionGate` per session (#770)
- `AcpServerConfig` passed through `serve_stdio`/`serve_connection` to configure agent identity from config values (#770)
- ACP section in `--init` wizard — prompts for `enabled`, `agent_name`, `agent_version` (#771)
- Integration tests for ACP transport using `tokio::io::duplex` — `initialize_handshake`, `new_session_and_cancel` (#773)
- ACP permission persistence to `~/.config/zeph/acp-permissions.toml` — `AllowAlways`/`RejectAlways` decisions survive restarts (#786)
- `acp.permission_file` config and `ZEPH_ACP_PERMISSION_FILE` env override for custom permission file path (#786)
- Multi-modal ACP prompts — image and embedded resource content blocks forwarded to LLM providers (#784)
- Tool output locations for IDE file navigation via `ToolCallLocation` (#784)
- Runtime model switching via `set_session_config_option` with provider allowlist validation (#785)
- `ProviderFactory` closure-based provider creation for dynamic model switching (#785)
- MCP extension management via `ext_method` — `_agent/mcp/add`, `_agent/mcp/remove`, `_agent/mcp/list` (#785)
- `provider_override` with `Arc<RwLock>` and poison recovery in agent loop (#785)
- `available_models` configuration in `AcpConfig` (#785)
- `with_provider_override()` builder method on `Agent` (#785)
- HTTP+SSE transport for ACP — POST `/acp` with SSE response stream, GET `/acp` for notification reconnect (#783)
- WebSocket transport for ACP — GET `/acp/ws` with bidirectional messaging (#783)
- Duplex bridge pattern for HTTP/WS connections — `tokio::io::duplex` bridging axum handlers to ACP SDK (#783)
- `AcpTransport` enum (`Stdio`/`Http`/`Both`) and `http_bind` config field in `[acp]` section (#783)
- `acp-http` feature gate for HTTP+WS transport dependencies (#783)
- Session routing via `Acp-Session-Id` header with UUID validation (#783)
- Body size limit (1 MiB), WS message size limit, max_sessions enforcement (503), CORS deny-all (#783)
- SSE keepalive pings (15s interval) and idle reaper with `last_activity` tracking (#783)

### Fixed
- Permission cache key collision on anonymous tools — uses `tool_call_id` as fallback when title is absent (#779)

### Changed
- CI: add CLA check for external contributors via `contributor-assistant/github-action`

## [0.11.6] - 2026-02-23

### Fixed
- Auto-create parent directories for `sqlite_path` on startup (#756)

### Added
- `autosave_assistant` and `autosave_min_length` config fields in `MemoryConfig` — assistant responses skip embedding when disabled (#748)
- `SemanticMemory::save_only()` — persist message to SQLite without generating a vector embedding (#748)
- `ResponseCache` in `zeph-memory` — SQLite-backed LLM response cache with blake3 key hashing and TTL expiry (#750)
- `response_cache_enabled` and `response_cache_ttl_secs` config fields in `LlmConfig` (#750)
- Background `cleanup_expired()` task for response cache (runs every 10 minutes) (#750)
- `ZEPH_MEMORY_AUTOSAVE_ASSISTANT`, `ZEPH_MEMORY_AUTOSAVE_MIN_LENGTH` env overrides (#748)
- `ZEPH_LLM_RESPONSE_CACHE_ENABLED`, `ZEPH_LLM_RESPONSE_CACHE_TTL_SECS` env overrides (#750)
- `MemorySnapshot`, `export_snapshot()`, `import_snapshot()` in `zeph-memory/src/snapshot.rs` (#749)
- `zeph memory export <path>` and `zeph memory import <path>` CLI subcommands (#749)
- SQLite migration `012_response_cache.sql` for the response cache table (#750)
- Temporal decay scoring in `SemanticMemory::recall()` — time-based score attenuation with configurable half-life (#745)
- MMR (Maximal Marginal Relevance) re-ranking in `SemanticMemory::recall()` — post-processing for result diversity (#744)
- Compact XML skills prompt format (`format_skills_prompt_compact`) for low-budget contexts (#747)
- `SkillPromptMode` enum (`full`/`compact`/`auto`) with auto-selection based on context budget (#747)
- Adaptive chunked context compaction — parallel chunk summarization via `join_all` (#746)
- `with_ranking_options()` builder for `SemanticMemory` to configure temporal decay and MMR
- `message_timestamps()` method on `SqliteStore` for Unix epoch retrieval via `strftime`
- `get_vectors()` method on `EmbeddingStore` for raw vector fetch from SQLite `vector_points`
- SQLite-backed `SqliteVectorStore` as embedded alternative to Qdrant for zero-dependency vector search (#741)
- `vector_backend` config option to select between `qdrant` and `sqlite` vector backends
- Credential scrubbing in LLM context pipeline via `scrub_content()` — redacts secrets and paths before LLM calls (#743)
- `redact_credentials` config option (default: true) to toggle context scrubbing
- Filter diagnostics mode: `kept_lines` tracking in `FilterResult` for all 9 filter strategies
- TUI expand ('e') highlights kept lines vs filtered-out lines with dim styling and legend
- Markdown table rendering in TUI chat panel — Unicode box-drawing borders, bold headers, column auto-width

### Changed
- Token estimation uses `chars/4` heuristic instead of `bytes/3` for better accuracy on multi-byte text (#742)

## [0.11.5] - 2026-02-22

### Added
- Declarative TOML-based output filter engine with 9 strategy types: `strip_noise`, `truncate`, `keep_matching`, `strip_annotated`, `test_summary`, `group_by_rule`, `git_status`, `git_diff`, `dedup`
- Embedded `default-filters.toml` with 25 pre-configured rules for CLI tools (cargo, git, docker, npm, pip, make, pytest, go, terraform, kubectl, brew, ls, journalctl, find, grep/rg, curl/wget, du/df/ps, jest/mocha/vitest, eslint/ruff/mypy/pylint)
- `filters_path` option in `FilterConfig` for user-provided filter rules override
- ReDoS protection: RegexBuilder with size_limit, 512-char pattern cap, 1 MiB file size limit
- Dedup strategy with configurable normalization patterns and HashMap pre-allocation
- NormalizeEntry replacement validation (rejects unescaped `$` capture group refs)
- Sub-agent orchestration system with A2A protocol integration (#709)
- Sub-agent definition format with TOML frontmatter parser (#710)
- `SubAgentManager` with spawn/cancel/collect lifecycle (#711)
- Tool filtering (AllowList/DenyList/InheritAll) and skill filtering with glob patterns (#712)
- Zero-trust permission model with TTL-based grants and automatic revocation (#713)
- In-process A2A channels for orchestrator-to-sub-agent communication
- `PermissionGrants` with audit trail via tracing
- Real LLM loop wired into `SubAgentManager::spawn()` with background tokio task execution (#714)
- `poll_subagents()` on `Agent<C>` for collecting completed sub-agent results (#714)
- `shutdown_all()` on `SubAgentManager` for graceful teardown (#714)
- `SubAgentMetrics` in `MetricsSnapshot` with state, turns, elapsed time (#715)
- TUI sub-agents panel (`zeph-tui` widgets/subagents) with color-coded states (#715)
- `/agent` CLI commands: `list`, `spawn`, `bg`, `status`, `cancel`, `approve`, `deny` (#716)
- Typed `AgentCommand` enum with `parse()` for type-safe command dispatch replacing string matching in the agent loop
- `@agent_name` mention syntax for quick sub-agent invocation with disambiguation from `@`-triggered file references

### Changed
- Migrated all 6 hardcoded filters (cargo_build, test_output, clippy, git, dir_listing, log_dedup) into the declarative TOML engine

### Removed
- `FilterConfig` per-filter config structs (`TestFilterConfig`, `GitFilterConfig`, `ClippyFilterConfig`, `CargoBuildFilterConfig`, `DirListingFilterConfig`, `LogDedupFilterConfig`) — filter params now in TOML strategy fields

## [0.11.4] - 2026-02-21

### Added
- `validate_skill_references(body, skill_dir)` in zeph-skills loader: parses Markdown links targeting `references/`, `scripts/`, or `assets/` subdirs, warns on missing files and symlink traversal attempts (#689)
- `sanitize_skill_body(body)` in zeph-skills prompt: escapes XML structural tags (`<skill`, `</skill>`, `<instructions`, `</instructions>`, `<available_skills`, `</available_skills>`) to prevent prompt injection (#689)
- Body sanitization applied automatically to all non-`Trusted` skills in `format_skills_prompt()` (#689)
- `load_skill_resource(skill_dir, relative_path)` public function in `zeph-skills::resource` for on-demand loading of skill resource files with path traversal protection (#687)
- Nested `metadata:` block support in SKILL.md frontmatter: indented key-value pairs under `metadata:` are parsed as structured metadata (#686)
- Field length validation in SKILL.md loader: `description` capped at 1024 characters, `compatibility` capped at 500 characters (#686)
- Warning log in `load_skill_body()` when body exceeds 20,000 bytes (~5000 tokens) per spec recommendation (#686)
- Empty value normalization for `compatibility` and `license` frontmatter fields: bare `compatibility:` now produces `None` instead of `Some("")` (#686)
- `SkillManager` in zeph-skills — install skills from git URLs or local paths, remove, verify blake3 integrity, list with trust metadata
- CLI subcommands: `zeph skill {install, remove, list, verify, trust, block, unblock}` — runs without agent loop
- In-session `/skill install <url|path>` and `/skill remove <name>` with hot reload
- Managed skills directory at `~/.config/zeph/skills/`, auto-appended to `skills.paths` at bootstrap
- Hash re-verification on trust promotion — recomputes blake3 before promoting to trusted/verified, rejects on mismatch
- URL scheme allowlist and path traversal validation in SkillManager as defense-in-depth
- Blocking I/O wrapped in `spawn_blocking` for async safety in skill management handlers
- `custom: HashMap<String, Secret>` field in `ResolvedSecrets` for user-defined vault secrets (#682)
- `list_keys()` method on `VaultProvider` trait with implementations for Age and Env backends (#682)
- `requires-secrets` field in SKILL.md frontmatter for declaring per-skill secret dependencies (#682)
- Gate skill activation on required secrets availability in system prompt builder (#682)
- Inject active skill's secrets as scoped env vars into `ShellExecutor` at execution time (#682)
- Custom secrets step in interactive config wizard (`--init`) (#682)
- crates.io publishing metadata (description, readme, homepage, keywords, categories) for all workspace crates (#702)

### Changed
- `requires-secrets` SKILL.md frontmatter field renamed to `x-requires-secrets` to follow JSON Schema vendor extension convention and avoid future spec collisions — **breaking change**: update skill frontmatter to use `x-requires-secrets`; the old `requires-secrets` form is still parsed with a deprecation warning (#688)
- `allowed-tools` SKILL.md field now uses space-separated values per agentskills.io spec (was comma-separated) — **breaking change** for skills using comma-delimited allowed-tools (#686)
- Skill resource files (references, scripts, assets) are no longer eagerly injected into the system prompt on skill activation; only filenames are listed as available resources — **breaking change** for skills relying on auto-injected reference content (#687)

## [0.11.3] - 2026-02-20

### Added
- `LoopbackChannel` / `LoopbackHandle` / `LoopbackEvent` in zeph-core — headless channel for daemon mode, pairs with a handle that exposes `input_tx` / `output_rx` for programmatic agent I/O
- `ProcessorEvent` enum in zeph-a2a server — streaming event type replacing synchronous `ProcessResult`; `TaskProcessor::process` now accepts an `mpsc::Sender<ProcessorEvent>` and returns `Result<(), A2aError>`
- `--daemon` CLI flag (feature `daemon+a2a`) — bootstraps a full agent + A2A JSON-RPC server under `DaemonSupervisor` with PID file lifecycle and Ctrl-C graceful shutdown
- `--connect <URL>` CLI flag (feature `tui+a2a`) — connects the TUI to a remote daemon via A2A SSE, mapping `TaskEvent` to `AgentEvent` in real-time
- Command palette daemon commands: `daemon:connect`, `daemon:disconnect`, `daemon:status`
- Command palette action commands: `app:quit` (shortcut `q`), `app:help` (shortcut `?`), `session:new`, `app:theme`
- Fuzzy-matching for command palette — character-level gap-penalty scoring replaces substring filter; `daemon_command_registry()` merged into `filter_commands`
- `TuiCommand::ToggleTheme` variant in command palette (placeholder — theme switching not yet implemented)
- `--init` wizard daemon step — prompts for A2A server host, port, and auth token; writes `config.a2a.*`
- Snapshot tests for `Config::default()` TOML serialization (zeph-core), git filter diff/status output, cargo-build filter success/error output, and clippy grouped warnings output — using insta for regression detection
- Tests for `handle_tool_result` covering blocked, cancelled, sandbox violation, empty output, exit-code failure, and success paths (zeph-core agent/tool_execution.rs)
- Tests for `maybe_redact` (redaction enabled/disabled) and `last_user_query` helper in agent/tool_execution.rs
- Tests for `handle_skill_command` dispatch covering unknown subcommand, missing arguments, and no-memory early-exit paths for stats, versions, activate, approve, and reset subcommands (zeph-core agent/learning.rs)
- Tests for `record_skill_outcomes` noop path when no active skills are present
- `insta` added to workspace dev-dependencies and to zeph-core and zeph-tools crate dev-deps
- `Embeddable` trait and `EmbeddingRegistry<T>` in zeph-memory — generic Qdrant sync/search extracted from duplicated code in QdrantSkillMatcher and McpToolRegistry (~350 lines removed)
- MCP server command allowlist validation — only permitted commands (npx, uvx, node, python3, python, docker, deno, bun) can spawn child processes; configurable via `mcp.allowed_commands`
- MCP env var blocklist — blocks 21 dangerous variables (LD_PRELOAD, DYLD_*, NODE_OPTIONS, PYTHONPATH, JAVA_TOOL_OPTIONS, etc.) and BASH_FUNC_* prefix from MCP server processes
- Path separator rejection in MCP command validation to prevent symlink-based bypasses

### Changed
- `MessagePart::Image` variant now holds `Box<ImageData>` instead of inline fields, improving semantic grouping of image data
- `Agent<C, T>` simplified to `Agent<C>` — ToolExecutor generic replaced with `Box<dyn ErasedToolExecutor>`, reducing monomorphization
- Shell command detection rewritten from substring matching to tokenizer-based pipeline with escape normalization, eliminating bypass vectors via backslash insertion, hex/octal escapes, quote splitting, and pipe chains
- Shell sandbox path validation now uses `std::path::absolute()` as fallback when `canonicalize()` fails on non-existent paths
- Blocked command matching extracts basename from absolute paths (`/usr/bin/sudo` now correctly blocked)
- Transparent wrapper commands (`env`, `command`, `exec`, `nice`, `nohup`, `time`, `xargs`) are skipped to detect the actual command
- Default confirm patterns now include `$(` and backtick subshell expressions
- Enable SQLite WAL mode with SYNCHRONOUS=NORMAL for 2-5x write throughput (#639)
- Replace O(n*iterations) token scan with cached_prompt_tokens in budget checks (#640)
- Defer maybe_redact to stream completion boundary instead of per-chunk (#641)
- Replace format_tool_output string allocation with Write-into-buffer (#642)
- Change ToolCall.params from HashMap to serde_json::Map, eliminating clone (#643)
- Pre-join static system prompt sections into LazyLock<String> (#644)
- Replace doom-loop string history with content hash comparison (#645)
- Return &'static str from detect_image_mime with case-insensitive matching (#646)
- Replace block_on in history persist with fire-and-forget async spawn (#647)
- Change `LlmProvider::name()` from `&'static str` to `&str`, eliminating `Box::leak` memory leak in CompatibleProvider (#633)
- Extract rate-limit retry helper `send_with_retry()` in zeph-llm, deduplicating 3 retry loops (#634)
- Extract `sse_to_chat_stream()` helpers shared by Claude and OpenAI providers (#635)
- Replace double `AnyProvider::clone()` in `embed_fn()` with single `Arc` clone (#636)
- Add `with_client()` builder to ClaudeProvider and OpenAiProvider for shared `reqwest::Client` (#637)
- Cache `JsonSchema` per `TypeId` in `chat_typed` to avoid per-call schema generation (#638)
- Scrape executor performs post-DNS resolution validation against private/loopback IPs with pinned address client to prevent SSRF via DNS rebinding
- Private host detection expanded to block `*.localhost`, `*.internal`, `*.local` domains
- A2A error responses sanitized: serde details and method names no longer exposed to clients
- Rate limiter rejects new clients with 429 when entry map is at capacity after stale eviction
- Secret redaction regex-based pattern matching replaces whitespace tokenizer, detecting secrets in URLs, JSON, and quoted strings
- Added `hf_`, `npm_`, `dckr_pat_` to secret redaction prefixes
- A2A client stream errors truncate upstream body to 256 bytes
- Add `default_client()` HTTP helper with standard timeouts and user-agent in zeph-core and zeph-llm (#666)
- Replace 5 production `Client::new()` calls with `default_client()` for consistent HTTP config (#667)
- Decompose agent/mod.rs (2602→459 lines) into tool_execution, message_queue, builder, commands, and utils modules (#648, #649, #650)
- Replace `anyhow` in `zeph-core::config` with typed `ConfigError` enum (Io, Parse, Validation, Vault)
- Replace `anyhow` in `zeph-tui` with typed `TuiError` enum (Io, Channel); simplify `handle_event()` return to `()`
- Sort `[workspace.dependencies]` alphabetically in root Cargo.toml

### Fixed
- False positive: "sudoku" no longer matched by "sudo" blocked pattern (word-boundary matching)
- PID file creation uses `OpenOptions::create_new(true)` (O_CREAT|O_EXCL) to prevent TOCTOU symlink attacks

## [0.11.2] - 2026-02-19

### Added
- `base_url` and `language` fields in `[llm.stt]` config for OpenAI-compatible local whisper servers (e.g. whisper.cpp)
- `ZEPH_STT_BASE_URL` and `ZEPH_STT_LANGUAGE` environment variable overrides
- Whisper API provider now passes `language` parameter for accurate non-English transcription
- Documentation for whisper.cpp server setup with Metal acceleration on macOS
- Per-sub-provider `base_url` and `embedding_model` overrides in orchestrator config
- Full orchestrator example with cloud + local + STT in default.toml
- All previously undocumented config keys in default.toml (`agent.auto_update_check`, `llm.stt`, `llm.vision_model`, `skills.disambiguation_threshold`, `tools.filters.*`, `tools.permissions`, `a2a.auth_token`, `mcp.servers.env`)

### Fixed
- Outdated config keys in default.toml: removed nonexistent `repo_id`, renamed `provider_type` to `type`, corrected candle defaults, fixed observability exporter default
- Add `wait(true)` to Qdrant upsert and delete operations for read-after-write consistency, fixing flaky `ingested_chunks_have_correct_payload` integration test (#567)
- Vault age backend now falls back to default directory for key/path when `--vault-key`/`--vault-path` are not provided, matching `zeph vault init` behavior (#613)

### Changed
- Whisper STT provider no longer requires OpenAI API key when `base_url` points to a local server
- Orchestrator sub-providers now resolve `base_url` and `embedding_model` via fallback chain: per-provider, parent section, global default

## [0.11.1] - 2026-02-19

### Added
- Persistent CLI input history with rustyline: arrow key navigation, prefix search, line editing, SQLite-backed persistence across restarts (#604)
- Clickable markdown links in TUI via OSC 8 hyperlinks — `[text](url)` renders as terminal-clickable link with URL sanitization and scheme allowlist (#580)
- `@`-triggered fuzzy file picker in TUI input — type `@` to search project files by name/path/extension with real-time filtering (#600)
- Command palette in TUI with read-only agent management commands (#599)
- Orchestrator provider option in `zeph init` wizard for multi-model routing setup (#597)
- `zeph vault` CLI subcommands: `init` (generate age keypair), `set` (store secret), `get` (retrieve secret), `list` (show keys), `rm` (remove secret) (#598)
- Atomic file writes for vault operations with temp+rename strategy (#598)
- Default vault directory resolution via XDG_CONFIG_HOME / APPDATA / HOME (#598)
- Auto-update check via GitHub Releases API with configurable scheduler task (#588)
- `auto_update_check` config field (default: true) with `ZEPH_AUTO_UPDATE_CHECK` env override
- `TaskKind::UpdateCheck` variant and `UpdateCheckHandler` in zeph-scheduler
- One-shot update check at startup when scheduler feature is disabled
- `--init` wizard step for auto-update check configuration

### Fixed
- Restore `--vault`, `--vault-key`, `--vault-path` CLI flags lost during clap migration (#587)

### Changed
- Refactor `AppBuilder::from_env()` to `AppBuilder::new()` with explicit CLI overrides
- Eliminate redundant manual `std::env::args()` parsing in favor of clap
- Add `ZEPH_VAULT_KEY` and `ZEPH_VAULT_PATH` environment variable support
- Init wizard reordered: vault backend selection is now step 1 before LLM provider (#598)
- API key and channel token prompts skipped when age vault backend is selected (#598)

## [0.11.0] - 2026-02-19

### Added
- Vision (image input) support across Claude, OpenAI, and Ollama providers (#490)
- `MessagePart::Image` content type with base64 serialization
- `LlmProvider::supports_vision()` trait method for runtime capability detection
- Claude structured content with `AnthropicContentBlock::Image` variant
- OpenAI array content format with `image_url` data-URI encoding
- Ollama `with_images()` support with optional `vision_model` config for dedicated model routing
- `/image <path>` command in CLI and TUI channels
- Telegram photo message handling with pre-download size guard
- `vision_model` field in `[llm.ollama]` config section and `--init` wizard update
- 20 MB max image size limit and path traversal protection
- Interactive configuration wizard via `zeph init` subcommand with 5-step setup (LLM provider, memory, channels, secrets backend, config generation)
- clap-based CLI argument parsing with `--help`, `--version` support
- `Serialize` derive on `Config` and all nested types for TOML generation
- `dialoguer` dependency for interactive terminal prompts
- Structured LLM output via `chat_typed<T>()` on `LlmProvider` trait with JSON schema enforcement (#456)
- OpenAI/Compatible native `response_format: json_schema` structured output (#457)
- Claude structured output via forced tool use pattern (#458)
- `Extractor<T>` utility for typed data extraction from LLM responses (#459)
- TUI test automation infrastructure: EventSource trait abstraction, insta widget snapshot tests, TestBackend integration tests, proptest layout verification, expectrl E2E terminal tests (#542)
- CI snapshot regression pipeline with `cargo insta test --check` (#547)
- Pipeline API with composable, type-safe `Step` trait, `Pipeline` builder, `ParallelStep` combinator, and built-in steps (`LlmStep`, `RetrievalStep`, `ExtractStep`, `MapStep`) (#466, #467, #468)
- Structured intent classification for skill disambiguation: when top-2 skill scores are within `disambiguation_threshold` (default 0.05), agent calls LLM via `chat_typed::<IntentClassification>()` to select the best-matching skill (#550)
- `ScoredMatch` struct exposing both skill index and cosine similarity score from matcher backends
- `IntentClassification` type (`skill_name`, `confidence`, `params`) with `JsonSchema` derive for schema-enforced LLM responses
- `disambiguation_threshold` in `[skills]` config section (default: 0.05) with `with_disambiguation_threshold()` builder on `Agent`
- DocumentLoader trait with text/markdown file loader in zeph-memory (#469)
- Text splitter with configurable chunk size, overlap, and sentence-aware splitting (#470)
- PDF document loader, feature-gated behind `pdf` (#471)
- Document ingestion pipeline: load, split, embed, store via Qdrant (#472)
- File size guard (50 MiB default) and path canonicalization for document loaders
- Audio input support: `Attachment`/`AttachmentKind` types, `SpeechToText` trait, OpenAI Whisper backend behind `stt` feature flag (#520, #521, #522)
- Telegram voice and audio message handling with automatic file download (#524)
- STT bootstrap wiring: `WhisperProvider` created from `[llm.stt]` config behind `stt` feature (#529)
- Slack audio file upload handling with host validation and size limits (#525)
- Local Whisper backend via candle for offline STT with symphonia audio decode and rubato resampling (#523)
- Shell-based installation script (`install/install.sh`) with SHA256 verification, platform detection, and `--version` flag
- Shellcheck lint job in CI pipeline
- Per-job permission scoping in release workflow (least privilege)
- TUI word-jump and line-jump cursor navigation (#557)
- TUI keybinding help popup on `?` in normal mode (#533)
- TUI clickable hyperlinks via OSC 8 escape sequences (#530)
- TUI edit-last-queued for recalling queued messages (#535)
- VectorStore trait abstraction in zeph-memory (#554)
- Operation-level cancellation for LLM requests and tool executions (#538)

### Changed
- Consolidate Docker files into `docker/` directory (#539)
- Typed deserialization for tool call params (#540)
- CI: replace oraclelinux base image with debian bookworm-slim (#532)

### Fixed
- Strip schema metadata and fix doom loop detection for native tool calls (#534)
- TUI freezes during fast LLM streaming and parallel tool execution: biased event loop with input priority and agent event batching (#500)
- Redundant syntax highlighting and markdown parsing on every TUI frame: per-message render cache with content-hash keying (#501)

## [0.10.0] - 2026-02-18

### Fixed
- TUI status spinner not cleared after model warmup completes (#517)
- Duplicate tool output rendering for shell-streamed tools in TUI (#516)
- `send_tool_output` not forwarded through `AppChannel`/`AnyChannel` enum dispatch (#508)
- Tool output and diff not sent atomically in native tool_use path (#498)
- Parallel tool_use calls: results processed sequentially for correct ordering (#486)
- Native `tool_result` format not recognized by TUI history loader (#484)
- Inline filter stats threshold based on char savings instead of line count (#483)
- Token metrics not propagated in native tool_use path (#482)
- Filter metrics not appearing in TUI Resources panel when using native tool_use providers (#480)
- Output filter matchers not matching compound shell commands like `cd /path && cargo test 2>&1 | tail` (#481)
- Duplicate `ToolEvent::Completed` emission in shell executor before filtering was applied (#480)
- TUI feature gate compilation errors (#435)

### Added
- GitHub CLI skill with token-saving patterns (#507)
- Parallel execution of native tool_use calls with configurable concurrency (#486)
- TUI compact/detailed tool output toggle with 'e' key binding (#479)
- TUI `[tui]` config section with `show_source_labels` option to hide `[user]`/`[zeph]`/`[tool]` prefixes (#505)
- Syntax-highlighted diff view for write/edit tool output in TUI (#455)
  - Diff rendering with green/red backgrounds for added/removed lines
  - Word-level change highlighting within modified lines
  - Syntax highlighting via tree-sitter
  - Compact/expanded toggle with existing 'e' key binding
  - New dependency: `similar` 2.7.0
- Per-tool inline filter stats in CLI chat: `[shell] cargo test (342 lines -> 28 lines, 91.8% filtered)` (#449)
- Filter metrics in TUI Resources panel: confidence distribution, command hit rate, token savings (#448)
- Periodic 250ms tick in TUI event loop for real-time metrics refresh (#447)
- Output filter architecture improvements (M26.1): `CommandMatcher` enum, `FilterConfidence`, `FilterPipeline`, `SecurityPatterns`, per-filter TOML config (#452)
- Token savings tracking and metrics for output filtering (#445)
- Smart tool output filtering: command-aware filters that compress tool output before context insertion
- `OutputFilter` trait and `OutputFilterRegistry` with first-match-wins dispatch
- `sanitize_output()` ANSI escape and progress bar stripping (runs on all tool output)
- Test output filter: cargo test/nextest failures-only mode (94-99% token savings on green suites)
- Git output filter: compact status/diff/log/push compression (80-99% savings)
- Clippy output filter: group warnings by lint rule (70-90% savings)
- Directory listing filter: hide noise directories (target, node_modules, .git)
- Log deduplication filter: normalize timestamps/UUIDs, count repeated patterns (70-85% savings)
- `[tools.filters]` config section with `enabled` toggle
- Skill trust levels: 4-tier model (Trusted, Verified, Quarantined, Blocked) with per-turn enforcement
- `TrustGateExecutor` wrapping tool execution with trust-level permission checks
- `AnomalyDetector` with sliding-window threshold counters for quarantined skill monitoring
- blake3 content hashing for skill integrity verification on load and hot-reload
- Quarantine prompt wrapping for structural isolation of untrusted skill bodies
- Self-learning gate: skills with trust < Verified skip auto-improvement
- `skill_trust` SQLite table with migration 009
- CLI commands: `/skill trust`, `/skill block`, `/skill unblock`
- `[skills.trust]` config section (default_level, local_level, hash_mismatch_level)
- `ProviderKind` enum for type-safe provider selection in config
- `RuntimeConfig` struct grouping agent runtime fields
- `AnyProvider::embed_fn()` shared embedding closure helper
- `Config::validate()` with bounds checking for critical config values
- `sanitize_paths()` for stripping absolute paths from error messages
- 10-second timeout wrapper for embedding API calls
- `full` feature flag enabling all optional features

### Changed
- Remove `P` generic from `Agent`, `SemanticMemory`, `CodeRetriever` — provider resolved at construction (#423)
- Architecture improvements, performance optimizations, security hardening (M24) (#417)
- Extract bootstrap logic from main.rs into `zeph-core::bootstrap::AppBuilder` (#393): main.rs reduced from 2313 to 978 lines
- `SecurityConfig` and `TimeoutConfig` gain `Clone + Copy`
- `AnyChannel` moved from main.rs to zeph-channels crate
- Remove 8 lightweight feature gates, make always-on: openai, compatible, orchestrator, router, self-learning, qdrant, vault-age, mcp (#438)
- Default features reduced to minimal set (empty after M26)
- Skill matcher concurrency reduced from 50 to 20
- `String::with_capacity` in context building loops
- CI updated to use `--features full`

### Breaking
- `LlmConfig.provider` changed from `String` to `ProviderKind` enum
- Default features reduced -- users needing a2a, candle, mcp, openai, orchestrator, router, tui must enable explicitly or use `--features full`
- Telegram channel rejects empty `allowed_users` at startup
- Config with extreme values now rejected by `Config::validate()`

### Deprecated
- `ToolExecutor::execute()` string-based dispatch (use `execute_tool_call()` instead)

### Fixed
- Closed #410 (clap dropped atty), #411 (rmcp updated quinn-udp), #413 (A2A body limit already present)

## [0.9.9] - 2026-02-17

### Added
- `zeph-gateway` crate: axum HTTP gateway with POST /webhook ingestion, bearer auth (blake3 + ct_eq), per-IP rate limiting, GET /health endpoint, feature-gated (`gateway`) (#379)
- `zeph-core::daemon` module: component supervisor with health monitoring, PID file management, graceful shutdown, feature-gated (`daemon`) (#380)
- `zeph-scheduler` crate: cron-based periodic task scheduler with SQLite persistence, built-in tasks (memory_cleanup, skill_refresh, health_check), TaskHandler trait, feature-gated (`scheduler`) (#381)
- New config sections: `[gateway]`, `[daemon]`, `[scheduler]` in config/default.toml (#367)
- New optional feature flags: `gateway`, `daemon`, `scheduler`
- Hybrid memory search: FTS5 keyword search combined with Qdrant vector similarity (#372, #373, #374)
- SQLite FTS5 virtual table with auto-sync triggers for full-text keyword search
- Configurable `vector_weight`/`keyword_weight` in `[memory.semantic]` for hybrid ranking
- FTS5-only fallback when Qdrant is unavailable (replaces empty results)
- `AutonomyLevel` enum (ReadOnly/Supervised/Full) for controlling tool access (#370)
- `autonomy_level` config key in `[security]` section (default: supervised)
- Read-only mode restricts agent to file_read, file_glob, file_grep, web_scrape
- Full mode allows all tools without confirmation prompts
- Documented `[telegram].allowed_users` allowlist in default config (#371)
- OpenTelemetry OTLP trace export with `tracing-opentelemetry` layer, feature-gated behind `otel` (#377)
- `[observability]` config section with exporter selection and OTLP endpoint
- Instrumentation spans for LLM calls (`llm_call`) and tool executions (`tool_exec`)
- `CostTracker` with per-model token pricing and configurable daily budget limits (#378)
- `[cost]` config section with `enabled` and `max_daily_cents` options
- `cost_spent_cents` field in `MetricsSnapshot` for TUI cost display
- Discord channel adapter with Gateway v10 WebSocket, slash commands, edit-in-place streaming (#382)
- Slack channel adapter with Events API webhook, HMAC-SHA256 signature verification, streaming (#383)
- Feature flags: `discord` and `slack` (opt-in) in zeph-channels and root crate
- `DiscordConfig` and `SlackConfig` with token redaction in Debug impls
- Slack timestamp replay protection (reject requests >5min old)
- Configurable Slack webhook bind address (`webhook_host`)

## [0.9.8] - 2026-02-16

### Added
- Graceful shutdown on Ctrl-C with farewell message and MCP server cleanup (#355)
- Cancel-aware LLM streaming via tokio::select on shutdown signal (#358)
- `McpManager::shutdown_all_shared()` with per-client 5s timeout (#356)
- Indexer progress logging with file count and per-file stats
- Skip code index for providers with native tool_use (#357)
- OpenAI prompt caching: parse and report cached token usage (#348)
- Syntax highlighting for TUI code blocks via tree-sitter-highlight (#345, #346, #347)
- Anthropic prompt caching with structured system content blocks (#337)
- Configurable summary provider for tool output summarization via local model (#338)
- Aggressive inline pruning of stale tool outputs in tool loops (#339)
- Cache usage metrics (cache_read_tokens, cache_creation_tokens) in MetricsSnapshot (#340)
- Native tool_use support for Claude provider (Anthropic API format) (#256)
- Native function calling support for OpenAI provider (#257)
- `ToolDefinition`, `ChatResponse`, `ToolUseRequest` types in zeph-llm (#254)
- `ToolUse`/`ToolResult` variants in `MessagePart` for structured tool flow (#255)
- Dual-mode agent loop: native structured path alongside legacy text extraction (#258)
- Dual system prompt: native tool_use instructions for capable providers, fenced-block instructions for legacy providers

### Changed
- Consolidate all SQLite migrations into root `migrations/` directory (#354)

## [0.9.7] - 2026-02-15

### Performance
- Token estimation uses `len() / 3` for improved accuracy (#328)
- Explicit tokio feature selection replacing broad feature gates (#326)
- Concurrent skill embedding for faster startup (#327)
- Pre-allocate strings in hot paths to reduce allocations (#329)
- Parallel context building via `try_join!` (#331)
- Criterion benchmark suite for core operations (#330)

### Security
- Path traversal protection in shell sandbox (#325)
- Canonical path validation in skill loader (#322)
- SSRF protection for MCP server connections (#323)
- Remove MySQL/RSA vulnerable transitive dependencies (#324)
- Secret redaction patterns for Google and GitLab tokens (#320)
- TTL-based eviction for rate limiter entries (#321)

### Changed
- `QdrantOps` shared helper trait for Qdrant collection operations (#304)
- `delegate_provider!` macro replacing boilerplate provider delegation (#303)
- Remove `TuiError` in favor of unified error handling (#302)
- Generic `recv_optional` replacing per-channel optional receive logic (#301)

### Dependencies
- Upgraded rmcp to 0.15, toml to 1.0, uuid to 1.21 (#296)
- Cleaned up deny.toml advisory and license configuration (#312)

## [0.9.6] - 2026-02-15

### Changed
- **BREAKING**: `ToolDef` schema field replaced `Vec<ParamDef>` with `schemars::Schema` auto-derived from Rust structs via `#[derive(JsonSchema)]`
- **BREAKING**: `ParamDef` and `ParamType` removed from `zeph-tools` public API
- **BREAKING**: `ToolRegistry::new()` replaced with `ToolRegistry::from_definitions()`; registry no longer hardcodes built-in tools — each executor owns its definitions via `tool_definitions()`
- **BREAKING**: `Channel` trait now requires `ChannelError` enum with typed error handling replacing `anyhow::Result`
- **BREAKING**: `Agent::new()` signature changed to accept new field grouping; agent struct refactored into 5 inner structs for improved organization
- **BREAKING**: `AgentError` enum introduced with 7 typed variants replacing scattered `anyhow::Error` handling
- `ToolDef` now includes `InvocationHint` (FencedBlock/ToolCall) so LLM prompt shows exact invocation format per tool
- `web_scrape` tool definition includes all parameters (`url`, `select`, `extract`, `limit`) auto-derived from `ScrapeInstruction`
- `ShellExecutor` and `WebScrapeExecutor` now implement `tool_definitions()` for single source of truth
- Replaced `tokio` "full" feature with granular features in zeph-core (async-io, macros, rt, sync, time)
- Removed `anyhow` dependency from zeph-channels
- Message persistence now uses `MessageKind` enum instead of `is_summary` bool for qdrant storage

### Added
- `ChannelError` enum with typed variants for channel operation failures
- `AgentError` enum with 7 typed variants for agent operation failures (streaming, persistence, configuration, etc.)
- Workspace-level `qdrant` feature flag for optional semantic memory support
- Type aliases consolidated into zeph-llm: `EmbedFuture` and `EmbedFn` with typed `LlmError`
- `streaming.rs` and `persistence.rs` modules extracted from agent module for improved code organization
- `MessageKind` enum for distinguishing summary and regular messages in storage

### Removed
- `anyhow::Result` from Channel trait (replaced with `ChannelError`)
- Direct `anyhow::Error` usage in agent module (replaced with `AgentError`)

## [0.9.5] - 2026-02-14

### Added
- Pattern-based permission policy with glob matching per tool (allow/ask/deny), first-match-wins evaluation (#248)
- Legacy blocked_commands and confirm_patterns auto-migrated to permission rules (#249)
- Denied tools excluded from LLM system prompt (#250)
- Tool output overflow: full output saved to file when truncated, path notice appended for LLM access (#251)
- Stale tool output overflow files cleaned up on startup (>24h TTL) (#252)
- `ToolRegistry` with typed `ToolDef` definitions for 7 built-in tools (bash, read, edit, write, glob, grep, web_scrape) (#239)
- `FileExecutor` for sandboxed file operations: read, write, edit, glob, grep (#242)
- `ToolCall` struct and `execute_tool_call()` on `ToolExecutor` trait for structured tool invocation (#241)
- `CompositeExecutor` routes structured tool calls to correct sub-executor by tool_id (#243)
- Tool catalog section in system prompt via `ToolRegistry::format_for_prompt()` (#244)
- Configurable `max_tool_iterations` (default 10, previously hardcoded 3) via TOML and `ZEPH_AGENT_MAX_TOOL_ITERATIONS` env var (#245)
- Doom-loop detection: breaks agent loop on 3 consecutive identical tool outputs
- Context budget check at 80% threshold stops iteration before context overflow
- `IndexWatcher` for incremental code index updates on file changes via `notify` file watcher (#233)
- `watch` config field in `[index]` section (default `true`) to enable/disable file watching
- Repo map cache with configurable TTL (`repo_map_ttl_secs`, default 300s) to avoid per-message filesystem traversal (#231)
- Cross-session memory score threshold (`cross_session_score_threshold`, default 0.35) to filter low-relevance results (#232)
- `embed_missing()` called on startup for embedding backfill when Qdrant available (#261)
- `AgentTaskProcessor` replaces `EchoTaskProcessor` for real A2A inference (#262)

### Changed
- ShellExecutor uses PermissionPolicy for all permission checks instead of legacy find_blocked_command/find_confirm_command
- Replaced unmaintained dirs-next 2.0 with dirs 6.x
- Batch messages retrieval in semantic recall: replaced N+1 query pattern with `messages_by_ids()` for improved performance

### Fixed
- Persist `MessagePart` data to SQLite via `remember_with_parts()` — pruning state now survives session restarts (#229)
- Clear tool output body from memory after Tier 1 pruning to reclaim heap (#230)
- TUI uptime display now updates from agent start time instead of always showing 0s (#259)
- `FileExecutor` `handle_write` now uses canonical path for security (TOCTOU prevention) (#260)
- `resolve_via_ancestors` trailing slash bug on macOS
- `vault.backend` from config now used as default backend; CLI `--vault` flag overrides config (#263)
- A2A error responses sanitized to prevent provider URL leakage

## [0.9.4] - 2026-02-14

### Added
- Bounded FIFO message queue (max 10) in agent loop: users can submit messages during inference, queued messages are delivered sequentially when response cycle completes
- Channel trait extended with `try_recv()` (non-blocking poll) and `send_queue_count()` with default no-op impls
- Consecutive user messages within 500ms merge window joined by newline
- TUI queue badge `[+N queued]` in input area, `Ctrl+K` to clear queue, `/clear-queue` command
- TelegramChannel `try_recv()` implementation via mpsc
- Deferred model warmup in TUI mode: interface renders immediately, Ollama warmup runs in background with status indicator ("warming up model..." → "model ready"), agent loop awaits completion via `watch::channel`
- `context_tokens` metric in TUI Resources panel showing current prompt estimate (vs cumulative session totals)
- `unsummarized_message_count` in `SemanticMemory` for precise summarization trigger
- `count_messages_after` in `SqliteStore` for counting messages beyond a given ID
- TUI status indicators for context compaction ("compacting context...") and summarization ("summarizing...")
- Debug tracing in `should_compact()` for context budget diagnostics (token estimate, threshold, decision)
- Config hot-reload: watch config file for changes via `notify_debouncer_mini` and apply runtime-safe fields (security, timeouts, memory limits, context budget, compaction, max_active_skills) without restart
- `ConfigWatcher` in zeph-core with 500ms debounced filesystem monitoring
- `with_config_reload()` builder method on Agent for wiring config file watcher
- `tool_name` field in `ToolOutput` for identifying tool type (bash, mcp, web-scrape) in persisted messages and TUI display
- Real-time status events for provider retries and orchestrator fallbacks surfaced as `[system]` messages across all channels (CLI stderr, TUI chat panel, Telegram)
- `StatusTx` type alias in `zeph-llm` for emitting status events from providers
- `Status` variant in TUI `AgentEvent` rendered as System-role messages (DarkGray)
- `set_status_tx()` on `AnyProvider`, `SubProvider`, and `ModelOrchestrator` for propagating status sender through the provider hierarchy
- Background forwarding tasks for immediate status delivery (bypasses agent loop for zero-latency display)
- TUI: toggle side panels with `d` key in Normal mode
- TUI: input history navigation (Up/Down in Insert mode)
- TUI: message separators and accent bars for visual structure
- TUI: tool output restored as expandable messages from conversation history
- TUI: collapsed tool output preview (3 lines) when restoring history
- `LlmProvider::context_window()` trait method for model context window size detection
- Ollama context window auto-detection via `/api/show` model info endpoint
- Context window sizes for Claude (200K) and OpenAI (128K/16K/1M) provider models
- `auto_budget` config field with `ZEPH_MEMORY_AUTO_BUDGET` env override for automatic context budget from model metadata
- `inject_summaries()` in Agent: injects SQLite conversation summaries into context (newest-first, budget-aware, with deduplication)
- Wire `zeph-index` Code RAG pipeline into agent loop (feature-gated `index`): `CodeRetriever` integration, `inject_code_rag()` in `prepare_context()`, repo map in system prompt, background project indexing on startup
- `IndexConfig` with `[index]` TOML section and `ZEPH_INDEX_*` env overrides (enabled, max_chunks, score_threshold, budget_ratio, repo_map_tokens)
- Two-tier context pruning strategy for granular token reclamation before full LLM compaction
  - Tier 1: selective `ToolOutput` part pruning with `compacted_at` timestamp on pruned parts
  - Tier 2: LLM-based compaction fallback when tier 1 is insufficient
  - `prune_protect_tokens` config field for token-based protection zone (shields recent context from pruning)
  - `tool_output_prunes` metric tracking tier 1 pruning operations
  - `compacted_at` field on `MessagePart::ToolOutput` for pruning audit trail
- `MessagePart` enum (Text, ToolOutput, Recall, CodeContext, Summary) for typed message content with independent lifecycle
- `Message::from_parts()` constructor with `to_llm_content()` flattening for LLM provider consumption
- `Message::from_legacy()` backward-compatible constructor for simple text messages
- SQLite migration 006: `parts` column for structured message storage (JSON-serialized)
- `save_message_with_parts()` in SqliteStore for persisting typed message parts
- inject_semantic_recall, inject_code_context, inject_summaries now create typed MessagePart variants

### Changed
- `index` feature enabled by default (Code RAG pipeline active out of the box)
- Agent error handler shows specific error context instead of generic message
- TUI inline code rendered as blue with dark background glow instead of bright yellow
- TUI header uses deep blue background (`Rgb(20, 40, 80)`) for improved contrast
- System prompt includes explicit `bash` block example and bans invented formats (`tool_code`, `tool_call`) for small model compatibility
- TUI Resources panel: replaced separate Prompt/Completion/Total with Context (current) and Session (cumulative) metrics
- Summarization trigger uses unsummarized message count instead of total, avoiding repeated no-op checks
- Empty `AgentEvent::Status` clears TUI spinner instead of showing blank throbber
- Status label cleared after summarization and compaction complete
- Default `summarization_threshold`: 100 → 50 messages
- Default `compaction_threshold`: 0.75 → 0.80
- Default `compaction_preserve_tail`: 4 → 6 messages
- Default `semantic.enabled`: false → true
- Default `summarize_output`: false → true
- Default `context_budget_tokens`: 0 (auto-detect from model)

### Fixed
- TUI chat line wrapping no longer eats 2 characters on word wrap (accent prefix width accounted for)
- TUI activity indicator moved to dedicated layout row (no longer overlaps content)
- Memory history loading now retrieves most recent messages instead of oldest
- Persisted tool output format includes tool name (`[tool output: bash]`) for proper display on restore
- `summarize_output` serde deserialization used `#[serde(default)]` yielding `false` instead of config default `true`

## [0.9.3] - 2026-02-12

### Added
- New `zeph-index` crate: AST-based code indexing and semantic retrieval pipeline
  - Language detection and grammar registry with feature-gated tree-sitter grammars (Rust, Python, JavaScript, TypeScript, Go, Bash, TOML, JSON, Markdown)
  - AST-based chunker with cAST-inspired greedy sibling merge and recursive decomposition (target 600 non-ws chars per chunk)
  - Contextualized embedding text generation for improved retrieval quality
  - Dual-write storage layer (Qdrant vector search + SQLite metadata) with INT8 scalar quantization
  - Incremental indexer with .gitignore-aware file walking and content-hash change detection
  - Hybrid retriever with query classification (Semantic/Grep/Hybrid) and budget-aware result packing
  - Lightweight repo map generation (tree-sitter signature extraction, budget-constrained output)
- `code_context` slot in `BudgetAllocation` for code RAG injection into agent context
- `inject_code_context()` method in Agent for transient code chunk injection before semantic recall

## [0.9.2] - 2026-02-12

### Added
- Runtime context compaction for long sessions: automatic LLM-based summarization of middle messages when context usage exceeds configurable threshold (default 75%)
- `with_context_budget()` builder method on Agent for wiring context budget and compaction settings
- Config fields: `compaction_threshold` (f32), `compaction_preserve_tail` (usize) with env var overrides
- `context_compactions` counter in MetricsSnapshot for observability
- Context budget integration: `ContextBudget::allocate()` wired into agent loop via `prepare_context()` orchestrator
- Semantic recall injection: `SemanticMemory::recall()` results injected as transient system messages with token budget control
- Message history trimming: oldest non-system messages evicted when history exceeds budget allocation
- Environment context injection: working directory, OS, git branch, and model name in system prompt via `<environment>` block
- Extended BASE_PROMPT with structured Tool Use, Guidelines, and Security sections
- Tool output truncation: head+tail split at 30K chars with UTF-8 safe boundaries
- Smart tool output summarization: optional LLM-based summarization for outputs exceeding 30K chars, with fallback to truncation on failure (disabled by default via `summarize_output` config)
- Progressive skill loading: matched skills get full body, remaining shown as description-only catalog via `<other_skills>`
- ZEPH.md project config discovery: walk up directory tree, inject into system prompt as `<project_context>`

## [0.9.1] - 2026-02-12

### Added
- Mouse scroll support for TUI chat widget (scroll up/down via mouse wheel)
- Splash screen with colored block-letter "ZEPH" banner on TUI startup
- Conversation history loading into chat on TUI startup
- Model thinking block rendering (`<think>` tags from Ollama DeepSeek/Qwen models) in distinct darker style
- Markdown rendering for all chat messages via `pulldown-cmark`: bold, italic, strikethrough, headings, code blocks, inline code, lists, blockquotes, horizontal rules
- Scrollbar track with proportional thumb indicator in chat widget

### Fixed
- Chat messages no longer overflow below the viewport when lines wrap
- Scroll no longer sticks at top after over-scrolling past content boundary

## [0.9.0] - 2026-02-12

### Added
- ratatui-based TUI dashboard with real-time agent metrics (feature-gated `tui`, opt-in)
- `TuiChannel` as new `Channel` implementation with bottom-up chat feed, input line, and status bar
- `MetricsSnapshot` and `MetricsCollector` in zeph-core via `tokio::sync::watch` for live metrics transport
- `with_metrics()` builder on Agent with instrumentation at 8 collection points: api_calls, latency, prompt/completion tokens, active skills, sqlite message count, qdrant status, summarization count
- Side panel widgets (skills, memory, resources) with live data from agent loop
- Confirmation modal dialog for destructive command approval in TUI (Y/Enter confirms, N/Escape cancels)
- Scroll indicators (▲/▼) in chat widget when content overflows viewport
- Responsive layout: side panels hidden on terminals narrower than 80 columns
- Multiline input via Shift+Enter in TUI insert mode
- Bottom-up chat layout with proper newline handling and per-message visual separation
- Panic hook for terminal state restoration on any panic during TUI execution
- Unicode-safe char-index cursor tracking for multi-byte input in TUI
- `--config <path>` CLI argument and `ZEPH_CONFIG` env var to override default config path
- OpenAI-compatible LLM provider with chat, streaming, and embeddings support
- Feature-gated `openai` feature (enabled by default)
- Support for OpenAI, Together AI, Groq, Fireworks, and any OpenAI-compatible API via configurable `base_url`
- `reasoning_effort` parameter for OpenAI reasoning models (low/medium/high)
- `/mcp add <id> <command> [args...]` for dynamic stdio MCP server connection at runtime
- `/mcp add <id> <url>` for HTTP transport (remote MCP servers in Docker/cloud)
- `/mcp list` command to show connected servers and tool counts
- `/mcp remove <id>` command to disconnect MCP servers
- `McpTransport` enum: `Stdio` (child process) and `Http` (Streamable HTTP) transports
- HTTP MCP server config via `url` field in `[[mcp.servers]]`
- `mcp.allowed_commands` config for command allowlist (security hardening)
- `mcp.max_dynamic_servers` config to limit concurrent dynamic servers (default 10)
- Qdrant registry sync after dynamic MCP add/remove for semantic tool matching

### Changed
- Docker images now include Node.js, npm, and Python 3 for MCP server runtime
- `ServerEntry` uses `McpTransport` enum instead of flat command/args/env fields

### Fixed
- Effective embedding model resolution: Qdrant subsystems now use the correct provider-specific embedding model name when provider is `openai` or orchestrator routes to OpenAI
- Skill watcher no longer loops in Docker containers (overlayfs phantom events)

## [0.8.2] - 2026-02-10

### Changed
- Enable all non-platform features by default: `orchestrator`, `self-learning`, `mcp`, `vault-age`, `candle`
- Features `metal` and `cuda` remain opt-in (platform-specific GPU accelerators)
- CI clippy uses default features instead of explicit feature list
- Docker images now include skill runtime dependencies: `curl`, `wget`, `git`, `jq`, `file`, `findutils`, `procps-ng`

## [0.8.1] - 2026-02-10

### Added
- Shell sandbox: configurable `allowed_paths` directory allowlist and `allow_network` toggle blocking curl/wget/nc in `ShellExecutor` (Issue #91)
- Sandbox validation before every shell command execution with path canonicalization
- `tools.shell.allowed_paths` config (empty = working directory only) with `ZEPH_TOOLS_SHELL_ALLOWED_PATHS` env override
- `tools.shell.allow_network` config (default: true) with `ZEPH_TOOLS_SHELL_ALLOW_NETWORK` env override
- Interactive confirmation for destructive commands (`rm`, `git push -f`, `DROP TABLE`, etc.) with CLI y/N prompt and Telegram inline keyboard (Issue #92)
- `tools.shell.confirm_patterns` config with default destructive command patterns
- `Channel::confirm()` trait method with default auto-confirm for headless/test scenarios
- `ToolError::ConfirmationRequired` and `ToolError::SandboxViolation` variants
- `execute_confirmed()` method on `ToolExecutor` for confirmation bypass after user approval
- A2A TLS enforcement: reject HTTP endpoints when `a2a.require_tls = true` (Issue #92)
- A2A SSRF protection: block private IP ranges (RFC 1918, loopback, link-local) with DNS resolution (Issue #92)
- Configurable A2A server payload size limit via `a2a.max_body_size` (default: 1 MiB)
- Structured JSON audit logging for all tool executions with stdout or file destination (Issue #93)
- `AuditLogger` with `AuditEntry` (timestamp, tool, command, result, duration) and `AuditResult` enum
- `[tools.audit]` config section with `ZEPH_TOOLS_AUDIT_ENABLED` and `ZEPH_TOOLS_AUDIT_DESTINATION` env overrides
- Secret redaction in LLM responses: detect API keys, tokens, passwords, private keys and replace with `[REDACTED]` (Issue #93)
- Whitespace-preserving `redact_secrets()` scanner with zero-allocation fast path via `Cow<str>`
- `[security]` config section with `redact_secrets` toggle (default: true)
- Configurable timeout policies for LLM, embedding, and A2A operations (Issue #93)
- `[timeouts]` config section with `llm_seconds`, `embedding_seconds`, `a2a_seconds`
- LLM calls wrapped with `tokio::time::timeout` in agent loop

## [0.8.0] - 2026-02-10

### Added
- `VaultProvider` trait with pluggable secret backends, `Secret` newtype with redacted debug output, `EnvVaultProvider` for environment variable secrets (Issue #70)
- `AgeVaultProvider`: age-encrypted JSON vault backend with x25519 identity key decryption (Issue #70)
- `Config::resolve_secrets()`: async secret resolution through vault provider for API keys and tokens
- CLI vault args: `--vault <backend>`, `--vault-key <path>`, `--vault-path <path>`
- `vault-age` feature flag on `zeph-core` and root binary
- `[vault]` config section with `backend` field (default: `env`)
- `docker-compose.vault.yml` overlay for containerized age vault deployment
- `CARGO_FEATURES` build arg in `Dockerfile.dev` for optional feature flags
- `CandleProvider`: local GGUF model inference via candle ML framework with chat templates (Llama3, ChatML, Mistral, Phi3, Raw), token generation with top-k/top-p sampling, and repeat penalty (Issue #125)
- `CandleProvider` embeddings: BERT-based embedding model loaded from HuggingFace Hub with mean pooling and L2 normalization (Issue #126)
- `ModelOrchestrator`: task-aware multi-model routing with keyword-based classification (coding, creative, analysis, translation, summarization, general) and provider fallback chains (Issue #127)
- `SubProvider` enum breaking recursive type cycle between `AnyProvider` and `ModelOrchestrator`
- Device auto-detection: Metal on macOS, CUDA on Linux with GPU, CPU fallback (Issue #128)
- Feature flags: `candle`, `metal`, `cuda`, `orchestrator` on workspace and zeph-llm crate
- `CandleConfig`, `GenerationParams`, `OrchestratorConfig` in zeph-core config
- Config examples for candle and orchestrator in `config/default.toml`
- Setup guide sections for candle local inference and model orchestrator
- 15 new unit tests for orchestrator, chat templates, generation config, and loader
- Progressive skill loading: lazy body loading via `OnceLock`, on-demand resource resolution for `scripts/`, `references/`, `assets/` directories, extended frontmatter (`compatibility`, `license`, `metadata`, `allowed-tools`), skill name validation per agentskills.io spec (Issue #115)
- `SkillMeta`/`Skill` composition pattern: metadata loaded at startup, body deferred until skill activation
- `SkillRegistry` replaces `Vec<Skill>` in Agent — lazy body access via `get_skill()`/`get_body()`
- `resource.rs` module: `discover_resources()` + `load_resource()` with path traversal protection via canonicalization
- Self-learning skill evolution system: automatic skill improvement through failure detection, self-reflection retry, and LLM-generated version updates (Issue #107)
- `SkillOutcome` enum and `SkillMetrics` for skill execution outcome tracking (Issue #108)
- Agent self-reflection retry on tool failure with 1-retry-per-message budget (Issue #109)
- Skill version generation and storage in SQLite with auto-activate and manual approval modes (Issue #110)
- Automatic rollback when skill version success rate drops below threshold (Issue #111)
- `/skill stats`, `/skill versions`, `/skill activate`, `/skill approve`, `/skill reset` commands for version management (Issue #111)
- `/feedback` command for explicit user feedback on skill quality (Issue #112)
- `LearningConfig` with TOML config section `[skills.learning]` and env var overrides
- `self-learning` feature flag on `zeph-skills`, `zeph-core`, and root binary
- SQLite migration 005: `skill_versions` and `skill_outcomes` tables
- Bundled `setup-guide` skill with configuration reference for all env vars, TOML keys, and operating modes
- Bundled `skill-audit` skill for spec compliance and security review of installed skills
- `allowed_commands` shell config to override default blocklist entries via `ZEPH_TOOLS_SHELL_ALLOWED_COMMANDS`
- `QdrantSkillMatcher`: persistent skill embeddings in Qdrant with BLAKE3 content-hash delta sync (Issue #104)
- `SkillMatcherBackend` enum dispatching between `InMemory` and `Qdrant` skill matching (Issue #105)
- `qdrant` feature flag on `zeph-skills` crate gating all Qdrant dependencies
- Graceful fallback to in-memory matcher when Qdrant is unavailable
- Skill matching tracing via `tracing::debug!` for diagnostics
- New `zeph-mcp` crate: MCP client via rmcp 0.14 with stdio transport (Issue #117)
- `McpClient` and `McpManager` for multi-server lifecycle management with concurrent connections
- `McpToolExecutor` implementing `ToolExecutor` for `` ```mcp `` block execution (Issue #120)
- `McpToolRegistry`: MCP tool embeddings in Qdrant `zeph_mcp_tools` collection with BLAKE3 delta sync (Issue #118)
- Unified matching: skills + MCP tools injected into system prompt by relevance (Issue #119)
- `mcp` feature flag on root binary and zeph-core gating all MCP functionality
- Bundled `mcp-generate` skill with instructions for MCP-to-skill generation via mcp-execution (Issue #121)
- `[[mcp.servers]]` TOML config section for MCP server connections

### Changed
- `Skill` struct refactored: split into `SkillMeta` (lightweight metadata) + `Skill` (meta + body), composition pattern
- `SkillRegistry` now uses `OnceLock<String>` for lazy body caching instead of eager loading
- Matcher APIs accept `&[&SkillMeta]` instead of `&[Skill]` — embeddings use description only
- `Agent` stores `SkillRegistry` directly instead of `Vec<Skill>`
- `Agent` field `matcher` type changed from `Option<SkillMatcher>` to `Option<SkillMatcherBackend>`
- Skill matcher creation extracted to `create_skill_matcher()` in `main.rs`

### Dependencies
- Added `age` 0.11.2 to workspace (optional, behind `vault-age` feature, `default-features = false`)
- Added `candle-core` 0.9, `candle-nn` 0.9, `candle-transformers` 0.9 to workspace (optional, behind `candle` feature)
- Added `hf-hub` 0.4 to workspace (HuggingFace model downloads with rustls-tls)
- Added `tokenizers` 0.22 to workspace (BPE tokenization with fancy-regex)
- Added `blake3` 1.8 to workspace
- Added `rmcp` 0.14 to workspace (MCP protocol SDK)

## [0.7.1] - 2026-02-09

### Added
- `WebScrapeExecutor`: safe HTML scraping via scrape-core with CSS selectors, SSRF protection, and HTTPS-only enforcement (Issue #57)
- `CompositeExecutor<A, B>`: generic executor chaining with first-match-wins dispatch
- Bundled `web-scrape` skill with CSS selector examples for structured data extraction
- `extract_fenced_blocks()` shared utility for fenced code block parsing (DRY refactor)
- `[tools.scrape]` config section with timeout and max body size settings

### Changed
- Agent tool output label from `[shell output]` to `[tool output]`
- `ShellExecutor` block extraction now uses shared `extract_fenced_blocks()`

## [0.7.0] - 2026-02-08

### Added
- A2A Server: axum-based HTTP server with JSON-RPC 2.0 routing for `message/send`, `tasks/get`, `tasks/cancel` (Issue #83)
- In-memory `TaskManager` with full task lifecycle: create, get, update status, add artifacts, append history, cancel (Issue #83)
- SSE streaming endpoint (`/a2a/stream`) with JSON-RPC response envelope wrapping per A2A spec (Issue #84)
- Bearer token authentication middleware with constant-time comparison via `subtle::ConstantTimeEq` (Issue #85)
- Per-IP rate limiting middleware with configurable 60-second sliding window (Issue #85)
- Request body size limit (1 MiB) via `tower-http::limit::RequestBodyLimitLayer` (Issue #85)
- `A2aServerConfig` with env var overrides: `ZEPH_A2A_ENABLED`, `ZEPH_A2A_HOST`, `ZEPH_A2A_PORT`, `ZEPH_A2A_PUBLIC_URL`, `ZEPH_A2A_AUTH_TOKEN`, `ZEPH_A2A_RATE_LIMIT`
- Agent card served at `/.well-known/agent.json` (public, no auth required)
- Graceful shutdown integration via tokio watch channel
- Server module gated behind `server` feature flag on `zeph-a2a` crate

### Changed
- `Part` type refactored from flat struct to tagged enum with `kind` discriminator (`text`, `file`, `data`) per A2A spec
- `TaskState::Pending` renamed to `TaskState::Submitted` with explicit per-variant `#[serde(rename)]` for kebab-case wire format
- Added `AuthRequired` and `Unknown` variants to `TaskState`
- `TaskStatusUpdateEvent` and `TaskArtifactUpdateEvent` gained `kind` field (`status-update`, `artifact-update`)

## [0.6.0] - 2026-02-08

### Added
- New `zeph-a2a` crate: A2A protocol implementation for agent-to-agent communication (Issue #78)
- A2A protocol types: `Task`, `TaskState`, `TaskStatus`, `Message`, `Part`, `Artifact`, `AgentCard`, `AgentSkill`, `AgentCapabilities` with full serde camelCase serialization (Issue #79)
- JSON-RPC 2.0 envelope types (`JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcError`) with method constants for A2A operations (Issue #79)
- `AgentCardBuilder` for constructing A2A agent cards from runtime config and skills (Issue #79)
- `AgentRegistry` with well-known URI discovery (`/.well-known/agent.json`), TTL-based caching, and manual registration (Issue #80)
- `A2aClient` with `send_message`, `stream_message` (SSE), `get_task`, `cancel_task` via JSON-RPC 2.0 (Issue #81)
- Bearer token authentication support for all A2A client operations (Issue #81)
- SSE streaming via `eventsource-stream` with `TaskEvent` enum (`StatusUpdate`, `ArtifactUpdate`) (Issue #81)
- `A2aError` enum with variants for HTTP, JSON, JSON-RPC, discovery, and stream errors (Issue #79)
- Optional `a2a` feature flag (enabled by default) to gate A2A functionality
- 42 new unit tests for protocol types, JSON-RPC envelopes, agent card builder, discovery registry, and client operations

## [0.5.0] - 2026-02-08

### Added
- Embedding-based skill matcher: `SkillMatcher` with cosine similarity selects top-K relevant skills per query instead of injecting all skills into the system prompt (Issue #75)
- `max_active_skills` config field (default: 5) with `ZEPH_SKILLS_MAX_ACTIVE` env var override
- Skill hot-reload: filesystem watcher via `notify-debouncer-mini` detects SKILL.md changes and re-embeds without restart (Issue #76)
- Skill priority: earlier paths in `skills.paths` take precedence when skills share the same name (Issue #76)
- `SkillRegistry::reload()` and `SkillRegistry::into_skills()` methods
- SQLite `skill_usage` table tracking per-skill invocation counts and last-used timestamps (Issue #77)
- `/skills` command displaying available skills with usage statistics
- Three new bundled skills: `git`, `docker`, `api-request` (Issue #77)
- 17 new unit tests for matcher, registry priority, reload, and usage tracking

### Changed
- `Agent::new()` signature: accepts `Vec<Skill>`, `Option<SkillMatcher>`, `max_active_skills` instead of pre-formatted skills prompt string
- `format_skills_prompt` now generic over `Borrow<Skill>` to accept both `&[Skill]` and `&[&Skill]`
- `Skill` struct derives `Clone`
- `Agent` generic constraint: `P: LlmProvider + Clone + 'static` (required for embed_fn closures)
- System prompt rebuilt dynamically per user query with only matched skills

### Dependencies
- Added `notify` 8.0, `notify-debouncer-mini` 0.6
- `zeph-core` now depends on `zeph-skills`
- `zeph-skills` now depends on `tokio` (sync, rt) and `notify`

## [0.4.3] - 2026-02-08

### Fixed
- Telegram "Bad Request: text must be non-empty" error when LLM returns whitespace-only content. Added `is_empty()` guard after `markdown_to_telegram` conversion in both `send()` and `send_or_edit()` (Issue #73)

### Added
- `Dockerfile.dev`: multi-stage build from source with cargo registry/build cache layers for fast rebuilds
- `docker-compose.dev.yml`: full dev stack (Qdrant + Zeph) with debug tracing (`RUST_LOG`, `RUST_BACKTRACE=1`), uses host Ollama via `host.docker.internal`
- `docker-compose.deps.yml`: Qdrant-only compose for native zeph execution on macOS

## [0.4.2] - 2026-02-08

### Fixed
- Telegram MarkdownV2 parsing errors (Issue #69). Replaced manual character-by-character escaping with AST-based event-driven rendering using pulldown-cmark 0.13.0
- UTF-8 safe text chunking for messages exceeding Telegram's 4096-byte limit. Uses `str::is_char_boundary()` with newline preference to prevent splitting multi-byte characters (emoji, CJK)
- Link URL over-escaping. Dedicated `escape_url()` method only escapes `)` and `\` per Telegram MarkdownV2 spec, fixing broken URLs like `https://example\.com`

### Added
- `TelegramRenderer` state machine for context-aware escaping: 19 special characters in text, only `\` and `` ` `` in code blocks
- Markdown formatting support: bold, italic, strikethrough, headers, code blocks, links, lists, blockquotes
- Comprehensive benchmark suite with criterion: 7 scenario groups measuring latency (2.83µs for 500 chars) and throughput (121-970 MiB/s)
- Memory profiling test to measure escaping overhead (3-20% depending on content)
- 30 markdown unit tests covering formatting, escaping, edge cases, and UTF-8 chunking (99.32% line coverage)

### Changed
- `crates/zeph-channels/src/markdown.rs`: Complete rewrite with pulldown-cmark event-driven parser (449 lines)
- `crates/zeph-channels/src/telegram.rs`: Removed `has_unclosed_code_block()` pre-flight check (no longer needed with AST parsing), integrated UTF-8 safe chunking
- Dependencies: Added pulldown-cmark 0.13.0 (MIT) and criterion 0.8.0 (Apache-2.0/MIT) for benchmarking

## [0.4.1] - 2026-02-08

### Fixed
- Auto-create Qdrant collection on first use. Previously, the `zeph_conversations` collection had to be manually created using curl commands. Now, `ensure_collection()` is called automatically before all Qdrant operations (remember, recall, summarize) to initialize the collection with correct vector dimensions (896 for qwen3-embedding) and Cosine distance metric on first access, similar to SQL migrations.

### Changed
- Docker Compose: Added environment variables for semantic memory configuration (`ZEPH_MEMORY_SEMANTIC_ENABLED`, `ZEPH_MEMORY_SEMANTIC_RECALL_LIMIT`) and Qdrant URL override (`ZEPH_QDRANT_URL`) to enable full semantic memory stack via `.env` file

## [0.4.0] - 2026-02-08

### Added

#### M9 Phase 3: Conversation Summarization and Context Budget (Issue #62)
- New `SemanticMemory::summarize()` method for LLM-based conversation compression
- Automatic summarization triggered when message count exceeds threshold
- SQLite migration `003_summaries.sql` creates dedicated summaries table with CASCADE constraints
- `SqliteStore::save_summary()` stores summary with metadata (first/last message IDs, token estimate)
- `SqliteStore::load_summaries()` retrieves all summaries for a conversation ordered by ID
- `SqliteStore::load_messages_range()` fetches messages after specific ID with limit for batch processing
- `SqliteStore::count_messages()` counts total messages in conversation
- `SqliteStore::latest_summary_last_message_id()` gets last summarized message ID for resumption
- `ContextBudget` struct for proportional token allocation (15% summaries, 25% semantic recall, 60% recent history)
- `estimate_tokens()` helper using chars/4 heuristic (100x faster than tiktoken, ±25% accuracy)
- `Agent::check_summarization()` lazy trigger after persist_message() when threshold exceeded
- Batch size = threshold/2 to balance summary quality with LLM call frequency
- Configuration: `memory.summarization_threshold` (default: 100), `memory.context_budget_tokens` (default: 0 = unlimited)
- Environment overrides: `ZEPH_MEMORY_SUMMARIZATION_THRESHOLD`, `ZEPH_MEMORY_CONTEXT_BUDGET_TOKENS`
- Inline comments in `config/default.toml` documenting all configuration parameters
- 26 new unit tests for summarization and context budget (196 total tests, 75.31% coverage)
- Architecture Decision Records ADR-016 through ADR-019 for summarization design
- Foreign key constraint added to `messages.conversation_id` with ON DELETE CASCADE

#### M9 Phase 2: Semantic Memory Integration (Issue #61)
- `SemanticMemory<P: LlmProvider>` orchestrator coordinating SQLite, Qdrant, and LlmProvider
- `SemanticMemory::remember()` saves message to SQLite, generates embedding, stores in Qdrant
- `SemanticMemory::recall()` performs semantic search with query embedding and fetches messages from SQLite
- `SemanticMemory::has_embedding()` checks if message already embedded to prevent duplicates
- `SemanticMemory::embed_missing()` background task to embed old messages (with LIMIT parameter)
- `Agent<P, C, T>` now generic over LlmProvider to support SemanticMemory
- `Agent::with_memory()` replaces SqliteStore with SemanticMemory
- Graceful degradation: embedding failures logged but don't block message save
- Qdrant connection failures silently downgrade to SQLite-only mode (no semantic recall)
- Generic provider pattern: `SemanticMemory<P: LlmProvider>` instead of `Arc<dyn LlmProvider>` for Edition 2024 async trait compatibility
- `AnyProvider`, `OllamaProvider`, `ClaudeProvider` now derive/implement `Clone` for semantic memory integration
- Integration test updated for SemanticMemory API (with_memory now takes 5 parameters including recall_limit)
- Semantic memory config: `memory.semantic.enabled`, `memory.semantic.recall_limit` (default: 5)
- 18 new tests for semantic memory orchestration (recall, remember, embed_missing, graceful degradation)

#### M9 Phase 1: Qdrant Integration (Issue #60)
- New `QdrantStore` module in zeph-memory for vector storage and similarity search
- `QdrantStore::store()` persists embeddings to Qdrant and tracks metadata in SQLite
- `QdrantStore::search()` performs cosine similarity search with filtering by conversation_id and role
- `QdrantStore::has_embedding()` checks if message has associated embedding
- `QdrantStore::ensure_collection()` idempotently creates Qdrant collection with 768-dimensional vectors
- SQLite migration `002_embeddings_metadata.sql` for embedding metadata tracking
- `embeddings_metadata` table with foreign key constraint to messages (ON DELETE CASCADE)
- PRAGMA foreign_keys enabled in SqliteStore via SqliteConnectOptions
- `SearchFilter` and `SearchResult` types for flexible query construction
- `MemoryConfig.qdrant_url` field with `ZEPH_QDRANT_URL` environment variable override (default: http://localhost:6334)
- Docker Compose Qdrant service (qdrant/qdrant:v1.13.6) on ports 6333/6334 with persistent storage
- Integration tests for Qdrant operations (ignored by default, require running Qdrant instance)
- Unit tests for SQLite metadata operations with 98% coverage
- 12 new tests total (3 unit + 2 integration for QdrantStore, 1 CASCADE DELETE test for SqliteStore, 3 config tests)

#### M8: Embeddings support (Issue #54)
- `LlmProvider` trait extended with `embed(&str) -> Result<Vec<f32>>` for generating text embeddings
- `LlmProvider` trait extended with `supports_embeddings() -> bool` for capability detection
- `OllamaProvider` implements embeddings via ollama-rs `generate_embeddings()` API
- Default embedding model: `qwen3-embedding` (configurable via `llm.embedding_model`)
- `ZEPH_LLM_EMBEDDING_MODEL` environment variable for runtime override
- `ClaudeProvider::embed()` returns descriptive error (Claude API does not support embeddings)
- `AnyProvider` delegates embedding methods to active provider
- 10 new tests: unit tests for all providers, config tests for defaults/parsing/env override
- Integration test for real Ollama embedding generation (ignored by default)
- README documentation: model compatibility notes and `ollama pull` instructions for both LLM and embedding models
- Docker Compose configuration: added `ZEPH_LLM_EMBEDDING_MODEL` environment variable

### Changed

**BREAKING CHANGES** (pre-1.0.0):
- `SqliteStore::save_message()` now returns `Result<i64>` instead of `Result<()>` to enable embedding workflow
- `SqliteStore::new()` uses `sqlx::migrate!()` macro instead of INIT_SQL constant for proper migration management
- `QdrantStore::store()` requires `model: &str` parameter for multi-model support
- Config constant `LLM_ENV_KEYS` renamed to `ENV_KEYS` to reflect inclusion of non-LLM variables

**Migration:**
```rust
// Before:
let _ = store.save_message(conv_id, "user", "hello").await?;

// After:
let message_id = store.save_message(conv_id, "user", "hello").await?;
```

- `OllamaProvider::new()` now accepts `embedding_model` parameter (breaking change, pre-v1.0)
- Config schema: added `llm.embedding_model` field with serde default for backward compatibility

## [0.3.0] - 2026-02-07

### Added

#### M7 Phase 1: Tool Execution Framework - zeph-tools crate (Issue #39)
- New `zeph-tools` leaf crate for tool execution abstraction following ADR-014
- `ToolExecutor` trait with native async (Edition 2024 RPITIT): accepts full LLM response, returns `Option<ToolOutput>`
- `ShellExecutor` implementation with bash block parser and execution (30s timeout via `tokio::time::timeout`)
- `ToolOutput` struct with summary string and blocks_executed count
- `ToolError` enum with Blocked/Timeout/Execution variants (thiserror)
- `ToolsConfig` and `ShellConfig` configuration types with serde Deserialize and sensible defaults
- Workspace version consolidation: `version.workspace = true` across all crates
- Workspace inter-crate dependency references: `zeph-llm.workspace = true` pattern for all internal dependencies
- 22 unit tests with 99.25% line coverage, zero clippy warnings
- ADR-014: zeph-tools crate design rationale and architecture decisions

#### M7 Phase 2: Command safety (Issue #40)
- DEFAULT_BLOCKED patterns: 12 dangerous commands (rm -rf /, sudo, mkfs, dd if=, curl, wget, nc, ncat, netcat, shutdown, reboot, halt)
- Case-insensitive command filtering via to_lowercase() normalization
- Configurable timeout and blocked_commands in TOML via `[tools.shell]` section
- Custom blocked commands additive to defaults (cannot weaken security)
- 35+ comprehensive unit tests covering exact match, prefix match, multiline, case variations
- ToolsConfig integration with core Config struct

#### M7 Phase 3: Agent integration (Issue #41)
- Agent now uses `ShellExecutor` for all bash command execution with safety checks
- SEC-001 CRITICAL vulnerability fixed: unfiltered bash execution removed from agent.rs
- Removed 66 lines of duplicate code (extract_bash_blocks, execute_bash, extract_and_execute_bash)
- ToolError::Blocked properly handled with user-facing error message
- Four integration tests for blocked command behavior and error handling
- Performance validation: < 1% overhead for tool executor abstraction
- Security audit: all acceptance criteria met, zero vulnerabilities

### Security

- **CRITICAL fix for SEC-001**: Shell commands now filtered through ShellExecutor with DEFAULT_BLOCKED patterns (rm -rf /, sudo, mkfs, dd if=, curl, wget, nc, shutdown, reboot, halt). Resolves command injection vulnerability where agent.rs bypassed all security checks via inline bash execution.

### Fixed

- Shell command timeout now respects `config.tools.shell.timeout` (was hardcoded 30s in agent.rs)
- Removed duplicate bash parsing logic from agent.rs (now centralized in zeph-tools)
- Error message pattern leakage: blocked commands now show generic security policy message instead of leaking exact blocked pattern

### Changed

**BREAKING CHANGES** (pre-1.0.0):
- `Agent::new()` signature changed: now requires `tool_executor: T` as 4th parameter where `T: ToolExecutor`
- `Agent` struct now generic over three types: `Agent<P, C, T>` (provider, channel, tool_executor)
- Workspace `Cargo.toml` now defines `version = "0.3.0"` in `[workspace.package]` section
- All crate manifests use `version.workspace = true` instead of explicit versions
- Inter-crate dependencies now reference workspace definitions (e.g., `zeph-llm.workspace = true`)

**Migration:**
```rust
// Before:
let agent = Agent::new(provider, channel, &skills_prompt);

// After:
use zeph_tools::shell::ShellExecutor;
let executor = ShellExecutor::new(&config.tools.shell);
let agent = Agent::new(provider, channel, &skills_prompt, executor);
```

## [0.2.0] - 2026-02-06

### Added

#### M6 Phase 1: Streaming trait extension (Issue #35)
- `LlmProvider::chat_stream()` method returning `Pin<Box<dyn Stream<Item = Result<String>> + Send>>`
- `LlmProvider::supports_streaming()` capability query method
- `Channel::send_chunk()` method for incremental response delivery
- `Channel::flush_chunks()` method for buffered chunk flushing
- `ChatStream` type alias for `Pin<Box<dyn Stream<Item = anyhow::Result<String>> + Send>>`
- Streaming infrastructure in zeph-llm and zeph-core (dependencies: futures-core 0.3, tokio-stream 0.1)

#### M6 Phase 2: Ollama streaming backend (Issue #36)
- Native token-by-token streaming for `OllamaProvider` using `ollama-rs` streaming API
- `OllamaProvider::chat_stream()` implementation via `send_chat_messages_stream()`
- `OllamaProvider::supports_streaming()` now returns `true`
- Stream mapping from `Result<ChatMessageResponse, ()>` to `Result<String, anyhow::Error>`
- Integration tests for streaming happy path and equivalence with non-streaming `chat()` (ignored by default)
- ollama-rs `"stream"` feature enabled in workspace dependencies

#### M6 Phase 3: Claude SSE streaming backend (Issue #37)
- Native token-by-token streaming for `ClaudeProvider` using Anthropic Messages API with Server-Sent Events
- `ClaudeProvider::chat_stream()` implementation via SSE event parsing
- `ClaudeProvider::supports_streaming()` now returns `true`
- SSE event parsing via `eventsource-stream` 0.2.3 library
- Stream pipeline: `bytes_stream() -> eventsource() -> filter_map(parse_sse_event) -> Box::pin()`
- Handles SSE events: `content_block_delta` (text extraction), `error` (mid-stream errors), metadata events (skipped)
- Integration tests for streaming happy path and equivalence with non-streaming `chat()` (ignored by default)
- eventsource-stream dependency added to workspace dependencies
- reqwest `"stream"` feature enabled for `bytes_stream()` support

#### M6 Phase 4: Agent streaming integration (Issue #38)
- Agent automatically uses streaming when `provider.supports_streaming()` returns true (ADR-014)
- `Agent::process_response_streaming()` method for stream consumption and chunk accumulation
- CliChannel immediate streaming: `send_chunk()` prints each chunk instantly via `print!()` + `flush()`
- TelegramChannel batched streaming: debounce at 1 second OR 512 bytes, edit-in-place for progressive updates
- Response buffer pre-allocation with `String::with_capacity(2048)` for performance
- Error message sanitization: full errors logged via `tracing::error!()`, generic messages shown to users
- Telegram edit retry logic: recovers from stale message_id (message deleted, permissions lost)
- tokio-stream dependency added for `StreamExt` trait
- 6 new unit tests for channel streaming behavior

### Fixed

#### M6 Phase 3: Security improvements
- Manual `Debug` implementation for `ClaudeProvider` to prevent API key leakage in debug output
- Error message sanitization: full Claude API errors logged via `tracing::error!()`, generic messages returned to users

### Changed

**BREAKING CHANGES** (pre-1.0.0):
- `LlmProvider` trait now requires `chat_stream()` and `supports_streaming()` implementations (no default implementations per project policy)
- `Channel` trait now requires `send_chunk()` and `flush_chunks()` implementations (no default implementations per project policy)
- All existing providers (`OllamaProvider`, `ClaudeProvider`) updated with fallback implementations (Phase 1 non-streaming: calls `chat()` and wraps in single-item stream)
- All existing channels (`CliChannel`, `TelegramChannel`) updated with no-op implementations (Phase 1: streaming not yet wired into agent loop)

## [0.1.0] - 2026-02-05

### Added

#### M0: Workspace bootstrap
- Cargo workspace with 5 crates: zeph-core, zeph-llm, zeph-skills, zeph-memory, zeph-channels
- Binary entry point with version display
- Default configuration file
- Workspace-level dependency management and lints

#### M1: LLM + CLI agent loop
- LlmProvider trait with Message/Role types
- Ollama backend using ollama-rs
- Config loading from TOML with env var overrides
- Interactive CLI agent loop with multi-turn conversation

#### M2: Skills system
- SKILL.md parser with YAML frontmatter and markdown body (zeph-skills)
- Skill registry that scans directories for `*/SKILL.md` files
- Prompt formatter with XML-like skill injection into system prompt
- Bundled skills: web-search, file-ops, system-info
- Shell execution: agent extracts ```bash``` blocks from LLM responses and runs them
- Multi-step execution loop with 3-iteration limit
- 30-second timeout on shell commands
- Context builder that combines base system prompt with skill instructions

#### M3: Memory + Claude
- SQLite conversation persistence with sqlx (zeph-memory)
- Conversation history loading and message saving per session
- Claude backend via Anthropic Messages API with 429 retry (zeph-llm)
- AnyProvider enum dispatch for runtime provider selection
- CloudLlmConfig for Claude-specific settings (model, max_tokens)
- ZEPH_CLAUDE_API_KEY env var for API authentication
- ZEPH_SQLITE_PATH env var override for database location
- Provider factory in main.rs selecting Ollama or Claude from config
- Memory integration into Agent with optional SqliteStore

#### M4: Telegram channel
- Channel trait abstraction for agent I/O (recv, send, send_typing)
- CliChannel implementation reading stdin/stdout via tokio::task::spawn_blocking
- TelegramChannel adapter using teloxide with mpsc-based message routing
- Telegram user whitelist via `telegram.allowed_users` config
- ZEPH_TELEGRAM_TOKEN env var for Telegram bot activation
- Bot commands: /start (welcome), /reset, /skills forwarded as ChannelMessage
- AnyChannel enum dispatch for runtime channel selection
- zeph-channels crate with teloxide 0.17 dependency
- TelegramConfig in config.rs with TOML and env var support

#### M5: Integration tests + release
- Integration test suite: config, skills, memory, and agent end-to-end
- MockProvider and MockChannel for agent testing without external dependencies
- Graceful shutdown via tokio::sync::watch + tokio::signal (SIGINT/SIGTERM)
- Ollama startup health check (warn-only, non-blocking)
- README with installation, configuration, usage, and skills documentation
- GitHub Actions CI/CD: lint, clippy, test (ubuntu + macos), coverage, security, release
- Dependabot for Cargo and GitHub Actions with auto-merge for patch/minor updates
- Auto-labeler workflow for PRs by path, title prefix, and size
- Release workflow with cross-platform binary builds and checksums
- Issue templates (bug report, feature request)
- PR template with review checklist
- LICENSE (MIT), CONTRIBUTING.md, SECURITY.md

### Fixed
- Replace vulnerable `serde_yml`/`libyml` with manual frontmatter parser (GHSA high + medium)

### Changed
- Move dependency features from workspace root to individual crate manifests
- Update README with badges, architecture overview, and pre-built binaries section

- Agent is now generic over both LlmProvider and Channel (`Agent<P, C>`)
- Agent::new() accepts a Channel parameter instead of reading stdin directly
- Agent::run() uses channel.recv()/send() instead of direct I/O
- Agent calls channel.send_typing() before each LLM request
- Agent::run() uses tokio::select! to race channel messages against shutdown signal

[0.16.0]: https://github.com/bug-ops/zeph/compare/v0.15.3...v0.16.0
[Unreleased]: https://github.com/bug-ops/zeph/compare/v0.16.0...HEAD
[0.15.3]: https://github.com/bug-ops/zeph/compare/v0.15.2...v0.15.3
[0.15.2]: https://github.com/bug-ops/zeph/compare/v0.15.1...v0.15.2
[0.15.1]: https://github.com/bug-ops/zeph/compare/v0.15.0...v0.15.1
[0.15.0]: https://github.com/bug-ops/zeph/compare/v0.14.3...v0.15.0
[0.14.3]: https://github.com/bug-ops/zeph/compare/v0.14.2...v0.14.3
[0.14.2]: https://github.com/bug-ops/zeph/compare/v0.14.1...v0.14.2
[0.14.1]: https://github.com/bug-ops/zeph/compare/v0.14.0...v0.14.1
[0.14.0]: https://github.com/bug-ops/zeph/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/bug-ops/zeph/compare/v0.12.6...v0.13.0
[0.12.6]: https://github.com/bug-ops/zeph/compare/v0.12.5...v0.12.6
[0.12.5]: https://github.com/bug-ops/zeph/compare/v0.12.4...v0.12.5
[0.12.4]: https://github.com/bug-ops/zeph/compare/v0.12.3...v0.12.4
[0.12.3]: https://github.com/bug-ops/zeph/compare/v0.12.2...v0.12.3
[0.12.2]: https://github.com/bug-ops/zeph/compare/v0.12.1...v0.12.2
[0.12.1]: https://github.com/bug-ops/zeph/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/bug-ops/zeph/compare/v0.11.6...v0.12.0
[0.11.6]: https://github.com/bug-ops/zeph/compare/v0.11.5...v0.11.6
[0.11.5]: https://github.com/bug-ops/zeph/compare/v0.11.4...v0.11.5
[0.11.4]: https://github.com/bug-ops/zeph/compare/v0.11.3...v0.11.4
[0.11.3]: https://github.com/bug-ops/zeph/compare/v0.11.2...v0.11.3
[0.11.2]: https://github.com/bug-ops/zeph/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/bug-ops/zeph/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/bug-ops/zeph/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/bug-ops/zeph/compare/v0.9.9...v0.10.0
[0.9.9]: https://github.com/bug-ops/zeph/compare/v0.9.8...v0.9.9
[0.9.8]: https://github.com/bug-ops/zeph/compare/v0.9.7...v0.9.8
[0.9.7]: https://github.com/bug-ops/zeph/compare/v0.9.6...v0.9.7
[0.9.6]: https://github.com/bug-ops/zeph/compare/v0.9.5...v0.9.6
[0.9.5]: https://github.com/bug-ops/zeph/compare/v0.9.4...v0.9.5
[0.9.4]: https://github.com/bug-ops/zeph/compare/v0.9.3...v0.9.4
[0.9.3]: https://github.com/bug-ops/zeph/compare/v0.9.2...v0.9.3
[0.9.2]: https://github.com/bug-ops/zeph/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/bug-ops/zeph/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/bug-ops/zeph/compare/v0.8.2...v0.9.0
[0.8.2]: https://github.com/bug-ops/zeph/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/bug-ops/zeph/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/bug-ops/zeph/compare/v0.7.1...v0.8.0
[0.7.1]: https://github.com/bug-ops/zeph/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/bug-ops/zeph/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/bug-ops/zeph/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/bug-ops/zeph/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/bug-ops/zeph/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/bug-ops/zeph/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/bug-ops/zeph/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/bug-ops/zeph/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/bug-ops/zeph/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bug-ops/zeph/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/bug-ops/zeph/releases/tag/v0.1.0
