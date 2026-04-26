---
aliases:
  - Agent Decomposition
  - Services Aggregator
  - AgentRuntime Newtype
  - Agent God-Object Phase 2
tags:
  - sdd
  - spec
  - core
  - refactoring
  - cross-cutting
created: 2026-04-26
status: draft
related:
  - "[[MOC-specs]]"
  - "[[001-system-invariants/spec]]"
  - "[[002-agent-loop/spec]]"
  - "[[027-runtime-layer/spec]]"
  - "[[039-background-task-supervisor/spec]]"
---

# Spec: Agent God-Object Decomposition (Services + AgentRuntime)

> [!info]
> Phase 2, prereq 2 of the Agent decomposition epic (#3498). Splits `Agent<C>`'s 25+ direct sub-state fields into three orthogonal, separately-borrowable groupings — conversation core (stays on `Agent`), `services: Services` (background subsystems), and `runtime: AgentRuntime` (configuration + lifecycle + telemetry). Pure refactor: no public API change, no behavioral change, no new abstractions.

## Sources

- GitHub issue [#3509](https://github.com/bug-ops/zeph/issues/3509) — task definition and acceptance criteria
- GitHub issue [#3498](https://github.com/bug-ops/zeph/issues/3498) — parent epic (Phase 2 god-object decomposition)
- GitHub issue [#3497](https://github.com/bug-ops/zeph/issues/3497) — Phase 1 (file splits + test extraction; merged at `80593883`)
- `crates/zeph-core/src/agent/mod.rs:175–223` — current `Agent<C>` struct definition with the `// TODO (A1 — deferred)` block at lines 158–174
- `crates/zeph-core/src/agent/state/mod.rs` — current sub-state struct definitions
- `.local/handoff/architect-plan.md` §A1 — original architect plan for this decomposition
- Companion reviews: `.local/handoff/arch-assessment-revised-2026-04-26T02-04-23.md` PR 8

---

## Goal

Eliminate the structural bottleneck where every method on `Agent<C>` must take `&mut self` because all 25+ sub-state structs are direct fields of `Agent`. Borrow-checker contention forces unrelated subsystems to serialize through one mutable borrow even when they touch disjoint state.

## Non-Goals

- **No new behavior.** This is a mechanical re-grouping of existing fields. No constructors, no traits, no helpers beyond field aggregators.
- **No public API change.** `Agent::new`, `Agent::run`, `Agent::shutdown`, and every `pub` slash-command handler keep their current signatures.
- **No `Services` trait, no `AgentRuntime` trait.** Plain structs with `pub(crate)` fields. Sealed traits and dependency-injection patterns are out of scope; they belong to a later phase if needed.
- **No `TurnContext` extraction.** Sketched here for awareness (P2-prereq-3) but not implemented.
- **No splitting of `MemoryState`.** Already split into four sub-structs in Phase 1; carried as a single unit.
- **No partial-borrow helper macros.** Plain field access (`self.services.mcp.foo()`) is sufficient; ergonomic helpers are deferred.

---

## Current State (mod.rs:175–223)

`Agent<C: Channel>` has the following fields. All sub-state structs live in `crates/zeph-core/src/agent/state/`. Counts are `self.<field>` references inside the `agent/` module subtree (see Migration Strategy for impact).

| # | Field | Type | Visibility | Refs |
|---|-------|------|------------|------|
| 1 | `provider` | `AnyProvider` | private | n/a — kept on Agent |
| 2 | `embedding_provider` | `AnyProvider` | private | n/a — kept on Agent |
| 3 | `channel` | `C` | private | n/a — kept on Agent |
| 4 | `tool_executor` | `Arc<dyn ErasedToolExecutor>` | `pub(crate)` | n/a — kept on Agent |
| 5 | `msg` | `MessageState` | `pub(super)` | (conversation core — stays) |
| 6 | `memory_state` | `MemoryState` | `pub(super)` | 239 |
| 7 | `skill_state` | `SkillState` | `pub(super)` | 130 |
| 8 | `context_manager` | `context_manager::ContextManager` | `pub(super)` | (conversation core — stays) |
| 9 | `tool_orchestrator` | `tool_orchestrator::ToolOrchestrator` | `pub(super)` | (conversation core — stays) |
| 10 | `learning_engine` | `learning_engine::LearningEngine` | `pub(super)` | (services group — see below) |
| 11 | `feedback` | `FeedbackState` | `pub(super)` | 8 |
| 12 | `runtime` | `RuntimeConfig` | `pub(super)` | 112 |
| 13 | `mcp` | `McpState` | `pub(super)` | 104 |
| 14 | `index` | `IndexState` | `pub(super)` | 12 |
| 15 | `session` | `SessionState` | `pub(super)` | 62 |
| 16 | `debug_state` | `DebugState` | `pub(super)` | 48 |
| 17 | `instructions` | `InstructionState` | `pub(super)` | 12 |
| 18 | `security` | `SecurityState` | `pub(super)` | 82 |
| 19 | `experiments` | `ExperimentState` | `pub(super)` | 10 |
| 20 | `compression` | `CompressionState` | `pub(super)` | 42 |
| 21 | `lifecycle` | `LifecycleState` | `pub(super)` | 94 |
| 22 | `providers` | `ProviderState` | `pub(super)` | 59 |
| 23 | `metrics` | `MetricsState` | `pub(super)` | 93 |
| 24 | `orchestration` | `OrchestrationState` | `pub(super)` | 101 |
| 25 | `focus` | `focus::FocusState` | `pub(super)` | 32 |
| 26 | `sidequest` | `sidequest::SidequestState` | `pub(super)` | 27 |
| 27 | `tool_state` | `ToolState` | `pub(super)` | 21 |
| 28 | `quality` (cfg `self-check`) | `Option<Arc<SelfCheckPipeline>>` | `pub(super)` | (services group) |
| 29 | `proactive_explorer` | `Option<Arc<ProactiveExplorer>>` | `pub(super)` | (services group) |
| 30 | `promotion_engine` | `Option<Arc<PromotionEngine>>` | `pub(super)` | (services group) |

Total in-module `self.<field>` references for the 19 grouped fields: **1286** (per `grep -rn` against `crates/zeph-core/src/agent/`).
External callers from outside `zeph-core` reaching into `agent.<field>`: **31 sites** across `crates/`.

---

## Target Structure

After the refactor, `Agent<C>` has exactly the following layout:

```rust
pub struct Agent<C: Channel> {
    // --- I/O & primary providers (kept inline) ---
    provider: AnyProvider,
    embedding_provider: AnyProvider,
    channel: C,
    pub(crate) tool_executor: Arc<dyn ErasedToolExecutor>,

    // --- Conversation core (kept inline) ---
    pub(super) msg: MessageState,
    pub(super) context_manager: context_manager::ContextManager,
    pub(super) tool_orchestrator: tool_orchestrator::ToolOrchestrator,

    // --- Aggregated background services ---
    pub(super) services: Services,

    // --- Aggregated runtime / lifecycle / telemetry ---
    pub(super) runtime: AgentRuntime,
}
```

Three groups, each a separately borrowable struct. **No `pub` fields on `Agent<C>` itself.** Visibility on aggregator fields stays `pub(super)` (same as today on the individual fields), preserving existing call sites' access rules.

### `Services` aggregator

Background subsystems that may be borrowed mutably **independently** of `runtime` and of conversation core. Defined in a new module: `crates/zeph-core/src/agent/state/services.rs`.

```rust
/// Aggregator for background subsystems borrowable independently of `AgentRuntime`
/// and conversation core. All fields are `pub(crate)` so the existing call-site
/// patterns inside `agent/*.rs` keep compiling after a mechanical field-path rewrite.
pub(crate) struct Services {
    pub(crate) memory: MemoryState,
    pub(crate) skill: SkillState,
    pub(crate) learning_engine: learning_engine::LearningEngine,
    pub(crate) feedback: FeedbackState,
    pub(crate) mcp: McpState,
    pub(crate) index: IndexState,
    pub(crate) security: SecurityState,
    pub(crate) experiments: ExperimentState,
    pub(crate) compression: CompressionState,
    pub(crate) orchestration: OrchestrationState,
    pub(crate) focus: focus::FocusState,
    pub(crate) sidequest: sidequest::SidequestState,
    pub(crate) tool_state: ToolState,
    pub(crate) session: SessionState,

    // Optional pipelines — moved here as services
    #[cfg(feature = "self-check")]
    pub(crate) quality: Option<std::sync::Arc<crate::quality::SelfCheckPipeline>>,
    pub(crate) proactive_explorer: Option<std::sync::Arc<zeph_skills::proactive::ProactiveExplorer>>,
    pub(crate) promotion_engine:
        Option<std::sync::Arc<zeph_memory::compression::promotion::PromotionEngine>>,
}
```

Rationale for inclusions beyond the issue's bullet list:

- **`learning_engine`** is grouped with the other learning subsystem (`feedback`); it is service-shaped (long-lived, called by tool execution and learning paths) and must move with `feedback` to keep their interaction borrow-clean.
- **`feedback`** is the data half of the learning subsystem (detector, judge, classifier).
- **`tool_state`** holds per-turn filtered-tool caches and dependency tracking — service-side bookkeeping that callers mutate alongside `mcp` / `tool_orchestrator`.
- **`session`** belongs in services: it bundles `EnvironmentContext`, response cache, LSP hooks, hooks config — none of which are conversation-core (which is strictly messages + context + orchestrator) and none of which are runtime-config (which is `Config`-derived).
- **`quality`, `proactive_explorer`, `promotion_engine`** are optional Arc-wrapped service singletons — they fit nowhere else.

The issue's bullet list is taken as **descriptive, not exhaustive**: any sub-state that is service-shaped lands in `Services`; any sub-state that is config / lifecycle / telemetry lands in `AgentRuntime`. This decision is recorded as the spec's binding contract.

#### Borrow model for `Services`

Most call sites today already mutate **one** sub-field at a time (`self.memory_state.persistence.foo()`, `self.mcp.sync_executor_tools()`). Migration is purely path-rewriting:

| Old | New |
|-----|-----|
| `&self.memory_state` | `&self.services.memory` |
| `&mut self.memory_state` | `&mut self.services.memory` |
| `&self.mcp` | `&self.services.mcp` |
| (etc. for every grouped field) | |

Two-field disjoint mutable borrows already work today via `&mut self` — they continue to work after the move because Rust supports disjoint mutable borrows through a struct: `let Services { memory, mcp, .. } = &mut self.services;` if a method ever needs both at once. **No proc-macro, no helper.** The compiler enforces disjointness.

`Services` itself does not need methods. Existing `impl McpState`, `impl IndexState`, `impl DebugState`, etc. stay where they are.

#### Shutdown ordering inside `Services`

`Agent::shutdown` today drops sub-state fields in declaration order. After this refactor, the same drop order is preserved by listing fields in `Services` in the same order as today's `Agent<C>` declaration. **No cross-aggregator ordering constraint exists** — `learning_engine`, `feedback`, `memory`, `session`, and `lifecycle` (the latter in `AgentRuntime`) do not have shutdown dependencies on each other:

- `learning_engine` is a passive store (events are buffered to SQLite; no async drain on shutdown).
- `feedback` is likewise passive (detector / judge / classifier own no resources requiring ordered teardown).
- `memory` (SQLite + optional Qdrant client) closes connections via `Drop` impls; no dependency on `learning_engine` or `feedback`.
- `session` (`EnvironmentContext`, response cache, LSP hooks, hooks config) is closed by `Drop`; no async dependency on the above.
- `lifecycle` (in `AgentRuntime`) holds the cancellation token and the supervisor handle. Cancellation is signalled before `shutdown` is called; by the time `Drop` runs on aggregators, all subsystem tasks are already joined.

Thus the placement of `learning_engine` inside `Services` (alongside `feedback`) is shutdown-safe regardless of whether `Services` is dropped before or after `AgentRuntime`. The struct field order in `Services` mirrors the current `Agent<C>` order to make this obvious in `cargo expand` output.

#### Visibility note on `Services` and `AgentRuntime` inner fields

Inner fields on both aggregators are declared `pub(crate)`, **not** `pub(super)` as currently used on `Agent<C>`. This is **intentional** for this PR:

- The 31 external-caller sites in `zeph-tui`, `zeph-channels`, `zeph-cli`, and workspace integration tests under `crates/*/tests/` reach into `agent.<field>` from outside the `agent::` module path. Today they compile because the fields are `pub(super)` on `Agent<C>` and those callers live inside `zeph-core` or sibling crates that have `pub(crate)`-visible re-exports. After moving fields under `services.` / `runtime.`, the inner fields must be reachable from the same external-caller sites — which require `pub(crate)`.
- Tightening to `pub(super)` is recorded as an out-of-scope follow-up (see §Out-of-Scope Follow-ups, "Services field visibility tightening"). It will land after the external reach-in sites are migrated to method calls in a separate PR.
- The aggregator fields **on `Agent<C>` itself** (`pub(super) services: Services`, `pub(super) runtime: AgentRuntime`) stay `pub(super)`, matching today's per-field visibility on `Agent<C>` exactly. No widening at that layer.

This is the only visibility delta in the PR. No other field on `Agent<C>` changes visibility.

### `AgentRuntime` newtype

Configuration snapshot, process lifecycle, provider catalog, telemetry, debug instrumentation, instructions hot-reload. None of these change per-turn except `metrics` and `lifecycle.turn_llm_requests`. Defined in `crates/zeph-core/src/agent/state/runtime.rs`.

```rust
/// Newtype aggregating runtime configuration, lifecycle, providers, metrics, debug,
/// and instructions. Borrowable independently of `Services` and conversation core.
pub(crate) struct AgentRuntime {
    pub(crate) config: RuntimeConfig,
    pub(crate) lifecycle: LifecycleState,
    pub(crate) providers: ProviderState,
    pub(crate) metrics: MetricsState,
    pub(crate) debug: DebugState,
    pub(crate) instructions: InstructionState,
}
```

**Naming note.** The existing inner type `RuntimeConfig` is already named `runtime` as an `Agent` field. To avoid the awkward `self.runtime.runtime`, the inner field is renamed `config` inside the newtype. Migration: `self.runtime.<x>` → `self.runtime.config.<x>` for the 112 references currently going through `self.runtime`. All other migrations are pure prefix changes (`self.lifecycle.<x>` → `self.runtime.lifecycle.<x>`).

`AgentRuntime` is named as a **newtype-style aggregator**, not a plain struct, because every consumer treats it as a single conceptual layer ("the agent's runtime envelope"). It does not need methods today; if shutdown plumbing eventually moves there, methods are added in a later PR.

---

## Migration Strategy

This is a single-PR refactor with mechanical rewrites and a fresh `Agent::new_with_registry_arc` initializer.

### Step 1 — Add the two aggregator structs

Create `crates/zeph-core/src/agent/state/services.rs` and `crates/zeph-core/src/agent/state/runtime.rs`. Re-export from `state/mod.rs`:

```rust
pub(crate) mod services;
pub(crate) mod runtime;

pub(crate) use self::services::Services;
pub(crate) use self::runtime::AgentRuntime;
```

Both structs are `pub(crate)` only — never `pub`. The agent module remains the sole owner.

### Step 2 — Rewrite `Agent<C>` field block

Replace lines 175–223 of `agent/mod.rs` with the target layout above. Visibility on the new aggregator fields is `pub(super)` (matches today's per-field `pub(super)` exactly).

### Step 3 — Rewrite `new_with_registry_arc`

The current constructor flat-initializes all 25+ fields in one `Self { … }` literal (annotated `#[allow(clippy::too_many_lines)]`). Restructure into three local builders:

```rust
let services = Services {
    memory: MemoryState::default(),
    skill: SkillState::new(registry, matcher, max_active_skills, last_skills_prompt),
    learning_engine: …,
    feedback: FeedbackState::default(),
    mcp: McpState::default(),
    index: IndexState::default(),
    security: …,
    experiments: ExperimentState::new(),
    compression: CompressionState::default(),
    orchestration: OrchestrationState::default(),
    focus: focus::FocusState::default(),
    sidequest: sidequest::SidequestState::default(),
    tool_state: ToolState::default(),
    session: SessionState::new(),
    #[cfg(feature = "self-check")]
    quality: None,
    proactive_explorer: None,
    promotion_engine: None,
};

let runtime = AgentRuntime {
    config: RuntimeConfig::default(),
    lifecycle: LifecycleState::new(),
    providers: ProviderState::new(initial_prompt_tokens),
    metrics: MetricsState::new(token_counter),
    debug: DebugState::default(),
    instructions: InstructionState::default(),
};

Self {
    provider, embedding_provider, channel, tool_executor,
    msg: MessageState { … },
    context_manager: …,
    tool_orchestrator: …,
    services,
    runtime,
}
```

### Step 4 — Field-path rewrite across `crates/zeph-core/src/agent/`

Structural rewrite against every `*.rs` file in the agent module subtree.

> [!warning]
> **Plain regex / sed / `sd` are NOT safe for this rewrite.** Two distinct collisions break naive textual substitution:
>
> 1. **`self.runtime` is ambiguous.** After Step 2, `Agent<C>` has a field `runtime: AgentRuntime`, and `AgentRuntime` has an inner field `config: RuntimeConfig`. A blind `s/self\.runtime/self.runtime.config/` will incorrectly rewrite legitimate accesses like `self.runtime.lifecycle` (which must remain `self.runtime.lifecycle`) into `self.runtime.config.lifecycle`. The rewrite must distinguish "old direct accesses to the `RuntimeConfig` (now nested as `runtime.config`)" from "new accesses to other `AgentRuntime` fields".
> 2. **`self.security` is ambiguous.** The current `RuntimeConfig` has its own sub-field `security` (config), AND `Agent<C>` has a top-level field `security: SecurityState`. After moving security state under `services`, the *config* `self.runtime.security` (now `self.runtime.config.security`) and the *service* `self.services.security` are distinct — but `self.security` (current code, before any rewrite) means the service. Plain regex cannot distinguish these without parsing.
>
> **Mandate:** use AST-aware rewriting. Either `ast-grep` with explicit field-path patterns, or `rust-analyzer`'s "rename field" refactor invoked per-field. **No `sed`, no `sd`, no `grep -P` substitution for this step.**

#### Tooling — choose one of these two approaches

**Option A (preferred): rust-analyzer "rename field" refactor.**

Invoke rename per-field through the LSP/CLI in IDE or via `rust-analyzer ssr` (Structural Search and Replace). One rename per source field, applied across the whole workspace at once. The compiler's name resolution disambiguates `self.runtime` (config) from `self.runtime.lifecycle` (newtype access) automatically because the source code, before Step 2, has no `runtime.lifecycle` to confuse with.

**Option B: `ast-grep` with explicit Rust patterns.**

`ast-grep` understands Rust syntax and matches structural field-access expressions, not text. Example invocations (one per field, run from workspace root):

```bash
# Memory state
ast-grep --lang rust --pattern 'self.memory_state' --rewrite 'self.services.memory' --update-all crates/

# Skill state
ast-grep --lang rust --pattern 'self.skill_state' --rewrite 'self.services.skill' --update-all crates/

# MCP, Orchestration, Security, Session, etc.
ast-grep --lang rust --pattern 'self.mcp' --rewrite 'self.services.mcp' --update-all crates/
ast-grep --lang rust --pattern 'self.orchestration' --rewrite 'self.services.orchestration' --update-all crates/
ast-grep --lang rust --pattern 'self.security' --rewrite 'self.services.security' --update-all crates/
ast-grep --lang rust --pattern 'self.session' --rewrite 'self.services.session' --update-all crates/
# ... (one per field in the rewrite table below)
```

For external-caller rewrites (`agent.<field>` outside the agent module), the same pattern works with the `agent` receiver — but use a metavariable to capture any binding name:

```bash
ast-grep --lang rust --pattern '$RECV.memory_state' --rewrite '$RECV.services.memory' --update-all crates/
```

#### Mandatory rewrite ordering

The rewrite **must** be performed in this exact order to avoid the ambiguities above. Each step requires `cargo check --workspace --all-features` to be green before proceeding to the next.

1. **First — rename the inner `runtime: RuntimeConfig` accesses.** Before Step 2 lands the new `AgentRuntime` newtype, rewrite every `self.runtime.<x>` (for `<x>` in the current `RuntimeConfig`'s public-to-the-module fields) to `self.runtime.config.<x>`. At this point `self.runtime` is still the `RuntimeConfig` and the rewrite is unambiguous because there is no `AgentRuntime` yet — every `self.runtime` access today is `RuntimeConfig`. Use `ast-grep --pattern 'self.runtime.$F' --rewrite 'self.runtime.config.$F'`. After this step, `self.runtime` (bare, no further field access) does not appear anywhere — verify with `ast-grep --pattern 'self.runtime' --debug-query` showing zero matches with no trailing field.
2. **Second — rename top-level service fields.** For each non-runtime service field (`self.memory_state` → `self.services.memory`, `self.security` → `self.services.security`, `self.mcp` → `self.services.mcp`, etc.), invoke `ast-grep` per-field. Order within this group does not matter; each pattern is unambiguous once Step 1 has cleared `self.runtime` ambiguity.
3. **Third — rename remaining runtime newtype fields.** `self.lifecycle` → `self.runtime.lifecycle`, `self.metrics` → `self.runtime.metrics`, `self.providers` → `self.runtime.providers`, `self.debug_state` → `self.runtime.debug`, `self.instructions` → `self.runtime.instructions`. These cannot collide with `self.runtime.<x>` from Step 1 because the inner `RuntimeConfig` fields (e.g., `security`, `cancel`, `session_id`, etc.) are different identifiers.
4. **Fourth — apply Step 2 of the migration (the actual struct definition change in `agent/mod.rs`).** The aggregator structs are introduced and the field block on `Agent<C>` is rewritten. After this step `cargo check` must succeed; if it fails, a rewrite was missed and `rustc` will name it.

**`self.security` disambiguation specifically.** After Step 1, `self.security` (bare) means the top-level service field; the *config* `security` is reachable only as `self.runtime.config.security`. Step 2's rewrite `self.security → self.services.security` is therefore safe — the only matches are the service.

**`self.skill` vs `self.skill_state`.** The `_state` suffix means these are different identifiers under AST matching; no collision.

**Note on `cargo fix`.** `cargo fix` only applies compiler-suggested rewrites and has no field-rename refactor — it cannot drive this migration. Listed for completeness; not used.

#### Rewrite table

Each row is one ast-grep / rust-analyzer rename invocation. Counts are `self.<field>` references inside `crates/zeph-core/src/agent/`.

| Old prefix | New prefix | Approx. sites | Rewrite step |
|------------|------------|---------------|--------------|
| `self.runtime.<F>` | `self.runtime.config.<F>` | 112 | Step 1 |
| `self.memory_state` | `self.services.memory` | 239 | Step 2 |
| `self.skill_state` | `self.services.skill` | 130 | Step 2 |
| `self.mcp` | `self.services.mcp` | 104 | Step 2 |
| `self.orchestration` | `self.services.orchestration` | 101 | Step 2 |
| `self.security` | `self.services.security` | 82 | Step 2 |
| `self.session` | `self.services.session` | 62 | Step 2 |
| `self.compression` | `self.services.compression` | 42 | Step 2 |
| `self.focus` | `self.services.focus` | 32 | Step 2 |
| `self.sidequest` | `self.services.sidequest` | 27 | Step 2 |
| `self.tool_state` | `self.services.tool_state` | 21 | Step 2 |
| `self.index` | `self.services.index` | 12 | Step 2 |
| `self.experiments` | `self.services.experiments` | 10 | Step 2 |
| `self.feedback` | `self.services.feedback` | 8 | Step 2 |
| `self.learning_engine` | `self.services.learning_engine` | (keep — moves under services) | Step 2 |
| `self.quality` (cfg) | `self.services.quality` | (small) | Step 2 |
| `self.proactive_explorer` | `self.services.proactive_explorer` | (small) | Step 2 |
| `self.promotion_engine` | `self.services.promotion_engine` | (small) | Step 2 |
| `self.lifecycle` | `self.runtime.lifecycle` | 94 | Step 3 |
| `self.metrics` | `self.runtime.metrics` | 93 | Step 3 |
| `self.providers` | `self.runtime.providers` | 59 | Step 3 |
| `self.debug_state` | `self.runtime.debug` | 48 | Step 3 |
| `self.instructions` | `self.runtime.instructions` | 12 | Step 3 |

**Total sites inside `crates/zeph-core/src/agent/`: ~1286 method-body rewrites.**
**Total sites outside agent module (other zeph-core code + other crates): ~31 — all reach `agent.<field>` rather than `self.<field>`. Same ast-grep patterns with `$RECV` metavariable.**

### Artifacts in scope (must be rewritten or regenerated)

The migration touches more than `.rs` source files. **All of the following are in-scope for the single PR** and must be updated together so CI passes:

1. **Production source files.** Every `.rs` under `crates/zeph-core/src/agent/` and the 31 external sites in `zeph-tui`, `zeph-channels` (acp), `zeph-cli`, etc.
2. **Workspace integration tests.** Every `.rs` under `crates/*/tests/` that constructs an `Agent<C>` or reaches into `agent.<field>`. These are not part of the agent module subtree but use the same field paths and must be migrated with the same ast-grep patterns.
3. **Doctest blocks.** `///` doc comments that contain ` ```rust ` blocks accessing `agent.<field>` paths. These are compiled by `cargo test --doc` and will fail loudly if missed. Run `cargo test --doc --workspace --features full` to catch them.
4. **Insta snapshot files (`*.snap`, `*.snap.new`).** Several agent-related tests use `insta` for golden-file assertions. Snapshots that embed debug-printed `Agent<C>` field paths or aggregator structures will diverge. Required regeneration:
   ```bash
   cargo insta test --workspace --features full --review
   # or, after manual review of changes:
   cargo insta accept --workspace
   ```
   The PR review must include reviewing the snapshot diffs — they should be limited to field-name changes (`memory_state` → `services.memory`, etc.), nothing more. Any semantic change in a snapshot is a bug.
5. **Inline comments and module-level docs.** `///` and `//!` comments referring to `Agent::memory_state`, `Agent::runtime` (as `RuntimeConfig`), etc. by name in prose. These do not break compilation but must be rewritten for consistency. Use grep manually after the AST rewrites.
6. **CHANGELOG.md.** A `[Unreleased]` entry describing the structural refactor.

### Step 5 — External callers

The 31 external `agent.<field>` sites live in `zeph-tui`, `zeph-channels` (acp), `crates/*/tests/`, and a handful of bin-side glue. Same `ast-grep --pattern '$RECV.<field>'` patterns apply.

### Step 6 — Verification

Run, in order, until all are green:

```bash
cargo +nightly fmt --check
cargo check --workspace --all-features --all-targets
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo nextest run --config-file .github/nextest.toml --workspace --features full --lib --bins
cargo nextest run --config-file .github/nextest.toml --workspace --features full -- --ignored
cargo test --doc --workspace --features full
cargo insta test --workspace --features full --review   # accept any pure field-rename snapshot diffs
RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --all-features -p zeph-core
```

Single PR boundary. Behavior must be identical; CI surface unchanged.

---

## TurnContext Boundary Sketch (P2-prereq-3 awareness)

> [!info]
> This section is **informational only** — no code lands here from this spec. It exists so the chosen `Services` / `AgentRuntime` split does not accidentally foreclose the next prereq.

`TurnContext` is the next prereq (#3498 sub-task) and represents the per-turn slice of state passed between context-assembly, tool-orchestration, and post-turn cleanup. Anticipated shape:

```rust
pub(crate) struct TurnContext<'a> {
    pub(crate) services: &'a mut Services,        // borrowed for the turn
    pub(crate) runtime: &'a mut AgentRuntime,     // borrowed for the turn
    pub(crate) msg: &'a mut MessageState,          // borrowed for the turn
    pub(crate) ctx: &'a mut ContextManager,
    pub(crate) iteration: usize,
    pub(crate) turn_intent: Option<String>,       // set at top of process_user_message
    pub(crate) cancel: CancellationToken,
}
```

The current refactor **enables** this by guaranteeing `services` and `runtime` are reachable through stable, separately-borrowable fields. Nothing in this spec commits to the `TurnContext` shape — it is sketched only to verify our split does not leave per-turn state stranded somewhere borrow-incompatible.

Validation: `current_turn_intent` (`SessionState`), `iteration_counter` (`DebugState`), `turn_llm_requests` (`LifecycleState`), `pending_timings` (`MetricsState`) are all reachable as `&mut` through one of `services` or `runtime`. ✓

---

## Acceptance Criteria

Mapping from issue #3509:

| Criterion | Implementation |
|-----------|----------------|
| `Agent<C>` direct fields reduced to three groups | Lines 175–223 of `agent/mod.rs` rewritten to: `provider`, `embedding_provider`, `channel`, `tool_executor`, `msg`, `context_manager`, `tool_orchestrator`, `services`, `runtime`. |
| `Services` and `AgentRuntime` are separately borrowable | They are sibling fields on `Agent`. `&mut self.services` and `&mut self.runtime` are disjoint borrows by construction. Any method touching only one borrows only that field. |
| SDD spec at `/specs/<area>/spec.md` defining `Services` contract, `AgentRuntime` interface, `TurnContext` boundary | This document. |
| All existing tests pass | Pre-PR: `cargo nextest run --workspace --features full --lib --bins` + `cargo nextest run -- --ignored` for Qdrant integration. |
| No `pub` fields added to `Agent<C>` itself | `Agent<C>` direct fields visibilities: `private` (provider, embedding_provider, channel), `pub(crate)` (tool_executor — pre-existing), `pub(super)` (msg, context_manager, tool_orchestrator, services, runtime). **No `pub`.** |

Additional invariants:

- No public API change. `Agent::new`, `Agent::new_with_registry_arc`, `Agent::run`, `Agent::shutdown` preserve their signatures.
- No new dependencies added to `zeph-core`.
- No clippy regression. The pre-existing `#[allow(clippy::too_many_lines)]` on the constructor remains; line count drops because the literal is split into three local `let`s.
- Visibility tightening or loosening is **out of scope** for this PR.

---

## Key Invariants

> [!important]
> These are non-negotiable constraints. Violating any of them invalidates the refactor.

1. **NEVER expose `pub` fields on `Agent<C>`.** Aggregator fields on `Agent<C>` are `pub(super)`. Aggregator inner fields (on `Services` and `AgentRuntime`) are `pub(crate)` — intentionally wider than the `pub(super)` they had as direct `Agent<C>` fields, because external-crate callers (`zeph-tui`, `zeph-channels`, `crates/*/tests/`) reach into them. Tightening this to `pub(super)` is recorded as a separate follow-up PR. Anything wider than `pub(crate)` must be added through an explicit method, not a field visibility bump.
2. **NEVER add a `Services` trait or an `AgentRuntime` trait in this PR.** They are plain structs. Trait abstractions are deferred until a concrete second implementor exists.
3. **`Services` and `AgentRuntime` MUST be separately borrowable.** No method may take `&mut Services` and `&mut AgentRuntime` from `&mut self` through anything other than direct field access on `self`. (Direct access is what makes the borrows disjoint.)
4. **NO behavior change.** Every `cargo nextest` test that passes today must pass post-refactor without modification beyond field-path renames in fixtures or test helpers.
5. **`learning_engine`, `quality`, `proactive_explorer`, `promotion_engine`, `tool_state`, `session` MUST land in `Services`.** The issue's bullet list is descriptive; this spec is the binding contract.
6. **`RuntimeConfig` field is renamed `config` inside `AgentRuntime`.** Justification recorded in §AgentRuntime newtype.
7. **NEVER bundle other refactors into the same PR.** No method extraction, no method renames, no visibility tweaks, no new helpers. Pure field re-grouping.
8. **NO partial-borrow macros, NO `BorrowMut` impls.** Plain field access only.

---

## Out-of-Scope Follow-ups (recorded for the next architects)

- **TurnContext extraction (P2-prereq-3, #3498 sub-task).** Sketched above.
- **Per-service `&mut self` API surfaces.** Today's pattern (`McpState::sync_executor_tools(&self)`, `IndexState::fetch_code_rag(&self, …)`) is fine. Promotion to traits or partial-borrow helpers is deferred.
- **`Services` field visibility tightening to `pub(super)`.** Possible after migration once external reach-in sites are migrated to method calls — separate PR.
- **Crate extraction.** The whole epic (#3498) is motivated by enabling future crate splits. This PR removes the structural blocker; actual crate extraction is a later phase.
- **`learning_engine` decomposition.** Currently a single struct; if it grows further it should split inside `Services` rather than being lifted back to `Agent`.

---

## Verification Checklist (for the developer agent)

- [ ] `Agent<C>` has exactly nine direct fields (or eight on builds without `tool_executor` cfg variations — none today).
- [ ] No field on `Agent<C>` is `pub`.
- [ ] `Services` and `AgentRuntime` are `pub(crate)` structs in `state/services.rs` and `state/runtime.rs`.
- [ ] `Services` has all 14 fields enumerated above (15 with optional `quality`, `proactive_explorer`, `promotion_engine`; 17 total when `self-check` feature is enabled).
- [ ] `AgentRuntime` has the six fields enumerated above.
- [ ] `RuntimeConfig` is reachable as `self.runtime.config`.
- [ ] No use of `self.<old-field>` survives anywhere under `crates/` (verify with `ast-grep --pattern 'self.memory_state'` etc., expect zero matches).
- [ ] No external-caller use of `agent.<old-field>` survives anywhere under `crates/` (verify with `ast-grep --pattern '$RECV.memory_state'` and the rewritten patterns; expect matches only of the new `agent.services.<field>` / `agent.runtime.<field>` form).
- [ ] Doctest blocks in `///` comments use the new field paths (caught by `cargo test --doc`).
- [ ] Insta snapshots regenerated and reviewed: every `*.snap` diff contains only field-path renames, no semantic changes (`cargo insta test --workspace --features full --review`).
- [ ] Workspace integration tests under `crates/*/tests/` migrated to new field paths (caught by `cargo nextest`).
- [ ] `cargo +nightly fmt --check` passes.
- [ ] `cargo clippy --workspace --all-features --all-targets -- -D warnings` passes.
- [ ] `cargo nextest run --config-file .github/nextest.toml --workspace --features full --lib --bins` passes.
- [ ] `cargo nextest run --config-file .github/nextest.toml --workspace --features full -- --ignored` passes (Qdrant integration).
- [ ] `cargo test --doc --workspace --features full` passes.
- [ ] `RUSTDOCFLAGS="--deny rustdoc::broken_intra_doc_links" cargo doc --no-deps --all-features -p zeph-core` builds clean.
- [ ] CHANGELOG entry under `[Unreleased]` describing the structural refactor.
- [ ] No new `tracing::warn`, no new metric, no new config key.
