---
aliases:
  - System Invariants
  - Zeph Invariants
  - System Contracts
tags:
  - sdd
  - spec
  - architecture
  - invariants
  - system-wide
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[002-agent-loop/spec]]"
  - "[[003-llm-providers/spec]]"
  - "[[006-tools/spec]]"
  - "[[020-config-loading/spec]]"
---

# Spec: Zeph System Invariants

> [!abstract]
> Non-negotiable architectural constraints extracted from the codebase.
> Any change that violates these invariants requires explicit architectural decision,
> not just a code review. This document is the authoritative reference for coding agents.

## Sources

### Internal
| Area | File |
|---|---|
| Channel trait | `crates/zeph-core/src/channel.rs` |
| AnyChannel dispatch | `crates/zeph-channels/src/any.rs` |
| Agent struct | `crates/zeph-core/src/agent/mod.rs` |
| LlmProvider trait | `crates/zeph-llm/src/provider.rs` |
| AnyProvider enum | `crates/zeph-llm/src/any.rs` |
| Message / MessagePart | `crates/zeph-llm/src/provider.rs` |
| ToolExecutor trait | `crates/zeph-tools/src/executor.rs` |
| Config struct | `crates/zeph-core/src/config/types/mod.rs` |
| Feature flags | `Cargo.toml` |

---

> Non-negotiable architectural constraints extracted from the codebase.
> Any change that violates these invariants requires explicit architectural decision,
> not just a code review. This document is the authoritative reference for coding agents.

## 1. Channel Contract

The `Channel` trait (`crates/zeph-core/src/channel.rs`) is the only I/O boundary:

- All methods are `&mut self` — channel is **stateful and owned per session**
- All methods are native async (Edition 2024) — **no `async-trait` macro**
- Return type: `impl Future<Output = Result<T, ChannelError>> + Send`
- `AnyChannel` enum (`crates/zeph-channels/src/any.rs`) is the **only multi-channel dispatch point**
- New channels: enum variant with `#[cfg(feature = "...")]` + macro dispatch — no runtime dyn dispatch
- `Cli` and `Telegram` are always present; `Discord`/`Slack` are feature-gated

**NEVER**: use `Box<dyn Channel>` or `Arc<dyn Channel>` — generics only.

## 2. Agent Loop Contract

`Agent<C: Channel>` (`crates/zeph-core/src/agent/mod.rs`):

- **Generic over `C: Channel`** — instantiated once per session, not cloned or shared
- Main loop: `tokio::select!` multiplexes channel recv + skill/config/instruction reload events + message queue drain
- `VecDeque<QueuedMessage>` is drained before each `channel.recv()` — queue is processed first
- Provider can be swapped at runtime via `Arc<parking_lot::RwLock<Option<AnyProvider>>>` — provider is NOT fixed at construction
- Builtin commands (`/exit`, `/clear`, `/compact`, etc.) short-circuit `process_user_message` and return `Some(bool)`
- All sub-state is held in dedicated named structs — no loose fields: `MessageState`, `MemoryState`, `SkillState`, `McpState`, `IndexState`, `DebugState`, `SecurityState`, `ToolState`, `RuntimeConfig`, and others

**NEVER**: make the agent generic over a Provider type; it uses `AnyProvider` (enum dispatch).

## 3. LLM Provider Contract

`LlmProvider` trait (`crates/zeph-llm/src/provider.rs`):

- All methods are `&self` (immutable) — providers are **concurrent-safe and shared via `Arc`**
- Object-safe: no generic methods, returns `impl Future<Output = Result<...>> + Send`
- Three distinct capabilities: `chat`, `chat_stream`, `chat_with_tools` — must be independent codepaths
- `debug_request_json()` must always return the exact JSON payload that would be sent to the API
- `AnyProvider` enum wraps all implementations — no `Box<dyn LlmProvider>` in hot paths

**NEVER**: add `&mut self` to provider methods or store mutable state without interior mutability.

## 4. Message & MessagePart Contract

`Message` / `MessagePart` (`crates/zeph-llm/src/provider.rs`):

- `MessagePart` uses `#[serde(tag = "kind", rename_all = "snake_case")]` — the `kind` field is the discriminator
- System message **must be first** in the messages vector (index 0)
- `ThinkingBlock` and `RedactedThinkingBlock` must be forwarded verbatim to the next request — never stripped
- `Compaction` variant must be preserved in conversation history for context continuity
- `MessageMetadata` controls visibility: `agent_visible`, `user_visible`, `focus_pinned` — all three must be checked
- `ToolOutput` part stores `compacted_at: Option<i64>` — set when summarized, never removed

**NEVER**: strip thinking blocks, compaction markers, or reorder the system message.

## 5. Tool Execution Contract

`ToolExecutor` trait (`crates/zeph-tools/src/executor.rs`):

- Held as `Arc<dyn ErasedToolExecutor>` in Agent — shared, immutable from agent perspective
- All tool execution goes through the native `tool_use` path — there is no legacy text-based path
- Two trust gates: `execute_tool_call` (pre-approved) vs `execute_tool_call_confirmed` (requires user approval)
- Returns `Option<ToolOutput>` — `None` = this executor doesn't own the tool (pass to next in composite chain)
- `CompositeExecutor` chains executors; order matters (first `Some(...)` wins)
- Shell blocklist check runs **unconditionally before** `PermissionPolicy` — cannot be bypassed

**NEVER**: collapse the two trust-gate methods into one; approval logic must remain separate.

## 6. Memory Pipeline Contract

`SemanticMemory` (`crates/zeph-memory/`):

- Held as `Option<Arc<SemanticMemory>>` — agent works without memory (graceful degradation)
- Backend: SQLite (relational history) + Qdrant (vector search) — both must be consistent
- Messages are **never deleted** — only marked `compacted_at` or summarized
- Tool pair summarization is **deferred**: stored on the message, applied when context pressure rises (not eagerly)
- Compaction thresholds: soft at ~60% context used, hard at ~90% — both must be honored
- Three recall sources (injected in order): semantic recall → code context → graph facts

**NEVER**: delete messages, skip deferred summary application at context pressure, or mix up recall source order.

## 7. Skill Matching Contract

`SkillRegistry` / `SkillMatcher` (`crates/zeph-skills/`):

- Registry is `Arc<RwLock<SkillRegistry>>` — shared, hot-reloadable without agent restart
- `max_active_skills` limits skills injected into the system prompt per turn — enforce strictly
- Matching priority: BM25 + embedding hybrid (if enabled) → pure embedding → keyword fallback
- `disambiguation_threshold` float gate: if top skill score < threshold, no skill is injected
- Hot-reload via `notify` crate with 500ms debounce — file change must not block the agent loop

**NEVER**: inject more than `max_active_skills` into a single turn; do not block on skill file I/O in the agent loop.

## 8. Configuration Contract

`Config` struct (`crates/zeph-core/src/config/`):

- Mandatory top-level sections: `agent`, `llm`, `skills`, `memory`, `tools` — absence is a hard error
- All other sections: `#[serde(default)]` — absence must produce sensible defaults, never panic
- Secrets are resolved via `VaultProvider` (age/env backend) into `ResolvedSecrets` — never store secrets inline in TOML
- Config is watched at runtime (`notify`) — reload must not drop in-flight requests
- Migration (`--migrate-config`): when a field is renamed/added, a migration step must be added before removing old field

**NEVER**: add required (non-default) fields to optional config sections; never read raw secrets from TOML.

## 8a. Provider Registry Contract

`[[llm.providers]]` (`crates/zeph-config/src/providers.rs`) is the **single source of truth** for all LLM provider declarations:

- All providers are declared **once** in `[[llm.providers]]` with a `name` field; no other section duplicates provider credentials or model names
- Subsystems that call LLMs reference a provider by name via a `*_provider` config field (e.g., `extract_provider = "fast"`)
- When `*_provider` is empty or absent, the subsystem falls back to the first (default) provider in the pool
- An unknown `*_provider` name produces a warning and falls back to the default provider — never a hard error
- `RoutingStrategy` is a property of the pool (`[llm] routing = "cascade"`), not a separate provider type
- The first entry in `[[llm.providers]]` is the default unless one entry has `default = true`; exactly one provider is the default
- Exactly one entry must have `embed = true` or `embedding_model` set; if none, the default provider handles embeddings with a warning if it lacks that capability
- Provider names must be unique; duplicate names are a config error at startup

**NEVER**: inline a model name, base URL, or provider credentials inside a subsystem config section; never resolve a provider by type string ("openai") — always by configured `name`; never add a new `[llm.*]` sub-section with its own provider credentials.

See `.local/specs/022-config-simplification/spec.md` for the full schema and examples.

## 9. Feature Flag Contract

Feature flags (`Cargo.toml [features]`):

- `default = []` — nothing is enabled by default; explicit bundles required
- **Always-on** (compiled in without feature flags): openai, compatible, orchestrator, router, self-learning, qdrant, vault-age, mcp
- New optional crates: `dep:zeph-<name>` in the feature definition — never unconditionally import
- Optional features that extend the TUI: use `zeph-tui?/feature-name` (conditional propagation)
- Bundles (`desktop`, `ide`, `server`, `full`) are the only way to enable groups of features
- CI MUST use `--features full` for lint and test runs — partial feature builds do not count

**NEVER**: enable optional features by default; never skip `--features full` in pre-merge checks.

## 10. Concurrency & Safety Contract

- `unsafe_code = "deny"` workspace-wide — zero exceptions
- Agent loop is **single-threaded async** — no parallel threads per session
- Shared mutable state: `parking_lot::RwLock<T>` (readers-preferred) or `parking_lot::Mutex<T>` (exclusive access) — no poison handling needed
- No blocking I/O inside async hot paths — use `tokio::task::spawn_blocking` if unavoidable
- TUI and ACP stdio transport are **mutually exclusive** (both own stdin/stdout) — enforce at startup
- MCP child process stderr must be suppressed in TUI mode

**NEVER**: use `std::sync::Mutex` or `std::sync::RwLock` in async code; never call blocking I/O directly inside `.await` chains.

## 11. Error Handling Contract

- Library crates (`zeph-*`): `thiserror` typed errors — every error variant is named and meaningful
- Binary (`src/main.rs`) and `zeph-core` agent code: `anyhow::Result` for top-level propagation
- `ToolError::kind()` classifies: `Transient` (retry) vs `Permanent` (abort) — callers must check
- No `unwrap()` or `expect()` in production paths — only in tests and `main()` for unrecoverable init failures

**NEVER**: use `panic!()` in library code; never ignore `ToolError::kind()` classification.

## 12. Integration Points (mandatory for every new feature)

Every new feature MUST be wired at all applicable integration points:

1. `config.toml` section with `#[serde(default)]`
2. CLI argument or subcommand (`clap`)
3. TUI command palette entry or `/` input command
4. `--init` wizard step (if user-configurable)
5. `--migrate-config` migration step (if config shape changes)
6. Background TUI operations: visible spinner + short status message (e.g., `Searching memory…`)

**NEVER**: ship a feature that is configurable but has no config section, or that runs silently in the background without a TUI indicator.

---

## 13. Database Backend Contract

`zeph-db` crate (feature-flag selected at compile time):

- `sqlite` and `postgres` features in `zeph-db` are **mutually exclusive** — enabling both is a `compile_error!`
- Default build uses `zeph-db/sqlite` (included via root `default` features) — PostgreSQL is always opt-in
- `--all-features` is **not a supported build mode** — use `--features full` for standard builds; `--features full,postgres --no-default-features` for PostgreSQL
- All consumer crates use `zeph_db::DbPool` — never `sqlx::SqlitePool` or `sqlx::PgPool` directly
- All SQL strings in consumer crates must pass through `sql!()` macro for placeholder compatibility
- Both migration directories (`sqlite/` and `postgres/`) must have matching file counts and schema-equivalent content
- `ZEPH_DATABASE_URL` is the canonical vault key for PostgreSQL credentials — never inline in TOML

**NEVER**: use `sqlx::Any` backend; enable both `sqlite` and `postgres` features simultaneously; reference raw sqlx pool types in consumer crates.

## 14. Memory Admission Control Contract

`AdmissionControl` in `zeph-memory::admission`:

- `remember()` returns `Result<Option<MessageId>>` — `None` means rejected by admission control, not an error
- When `[memory.admission] enabled = false`, `remember()` always returns `Some(_)` (pass-through)
- Admission scoring failure is **fail-open** — content is admitted on any scoring error
- Admission score is computed per-call and never stored
- Weight sum in `[memory.admission.weights]` must equal 1.0 ± 0.01

**NEVER**: treat `None` from `remember()` as an error; use admission control as a security gate; store admission scores.

## 15. RuntimeLayer Contract

`RuntimeLayer` trait (`crates/zeph-core/src/runtime_layer.rs`):

- All hooks have default no-op implementations — never required to override
- Hook failures are **non-fatal** — panics or errors in layer hooks must not abort the agent turn
- `LayerContext.turn_number` is incremented exactly once per user turn, before any hooks fire
- `before_chat` fires before the LLM call; `after_chat` fires after response receipt
- `before_tool` fires before executor dispatch; `after_tool` fires after `Option<ToolOutput>` resolves
- No blocking I/O in hooks — all are called synchronously in the async loop

**NEVER**: block the agent loop in a hook; store `Box<dyn RuntimeLayer>` in the agent (use `Arc<dyn RuntimeLayer>`).

---

## Agent Boundaries

### Always (without asking)
- Preserve all trait method signatures exactly
- Keep `default = []` in `[features]`
- Maintain `kind` discriminator in `MessagePart` serde
- Enforce `max_active_skills` limit in skill injection
- Run blocklist check before permission policy in shell executor
- Use `Arc<dyn RuntimeLayer>` not `Box<dyn RuntimeLayer>` in agent struct
- Return `Result<Option<MessageId>>` from `remember()` — never `Result<MessageId>`

### Ask First
- Adding a new `MessagePart` variant (breaks serialization compatibility)
- Changing `AnyChannel` or `AnyProvider` enum variants
- Adding required (non-default) config fields
- Removing or renaming existing feature flags
- Adding a new always-on capability (currently compiled without a feature flag)
- Adding new hook methods to `RuntimeLayer` (requires default impl to avoid breaking changes)
- Adding fields to `LayerContext` (affects all RuntimeLayer hook signatures)

### Never
- Add `async-trait` to library crates
- Make `LlmProvider` methods `&mut self`
- Use `Box<dyn Channel>` or `Arc<dyn Channel>`
- Delete messages from conversation history
- Use `unsafe` blocks
- Commit secrets to source files
- Skip `--features full` in pre-merge checks
- Enable `sqlite` and `postgres` features simultaneously
- Use `sqlx::Any` backend
- Block the agent loop in a `RuntimeLayer` hook
