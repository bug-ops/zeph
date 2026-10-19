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
| Config struct | `crates/zeph-config/src/root.rs` |
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
- `ThinkingBlock` and `RedactedThinkingBlock` are forwarded verbatim, in response order, within the current turn (every assistant message after the last `MessageOrigin::TurnInput`). On models with preserved thinking (`preserved_thinking` in `[[003-llm-providers/spec]]`), thinking of earlier turns is dropped as a whole leading run; other models receive all thinking verbatim. At a history rewrite inside a turn (pressure prune, soft/hard compaction, `complete_focus`, `compress_context`, the server-compaction mirror, a deferred-summary insertion) thinking is stripped once as a whole trailing run from the lowest touched index. The strip goes through `strip_thinking_after_edit(.., provider.thinking_binding())` and happens only for `ThinkingBinding::PrefixBound` providers (thinking signed over the preceding prefix); `Unbound` providers keep their thinking. The `ThinkingReplay::Dropped` and binding-mismatch retry strips stay unconditional. Thinking is never persisted (`MessagePart::strip_for_persistence`)
- `Compaction` variant must be preserved in conversation history for context continuity
- `MessageMetadata` controls visibility: `agent_visible`, `user_visible`, `focus_pinned` — all three must be checked
- `ToolOutput` part stores `compacted_at: Option<i64>` — set when summarized, never removed

- `MessageMetadata.origin: Option<MessageOrigin>` marks messages the loop must find after history rewrites. `MessageOrigin::TurnInput` is set on the user message that starts a turn (`build_user_message`); at most one message carries it (`MessageState::supersede_turn_input` clears older markers before the new turn input is pushed)
- `zeph_llm::provider::context_floor(&[Message])` = 1 + index of the last message carrying a `Compaction` part, else 1. It is the only head boundary: every pass that inserts at, drains from, selects from, or tags the head of history starts at the floor and never moves, reorders, or inserts in front of the compaction block (see `[[002-agent-loop/spec]]`). Head inserts go through `insert_at_context_floor`
- `MessageState::set_system_prompt` is the only writer of the system prompt: it overwrites `messages[0]` only when it is `Role::System` (clearing its `parts`), otherwise it inserts a new System message at index 0. Writing the prompt into a non-System head silently drops it from requests (#6815)

- `MessagePart::RuntimeNote { text }` is agent-authored per-iteration state (remaining tool budget, LSP notes, utility hints). Contract: attached only to the unsent, agent-visible tail `User` message that carries `ToolResult` parts (loop iteration 1 or later), at most one per message, via `Message::append_runtime_note` (`NoteAppend::{Appended, AlreadyPresent, NotAToolResultMessage}`); append-only (never removed or edited once sent); excluded from `flatten_parts`/`Message::content`/`as_plain_text`; included by `MessagePart::as_request_text` and `Message::to_request_text` (the single helper for content-based builder paths); never persisted (`MessagePart::strip_for_persistence` at every persistence sink); a message left with only notes counts as empty for repair
- Site rule: `MessagePart` is `#[non_exhaustive]`, so a wildcard arm silently drops a note or leaks it. Every consumer names `RuntimeNote` explicitly: request builders (Claude text block after the `tool_result` blocks, OpenAI `user` message after the `tool` messages, Gemini text part after `functionResponse`, Ollama `user` message after the `tool` message, Candle template), `masking.rs` (both text accessors), `TokenCounter`, subagent size estimate and `trim_parent_messages` (strips), debug dump (labelled), persistence sinks (SQLite, session log, subagent transcript), Layer B repair, ACP/TUI display (`as_plain_text` exclusion)
- Rendering: `<runtime-note id="{nonce}">…</runtime-note>` where the nonce is a per-session uuid generated once (stable prefix) and named in the system prompt as the only genuine id. Agent-authored text inside a section (LSP, hints) is escaped with `DelimiterTag::RuntimeNote`; tool output is NOT escaped for this tag, so file reads and edits stay byte-exact and a forged note is distinguishable only by its id (see `[[040-sanitizer/spec]]`)

**NEVER**: remove a thinking block from the middle of a turn (a gap), reorder thinking relative to the text and `tool_use` blocks of its response, strip compaction markers, or reorder the system message. Thinking is only ever removed as a whole leading run (earlier turns) or a whole trailing run (history rewrite), because each block is signed over everything before it.

## 5. Tool Execution Contract

`ToolExecutor` trait (`crates/zeph-tools/src/executor.rs`):

- Held as `Arc<dyn ErasedToolExecutor>` in Agent — shared, immutable from agent perspective
- All tool execution goes through the native `tool_use` path — there is no legacy text-based path
- Two trust gates: `execute_tool_call` (pre-approved) vs `execute_tool_call_confirmed` (requires user approval)
- Returns `Option<ToolOutput>` — `None` = this executor doesn't own the tool (pass to next in composite chain)
- `CompositeExecutor` chains executors; order matters (first `Some(...)` wins)
- Shell blocklist check runs **unconditionally before** `PermissionPolicy` — cannot be bypassed
- `ClaimSource::Synthetic` marks harness-authored placeholders for calls that never ran; `ClaimSource::ContextManagement` marks the results of `start_focus`, `complete_focus`, `compress_context` and `request_compaction`, which did run. `ToolError::is_local_policy_denial()` is true only for denials raised by zeph's own policy layer (see `[[040-sanitizer/spec]]`)

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
- The matcher/threshold/`max_active_skills` cap applies to every system prompt build, including
  the initial prompt at agent construction and any hot-reload rebuild that runs before the first
  per-turn query is available — see `specs/005-skills/spec.md` § Construction-Time / Reload-Time
  Skill Prompt Contract for the catalog-only fallback used when no query exists yet (#6413)

**NEVER**: inject more than `max_active_skills` into a single turn; inject full skill bodies into
a system prompt built without a per-turn query (construction, hot-reload); do not block on skill
file I/O in the agent loop.

## 8. Configuration Contract

`Config` struct (`crates/zeph-core/src/config/`):

- Mandatory top-level sections: `agent`, `llm`, `skills`, `memory`, `tools` — absence is a hard error
- All other sections: `#[serde(default)]` — absence must produce sensible defaults, never panic
- Secrets are resolved via `VaultProvider` (age/env backend) into `ResolvedSecrets` — never store secrets inline in TOML
- Config is watched at runtime (`notify`) — reload must not drop in-flight requests
- Migration (`--migrate-config`): when a field is renamed/added, a migration step must be added before removing old field

**NEVER**: add required (non-default) fields to optional config sections; never read raw secrets from TOML.

## 8a. Provider Registry Contract

`[[llm.providers]]` (`crates/zeph-config/src/providers/` — `mod.rs`, `entry.rs`, `llm.rs`, `router.rs`, `thinking.rs`, `candle.rs`) is the **single source of truth** for all LLM provider declarations:

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

- `default = ["scheduler", "sqlite"]` — the minimal set required for the standard CLI experience: cron scheduler and the SQLite database backend. Per-task CPU/wall-time metrics are an always-on capability (no flag, see spec 029 §3.3), not a default feature. See spec 029 §3.1 for the canonical list.
- Default features must satisfy two criteria: (a) the feature gates a real optional dependency (not a pure behavioral marker), and (b) the feature has `Tested` status in coverage-status.md with no open P0/P1 issues.
- **Always-on** (compiled in without feature flags): openai, compatible, orchestrator, router, self-learning, qdrant, vault-age, mcp
- New optional crates: `dep:zeph-<name>` in the feature definition — never unconditionally import
- Optional features that extend the TUI: use `zeph-tui?/feature-name` (conditional propagation)
- Bundles (`desktop`, `ide`, `server`, `full`) are the only way to enable groups of features
- CI MUST exercise the `full` configuration on every PR — satisfied by four independent legs (coverage, `bundle-check (full)`, `release-build-full`, `ci-non-linux.yml`) rather than a literal `--features full` on the hot lint/test path; see spec 029 §5.6

**NEVER**: add a pure behavioral marker to `default` (must gate real optional deps per spec 029 §2); never drop one of the four CI legs that together exercise `full` without replacing it.

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

## 16. RuntimeContext Contract

`RuntimeContext` (`zeph_core::RuntimeContext`, `Copy + Clone + Debug + Default + PartialEq + Eq`) is the single carrier for process-scoped runtime mode flags:

- Fields: `tui_mode: bool`, `daemon_mode: bool`
- Passed as a single parameter to all subsystem initializers in `runner.rs`, `tracing_init.rs`, and `daemon.rs` — never passed as individual `bool` parameters
- `suppress_stderr()` helper method centralizes the TUI/daemon stderr suppression decision — never inline it
- `Default` = `RuntimeContext { tui_mode: false, daemon_mode: false }` (CLI mode)

**NEVER**: add `tui_mode: bool` or `daemon_mode: bool` as separate parameters to subsystem init functions; always pass `RuntimeContext`.

---

## Agent Boundaries

### Always (without asking)
- Preserve all trait method signatures exactly
- Only add features to `default` that gate real optional deps AND have `Tested` coverage status (per spec 029 §2 + §3.1)
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
- Pass `tui_mode` or `daemon_mode` as individual `bool` parameters to subsystem initializers (use `RuntimeContext`)
