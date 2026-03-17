# Spec: Zeph System Invariants

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
- Provider can be swapped at runtime via `Arc<RwLock<Option<AnyProvider>>>` — provider is NOT fixed at construction
- Builtin commands (`/exit`, `/clear`, `/compact`, etc.) short-circuit `process_user_message` and return `Some(bool)`
- All sub-state is held in named structs (`MemoryState`, `SkillState`, `McpState`, etc.) — no loose fields

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
- Two execution paths: structured (`execute_tool_call`) and legacy text-based (`execute`) — both must work
- Two trust gates: plain `execute` (pre-approved) vs `execute_confirmed` (requires user approval)
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
- Shared mutable state: `Arc<RwLock<T>>` (readers-preferred) or `Arc<Mutex<T>>` (exclusive access)
- No blocking I/O inside async hot paths — use `tokio::task::spawn_blocking` if unavoidable
- TUI and ACP stdio transport are **mutually exclusive** (both own stdin/stdout) — enforce at startup
- MCP child process stderr must be suppressed in TUI mode

**NEVER**: use `std::sync::Mutex` in async code or call blocking I/O directly inside `.await` chains.

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

## Agent Boundaries

### Always (without asking)
- Preserve all trait method signatures exactly
- Keep `default = []` in `[features]`
- Maintain `kind` discriminator in `MessagePart` serde
- Enforce `max_active_skills` limit in skill injection
- Run blocklist check before permission policy in shell executor

### Ask First
- Adding a new `MessagePart` variant (breaks serialization compatibility)
- Changing `AnyChannel` or `AnyProvider` enum variants
- Adding required (non-default) config fields
- Removing or renaming existing feature flags
- Adding a new always-on capability (currently compiled without a feature flag)

### Never
- Add `async-trait` to library crates
- Make `LlmProvider` methods `&mut self`
- Use `Box<dyn Channel>` or `Arc<dyn Channel>`
- Delete messages from conversation history
- Use `unsafe` blocks
- Commit secrets to source files
- Skip `--features full` in pre-merge checks
