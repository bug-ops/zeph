---
aliases:
  - Cross-Thread Store
  - Command Handoff
  - Store + Command Dynamic Handoff
  - Spec 080
  - LangGraph Store/Command Parity
tags:
  - sdd
  - spec
  - memory
  - orchestration
  - security
  - cross-cutting
created: 2026-07-17
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[004-memory/spec]]"
  - "[[009-orchestration/spec]]"
  - "[[010-security/spec]]"
  - "[[031-database-abstraction/spec]]"
  - "[[039-background-task-supervisor/spec]]"
  - "[[040-sanitizer/spec]]"
  - "[[075-orchestration-node-control-parity/spec]]"
issues:
  - "#6363"
---

# Spec 080 — Cross-Thread Store + Command-Style Dynamic Task Handoff (GitHub #6363)

> [!info]
> Two coupled primitives, LangGraph parity: (1) a generic namespaced cross-thread KV `Store` in
> `zeph-memory`, and (2) a task-level `Command(update, goto)` terminal routing outcome in
> `zeph-orchestration`, where the store is the shared-state channel `Command.update` writes into.
> This spec is the formal `/sdd` output for an already-designed, two-round architect/critic
> reviewed feature (final verdict: **approved / minor**); it does not re-derive the architecture,
> it formalizes it into traceable requirements and resolves the one open validation gap the
> review left for spec-time decision (§6, N1 / FR-B-010).

## Sources

### External
- LangGraph (LangChain, Python), https://github.com/langchain-ai/langgraph — `Store` (namespaced
  cross-thread key-value persistence: `put`/`get`/`search`/`delete`, addressable by
  `(namespace, key)`) and `Command(update=..., goto=...)` (a node's return value that both mutates
  shared state and declares the next node to run, replacing static conditional edges with
  runtime-chosen routing). Source material for the parity finding in GitHub issue #6363. Zeph's
  MVP deliberately narrows both: `Store` ships as a plain relational KV (no built-in semantic
  search in v1) and `Command.goto` is forward-only (no backward/looping targets) — see §1 Out of
  Scope.

### Internal

| File | Contents |
|---|---|
| `crates/zeph-memory/src/store/preferences.rs` | Existing sub-store pattern (`SqliteStore` impl, `Dialect`/`sql!`/`rewrite_placeholders` idioms) that `cross_thread.rs` mirrors — global scope, no conversation/thread addressing |
| `crates/zeph-memory/src/store/persona.rs` | Existing sub-store with session-id *provenance* only, not addressable KV — closest existing analogue, still insufficient for #6363 |
| `crates/zeph-memory/src/semantic/cross_session.rs` | Existing cross-session *search* surface — semantic, not addressable-by-key; confirms no generic KV primitive exists today |
| `crates/zeph-db/migrations/sqlite/109_durable_promise_notified.sql`, `crates/zeph-db/migrations/postgres/109_durable_promise_notified.sql` | Highest existing migration pair at design time — this spec's migration 110 is the next free slot (re-verify against HEAD before implementation; see §6 Never) |
| `crates/zeph-db/migrations/sqlite/108_acp_session_owner.sql`, `crates/zeph-db/migrations/postgres/108_acp_session_owner.sql` | Precedent this spec's `owner_key` column follows — added to `acp_sessions` for issue #5868 (unscoped listing was "global across every connection sharing one SQLite store") |
| `crates/zeph-db/tests/migration_parity.rs` | Cross-dialect migration parity gate — both `110_cross_thread_store.sql` files (SQLite + Postgres) must define the same table/column/index set or the build fails |
| `crates/zeph-orchestration/Cargo.toml:39-46` | `zeph-memory.workspace = true` is listed under `[dev-dependencies]` (block starts `:39`), not `[dependencies]` (`:15`) — confirms `zeph-orchestration` has no production dependency on `zeph-memory` today; this spec's binding invariant (§6) keeps it that way |
| `crates/zeph-orchestration/src/scheduler/mod.rs:147-165` | `TaskOutcome` enum, `#[non_exhaustive]` (`:148`) — the new `Handoff` variant is additive-safe; existing `Completed`/`Failed` variants shown for contrast |
| `crates/zeph-orchestration/src/scheduler/mod.rs:220-325` | `DagScheduler` — holds no memory/persistence handle by design; persistence is threaded externally from `zeph-core` without the scheduler holding a reference |
| `crates/zeph-orchestration/src/dag.rs:61-` | `validate()` — structural DAG validation; new `try_handoff`-adjacent target-validation guards join the existing per-task checks here |
| `crates/zeph-orchestration/src/dag.rs:185-240` | `validate_route_to()` — plan-time validation of `recovery.route_to` targets; `:209` requires the target's `depends_on` be empty. This is the precedent §6/FR-B-010 (N1) mirrors for `Command.goto` |
| `crates/zeph-orchestration/src/dag.rs:328-` | `ready_tasks()` — `Ready`/`Pending` arms; how an activated Dormant/Pending node becomes dispatchable |
| `crates/zeph-orchestration/src/dag.rs:446-` | `try_reroute()` — existing Dormant→Ready activation for `route_to`; runtime guard rejects a non-`Dormant` target. `dag::try_handoff` (new) is a sibling function reusing this activation machinery |
| `crates/zeph-orchestration/src/scheduler/tick/mod.rs:571-` | `handle_completed_outcome()` — event-path outcome handling; gains a `Handoff` match arm |
| `crates/zeph-orchestration/src/scheduler/router.rs:18-59` | `build_task_prompt()` — builds `<completed-dependencies>`/`<recovery-source>` blocks; stays store-free. zeph-core appends the new `<shared-state>` block after this returns |
| `crates/zeph-core/src/agent/scheduler_loop.rs:180-368` | `on_done` spawn-path closure (`:194-231`, **synchronous**, detaches only the event-send via `spawn_oneshot`) and the inline `RunInline` path (`handle_run_inline_action`, `async fn`) — the produce-side parse, sanitizer scan, and store write for `Handoff` all execute inside the detached async send task on the spawn path (§5, N2 nuance), or inline on the `RunInline` path |
| `crates/zeph-core/src/agent/state/persistence.rs:23` | `self.services.memory.persistence.memory: Option<Arc<SemanticMemory>>` — the zeph-core-resident handle used for the store write/read |
| `crates/zeph-acp/src/custom.rs` | `list_acp_sessions_for_owner`/`acp_session_accessible_for_owner` — the existing per-owner filtering template `owner_key`-scoped store methods follow |
| `crates/zeph-sanitizer/src/vigil.rs:10,22` | Doc comments explicitly disclaiming injection-resistance: spotlighting "does spotlighting only... NOT a claim of injection resistance" — informs §4/§6 framing that the sanitizer scan is a checkpoint, not an elimination of risk |
| `crates/zeph-sanitizer/src/exfiltration.rs:320` | `ExfiltrationGuard::validate_tool_call` — the existing scan path a registered tool call gets automatically and an output-convention parse must be explicitly routed through |
| `crates/zeph-sanitizer/src/lib.rs:49-55` | `ContentTrustLevel` tiers — the external/spotlighted tier the new `<shared-state>` prompt block is wrapped in |
| `crates/zeph-config/src/experiment.rs:261-,274,333` | `OrchestrationConfig` — existing precedent for a validated `> 0` numeric field (`default_idle_timeout_secs`); `[orchestration.command]` follows the same pattern |
| `crates/zeph-config/src/migrate/mod.rs`, `crates/zeph-config/src/migrate/tests.rs:11,1907` | `MIGRATIONS` registry; `MIGRATIONS.len()` count-assertion (`90` at design time) — this spec's two new migrate steps bump it to `92` (re-verify against HEAD; §6 Never) |

---

## 1. Overview

### Problem Statement

`zeph-orchestration`'s DAG has two LangGraph-parity gaps (GitHub #6363):

- **(a) No generic cross-thread key-value store.** Existing persistence is either global with no
  conversation/thread scope (`store/preferences.rs`), provenance-only (`store/persona.rs`), or
  search-oriented rather than addressable-by-key (`semantic/cross_session.rs`). There is no
  `put`/`get`/`delete`/`list` primitive a graph node (or, later, other subsystems) can use to
  read and write namespaced state that outlives a single task and is visible to other tasks in
  the same graph.
- **(b) No runtime `Command(update, goto)`.** The only existing dynamic-routing primitive is
  `recovery.route_to: Option<TaskId>` (`dag.rs:185-240`, `:446-`) — declarative (chosen at
  plan-construction time), single-target, failure-only, and explicitly forbids chaining
  (`validate_route_to` requires an empty `depends_on` on the target). It has no notion of a
  successfully-completing node choosing, at runtime, where execution goes next and what state it
  hands off — the two things LangGraph's `Command` couples together.

### Goal

A DAG node's agent can, on successful completion, emit a `Command`-style directive that (1) writes
key-value updates into a generic cross-thread store scoped to the graph, and (2) routes execution
to a named node already present in the plan — with the store acting as the shared-state channel
the routed-to node reads from. Both primitives are additive, opt-in, and produce zero behavior
change for any graph/config that does not enable them.

### Out of Scope (v1 / MVP)

- **Semantic (embedding) search over store values.** MVP `search` is namespace-prefix + keyword
  match only. A `[memory.store] search_provider` field is reserved (declare-once `*_provider`
  pattern per `CLAUDE.md` §Multi-Model Design) but unused in v1.
- **Backward or looping `goto` targets** (true LangGraph-style cycles). MVP `Command.goto` is
  **forward-only**: a target must not already be `Completed`. Cycles are deferred to a Phase 2
  behind a visit-counter design (§6 Never).
- **A native `orch_handoff` LLM-facing tool.** MVP's produce side is an output-convention parse of
  a trailing fenced block, not a registered tool call. A native-tool alternative is a distinct
  future design that must be separately reconciled with issue #6234 / spec-073's INV-9 boundary
  before adoption (§4 NFR-SEC-04) — it does not automatically inherit this spec's clean boundary.
- **Migrating existing bespoke tables** (`learned_preferences`, persona facts, etc.) onto the new
  store. Those stay as-is; the new store is additive, not a consolidation.
- **Cross-process / cross-instance store access** beyond what the existing SQLite/Postgres
  backend already provides via `zeph-db`.
- **Exposing `Command` target selection to the LLM's tool schema** by default — the parse is a
  post-hoc convention on free-text output, never a `ToolDefinition`.
- **route_to subsumption.** `Command` is a parallel mechanism sharing only low-level Dormant→Ready
  activation machinery with `route_to`; it does not replace, extend, or change `route_to`'s
  existing declarative-failure semantics or code path (PR #6346 hardening is untouched).

---

## 2. User Stories

**US-1 — Orchestration author writes shared state across a graph.**
As an orchestration graph author, I want one task's agent to persist a value that a later task in
the same graph can read, so that multi-step plans can accumulate structured findings instead of
relying solely on prose passed through `<completed-dependencies>` text.

- *Acceptance*: GIVEN `[memory.store].enabled = true` and a running graph, WHEN a task's Command
  `update` writes `{"finding": "X"}` under the graph's namespace, THEN a subsequently-dispatched
  task in the same graph sees `finding: X` in its `<shared-state>` prompt block (FR-A-004,
  FR-B-006).

**US-2 — Orchestration author lets an agent choose the next step at runtime.**
As an orchestration graph author, I want a node's agent to be able to route execution to a
specific already-planned node based on what it found, rather than being limited to the graph's
static edges or `route_to`'s failure-only fallback, so that plans can branch on runtime content.

- *Acceptance*: GIVEN `[orchestration.command].enabled = true`, WHEN a completing node's final
  output ends with a well-formed ` ```zeph-command ` block naming a valid, not-yet-completed,
  dependency-satisfied target, THEN that target transitions `Dormant`/`Pending → Ready` and is
  dispatched next, with `commanded_from` recording the source (FR-B-002, FR-B-005, FR-B-010).

**US-3 — Operator trusts the feature is off by default and bounded when on.**
As an operator, I want both primitives disabled by default and, once enabled, bounded against
runaway routing loops and cross-tenant data leakage, so that enabling them does not introduce an
unbounded blast radius.

- *Acceptance*: GIVEN default config, WHEN a graph runs, THEN no store row is written and no
  `Handoff` outcome is possible (FR-A-001, FR-B-001). GIVEN `[orchestration.command].enabled =
  true`, WHEN handoffs occur, THEN the graph terminates after at most `max_handoffs` hops even
  under adversarial back-and-forth attempts, because a Handoff-emitting node becomes terminal in
  the same pass (FR-B-008). GIVEN two distinct `owner_key` values, WHEN each writes to the same
  `(namespace, key)`, THEN neither can read or overwrite the other's row (FR-A-006).

**US-4 — Security reviewer trusts untrusted content cannot silently drive control flow.**
As a security reviewer, I want any LLM-emitted `Command` and any store-derived prompt content to
pass through the same sanitizer checkpoints untrusted tool output already goes through, so that
this new control-flow surface does not bypass existing defenses.

- *Acceptance*: GIVEN a parsed `HandoffCommand`, WHEN it is about to drive routing or a store
  write, THEN it has passed `ExfiltrationGuard`/sanitizer validation first, and a rejection
  produces a loud `TaskOutcome::Failed`, never a silent `Completed` (FR-B-003, FR-B-009). GIVEN a
  `<shared-state>` prompt block built from store reads, WHEN it reaches a downstream node's
  prompt, THEN it is wrapped as untrusted/spotlighted content, not plain trusted text (FR-A-007).

---

## 3. Functional Requirements

### Group A — Cross-Thread Store (`crates/zeph-memory`)

| ID | Requirement | Priority |
|----|------------|----------|
| FR-A-001 | WHEN `[memory.store].enabled = false` (default) THE SYSTEM SHALL expose no store read/write path to orchestration or CLI — zero behavior change | must |
| FR-A-002 | WHEN `[memory.store].enabled = true` THE SYSTEM SHALL provide `store_put`/`store_get`/`store_delete`/`store_list`/`store_search` methods on `SqliteStore`/the Postgres-backed equivalent, each taking `owner_key: &str` as the first parameter | must |
| FR-A-003 | WHEN `store_put` is called with an `expected_version` THE SYSTEM SHALL perform a compare-then-write in one statement (`WHERE version = ?`, checking rows-affected) and return `MemoryError::VersionConflict` on mismatch, rather than silently overwriting | must |
| FR-A-004 | WHEN `store_put` is called without `expected_version` (or on first write) THE SYSTEM SHALL upsert the row, incrementing `version` and refreshing `updated_at` | must |
| FR-A-005 | WHEN a value exceeds `[memory.store].max_value_bytes` THE SYSTEM SHALL reject the write with a descriptive error rather than truncating or silently accepting it | must |
| FR-A-006 | WHEN any store method is called THE SYSTEM SHALL scope every read and write to the caller-supplied `owner_key`, such that no method can return or mutate a row belonging to a different `owner_key` | must |
| FR-A-007 | WHEN `store_list`/`store_search` results are assembled into a prompt-facing `<shared-state>` block (by zeph-core, §Group B) THE SYSTEM SHALL wrap that block as untrusted/spotlighted content per the existing `ContentTrustLevel` tiers (`sanitizer/lib.rs:49-55`) — this requirement is cross-referenced from FR-B-006, the store itself has no prompt-assembly responsibility | must |
| FR-A-008 | WHEN `zeph-db`'s migration-parity test runs THE SYSTEM SHALL find migration 110's SQLite and Postgres definitions of `cross_thread_store` structurally identical (same table/column/index set) | must |
| FR-A-009 | WHEN `--migrate-config` runs on a pre-080 config THE SYSTEM SHALL add a `[memory.store]` section with `enabled = false`, `max_value_bytes = 65536` | must |
| FR-A-010 | WHEN `--init` runs THE SYSTEM SHALL prompt for enabling the cross-thread store, defaulting to `No` | should |
| FR-A-011 | WHEN the `zeph store {get,put,list,delete}` CLI subcommand or the `/store` slash command is invoked THE SYSTEM SHALL require an explicit `owner_key` (or resolve it from the invoking channel's identity per §4 NFR-SEC-02) rather than defaulting silently to a shared bucket without the operator's awareness | should |

### Group B — Command Handoff (`crates/zeph-orchestration` + `crates/zeph-core`)

| ID | Requirement | Priority |
|----|------------|----------|
| FR-B-001 | WHEN `[orchestration.command].enabled = false` (default) THE SYSTEM SHALL never produce a `TaskOutcome::Handoff` — a trailing ` ```zeph-command ` block in a node's output, if present, is left as ordinary output text and the outcome is `Completed` as today | must |
| FR-B-002 | WHEN `[orchestration.command].enabled = true` AND a node's final output ends with a sole-trailing ` ```zeph-command\n{"goto": "<id\|title>", "update": {...}}\n``` ` block THE SYSTEM SHALL parse it into `HandoffCommand { goto: TaskRef, update: Vec<(String,String)> }` in zeph-core (`scheduler_loop.rs`), not in zeph-orchestration | must |
| FR-B-003 | WHEN a `HandoffCommand` is parsed THE SYSTEM SHALL route it (both the `goto` reference and every `update` key/value) through the sanitizer / `ExfiltrationGuard::validate_*` scan (reachable from zeph-core) before it drives any routing or store write | must |
| FR-B-004 | WHEN the sanitizer scan (FR-B-003) passes THE SYSTEM SHALL write each sanitized `update` key-value pair into the cross-thread store under namespace `orch/{graph_id}` and the dispatching context's `owner_key`, and THE SYSTEM SHALL complete this write before emitting the `TaskOutcome::Handoff` event (write-before-send is a hard ordering constraint — see §6 Never) | must |
| FR-B-005 | WHEN the store write (FR-B-004) succeeds THE SYSTEM SHALL emit `TaskOutcome::Handoff { output, goto }` — the `update` payload is not carried on the event, it is already persisted | must |
| FR-B-006 | WHEN zeph-orchestration's `handle_completed_outcome` receives a `Handoff` outcome THE SYSTEM SHALL store the node's `TaskResult`, mark the emitting node **terminal** (`Completed`), and call pure `dag::try_handoff(graph, goto)` — no store access occurs in zeph-orchestration | must |
| FR-B-007 | WHEN `dag::try_handoff` validates a `goto` target THE SYSTEM SHALL reject (and not activate) the target if: it is out of range, it is already `Completed` (forward-only), it is currently a live `route_to` reservation held by a non-terminal source (§6, mirrors F6), the per-graph handoff budget is exhausted, or (per FR-B-010) its `depends_on` are not fully satisfied — each rejection surfaces a loud, named error (`OrchestrationError::InvalidHandoffTarget` / `HandoffBudgetExhausted`) | must |
| FR-B-008 | WHEN `dag::try_handoff` validation passes THE SYSTEM SHALL activate the target (`Dormant`/`Pending → Ready`), set `commanded_from = Some(source)`, and decrement the per-graph `max_handoffs` budget | must |
| FR-B-009 | WHEN the produce-side parse detects a trailing block that is malformed, partial, or fails the sanitizer scan (FR-B-003) THE SYSTEM SHALL emit `TaskOutcome::Failed` with a descriptive error and a `tracing::warn!` — NEVER a silent fallback to `Completed`. Absence of any trailing block is not an error and produces ordinary `Completed` | must |
| FR-B-010 | WHEN `dag::try_handoff` validates a `goto` target's dependency state THE SYSTEM SHALL require the target's `depends_on` set to be either empty or fully `Completed` at validation time, mirroring `validate_route_to`'s existing empty-`depends_on` constraint (`dag.rs:209-214`) — a target with unsatisfied dependencies is rejected as `InvalidHandoffTarget`, never force-activated with a partial `<completed-dependencies>` context (resolves critic finding N1; decision (a) of the two offered in the design review) | must |
| FR-B-011 | WHEN any node dispatches (spawn or `RunInline`) under a graph with `[orchestration.command].enabled = true` THE SYSTEM SHALL have zeph-core append a `<shared-state>` block (from `store_list("orch/{graph_id}", owner_key, …)`), wrapped as untrusted/spotlighted content, to the prompt `router.rs::build_task_prompt` produced — `router.rs` itself remains store-free | must |
| FR-B-012 | WHEN `--migrate-config` runs on a pre-080 config THE SYSTEM SHALL add an `[orchestration.command]` section with `enabled = false`, `max_handoffs = 16` | must |
| FR-B-013 | WHEN `max_handoffs` is loaded from config THE SYSTEM SHALL reject a value of `0` at graph-plan-validation time (config-validated `> 0`), following the `default_idle_timeout_secs` precedent | must |
| FR-B-014 | WHEN `--init` runs THE SYSTEM SHALL prompt for enabling Command handoff, defaulting to `No` | should |

---

## 4. Non-Functional Requirements

**Security**

- **NFR-SEC-01 (bounded blast radius is the real mitigation).** The produce-side parse is
  deliberately *not* a registered tool — this removes the automatic confirmation/permission gate
  a tool call would get, it does not add one. The compensating controls, both mandatory, are: (a)
  the sanitizer/`ExfiltrationGuard` scan (FR-B-003) as an equivalent checkpoint, and (b) structural
  bounding — `goto` may only target a node already present in the plan (no arbitrary node
  creation, no code execution), and `update` may only write into the emitting task's own
  `orch/{graph_id}` namespace under the caller's `owner_key` (no cross-namespace or cross-owner
  write). This spec explicitly does NOT claim "not a tool" as a security property.
- **NFR-SEC-02 (tenancy).** Every store row is scoped by `owner_key`, enforced as a query filter
  on every read/write method (FR-A-006), not merely a caller convention — the isolation mechanism
  is *ready* (PK column + per-call filtering seam) for every channel. v1 default path uses
  `owner_key = "local"` for CLI/Telegram/gateway dispatch; the ACP dispatch path passes its real
  per-client `owner_key` (distinct from the existing `acp-local` unauthenticated-ACP sentinel —
  document, not a collision). This closes the class of bug fixed for `acp_sessions` in migration
  108 (issue #5868) one layer up, rather than reintroducing it. **Isolation is *active* only for
  ACP today.** CLI/TUI collapsing to `"local"` is correct (genuinely single-user). Gateway/A2A is
  a real, not hypothetical, deferred blind spot: both authenticate with a single shared bearer
  token across potentially many spoofable `sender`/caller identities (`handlers.rs` webhook
  `sender` field is unauthenticated free text), and all of them land in the same `"local"` bucket
  in v1 — meaning two distinct gateway callers can read/write each other's store rows. This is
  accepted for MVP specifically because closing it later is additive ("thread a real `owner_key`
  into the gateway/A2A dispatch site"), not a breaking migration — the schema and method
  signatures already take `owner_key` everywhere. See §10 OQ-1 for the tracked-follow-up
  recommendation.
- **NFR-SEC-03 (untrusted-content wrapping).** The `<shared-state>` prompt block is spotlighted
  per `ContentTrustLevel` (FR-A-007/FR-B-011) because its provenance may include a previously
  injected `Command.update` value. Spotlighting is not injection-resistance (`vigil.rs:10,22`) —
  it is this codebase's existing, accepted posture for all untrusted content, and this feature
  inherits it rather than introducing a new posture.
- **NFR-SEC-04 (Phase-2 boundary flag).** A future native `orch_handoff` LLM-facing tool
  (explicitly out of scope here) is structurally analogous to the LLM-initiated interrupt governed
  by issue #6234 / spec-073's INV-9 ("LLM may create but never resolve an interrupt"). Any future
  design for that tool MUST be reconciled with INV-9 in its own architecture pass; it does not
  inherit this spec's MVP boundary automatically.

**Portability (zeph-db dual-backend)**

- **NFR-POR-01.** Migration 110 ships both `crates/zeph-db/migrations/sqlite/110_cross_thread_store.sql`
  and `crates/zeph-db/migrations/postgres/110_cross_thread_store.sql`, structurally identical per
  `migration_parity.rs` (FR-A-008). `owner_key` is `NOT NULL DEFAULT 'local'` as part of the
  composite primary key `(owner_key, namespace, key)` — never nullable, since a NULL PK column is
  invalid on Postgres and all-distinct on SQLite (would silently break upsert).

**Performance / Await Discipline**

- **NFR-PERF-01.** The store `update` write and the sanitizer scan are `async` and execute inside
  the detached `send_event` async task the `on_done` spawn-path closure hands to
  `spawn_oneshot` (`scheduler_loop.rs:194-231`), or inline on the `RunInline` path — never
  synchronously inside the sync `on_done` closure body itself, and never via `block_on`.
- **NFR-PERF-02.** No lock guard is held across an `.await` on this feature's code paths (store
  write, sanitizer scan, `<shared-state>` read) — per the project's Await Discipline contract.
- **NFR-PERF-03.** The store write (FR-B-004) MUST complete before the `TaskOutcome::Handoff`
  event is sent — this is a correctness-load-bearing ordering constraint (§6 Never), not merely a
  performance concern: the goto target only becomes `Ready` after `handle_completed_outcome`
  processes that event, so write-before-send guarantees the target's `<shared-state>` read
  (FR-B-011) observes the update rather than stale/missing state.

**Concurrency / TaskSupervisor**

- **NFR-CONC-01.** No new `tokio::spawn()` call site is introduced. The store write/sanitizer scan
  reuse the existing `spawn_oneshot`-detached event-send task on the spawn path, or run inline on
  the `RunInline` path — both are pre-existing supervised/inline execution contexts.

**Auditability**

- **NFR-AUD-01.** Every store write (whether via Command `update` or the CLI/slash-command
  surface) and every `Handoff` routing decision (accepted or rejected, with the specific rejection
  reason) is logged via `tracing` and, where applicable, the existing tool/orchestration audit
  path, so a Command-driven graph run is reconstructable after the fact.

---

## 5. Architecture / Data Model

### 5.1 Layering boundary (binding)

```
zeph-orchestration           zeph-core                    zeph-memory
──────────────────           ─────────                    ───────────
dag::try_handoff (pure)  ◄── TaskOutcome::Handoff{output,goto}
handle_completed_outcome     produce-side parse
  marks source Completed     sanitizer scan (F3a)
  activates target           store write (update, F1)  ──► cross_thread_store
router::build_task_prompt    <shared-state> read (F1)  ◄── (owner_key-scoped)
  (unchanged, store-free)      + untrusted wrap (F3b)
```

`zeph-orchestration` keeps `zeph-memory` as a **dev-dependency only** (`Cargo.toml:39-46`,
confirmed unchanged by this spec). All store I/O — both the `update` write and the
`<shared-state>` read — lives in `zeph-core`, where the `Arc<SemanticMemory>` handle already
exists (`state/persistence.rs:23`). `try_handoff` and `build_task_prompt` stay pure/store-free.

### 5.2 Execution flow

1. **[core]** Node sub-agent finishes. `scheduler_loop.rs` parses the final output for a
   sole-trailing ` ```zeph-command ` block. No block → ordinary `Completed`. A detected-but-
   malformed block → `TaskOutcome::Failed` (FR-B-009).
2. **[core]** The parsed `HandoffCommand` (goto + every update key/value) is scanned via the
   sanitizer/`ExfiltrationGuard` path (FR-B-003). A rejection → `TaskOutcome::Failed`.
3. **[core]** Each sanitized `update` KV is written into `cross_thread_store` under
   `owner_key`/`namespace = orch/{graph_id}` (FR-B-004). This write completes before step 4
   (NFR-PERF-03).
4. **[core→orch]** `TaskOutcome::Handoff { output, goto }` is emitted over the existing
   completion-event channel.
5. **[orch]** `handle_completed_outcome` stores the `TaskResult`, marks the emitting node
   `Completed` (terminal — the livelock-guard invariant, FR-B-006/§6), then calls
   `dag::try_handoff(graph, goto)`.
6. **[orch]** `try_handoff` validates (in-range, not-`Completed`, not a live `route_to`
   reservation, `depends_on` satisfied per FR-B-010, budget not exhausted) and, on success,
   activates the target `Dormant`/`Pending → Ready`, sets `commanded_from`, decrements the
   budget. Any validation failure is a named, logged error (FR-B-007).
7. **[core]** At dispatch of any node in a Command-enabled graph, zeph-core reads
   `store_list("orch/{graph_id}", owner_key, …)` and appends a `<shared-state>` block, wrapped as
   untrusted/spotlighted content, to the prompt `router.rs::build_task_prompt` produced
   (FR-B-011).

### 5.3 Store schema (migration 110)

```sql
CREATE TABLE cross_thread_store (
    owner_key   TEXT    NOT NULL DEFAULT 'local',
    namespace   TEXT    NOT NULL,
    key         TEXT    NOT NULL,
    value       TEXT    NOT NULL,                          -- JSON payload
    version     INTEGER NOT NULL DEFAULT 1,
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, -- TIMESTAMPTZ on Postgres
    updated_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (owner_key, namespace, key)
);
CREATE INDEX idx_cross_thread_store_owner_ns ON cross_thread_store(owner_key, namespace);
```

- `namespace` is a hierarchical string convention (`orch/{graph_id}`, reserved for future
  non-orchestration callers e.g. `user/{conversation_id}/prefs`). Prefix scans via
  `namespace LIKE ?||'%'` power `list`/`search`.
- `put` upserts (`ON CONFLICT(owner_key,namespace,key) DO UPDATE`), bumping `version` and
  refreshing `updated_at`. `expected_version`, if supplied, gates the update via
  `WHERE version = ?` + rows-affected check → `VersionConflict` on mismatch (FR-A-003).
- MVP `search` = namespace-prefix + `value LIKE`/keyword match; no embedding index (§1 Out of
  Scope). Follows `preferences.rs`'s `Dialect::select_as_text`/`rewrite_placeholders`/`sql!`
  idioms and existing key/value byte-truncation conventions.

### 5.4 Key types

| Type | Location | Purpose |
|------|----------|---------|
| `StoreItem` | `zeph-memory/src/store/cross_thread.rs` (new) | `{ owner_key, namespace, key, value: String(JSON), version, created_at, updated_at }` |
| `CrossThreadStore` methods | impl on `SqliteStore`/Postgres equivalent, same file | `store_put(owner_key, ns, key, value, expected_version: Option<i64>)`, `store_get`, `store_delete`, `store_list(owner_key, ns_prefix, limit)`, `store_search(owner_key, ns_prefix, query, limit)` |
| `MemoryError::VersionConflict` | `zeph-memory/src/error.rs` | thiserror variant for optimistic-concurrency failure |
| `HandoffCommand` | `zeph-orchestration/src/command.rs` (extend) | `{ goto: TaskRef, update: Vec<(String,String)> }`; `TaskRef = ById(TaskId) | ByTitle(String)` — parsed and consumed entirely in zeph-core; only `goto` crosses into orchestration |
| `TaskOutcome::Handoff` | `zeph-orchestration/src/scheduler/mod.rs` (new non_exhaustive variant) | `{ output: String, goto: TaskRef }` — no `update` field; the payload is pre-persisted (§5.2 step 3-4) |
| `TaskNode.commanded_from: Option<TaskId>` | `zeph-orchestration/src/graph.rs` (new field, mirrors `routed_from`) | `#[serde(default, skip_serializing_if = "Option::is_none")]` |
| `dag::try_handoff` | `zeph-orchestration/src/dag.rs` (new fn, sibling of `try_reroute`) | Pure validation + activation (FR-B-007/FR-B-008) |
| `OrchestrationError::HandoffBudgetExhausted` / `InvalidHandoffTarget` | `zeph-orchestration/src/error.rs` | thiserror variants for the livelock guard and bad-goto rejections |

### 5.5 Config

```toml
[memory.store]
enabled = false
max_value_bytes = 65536
# search_provider = "fast"   # reserved for future semantic search (declare-once name)

[orchestration.command]
enabled = false        # opt-in: lets node agents dynamically reroute
max_handoffs = 16       # per-graph livelock budget, validated > 0
```

---

## 6. Key Invariants

### Always (without asking)

- **All cross-thread-store I/O — both the `update` write and the `<shared-state>` read — happens
  in `zeph-core`.** `zeph-orchestration` never gains a production dependency on `zeph-memory`;
  `try_handoff` and `build_task_prompt` stay pure/store-free (§5.1). This is the single relocation
  invariant that resolves the design review's F1 finding.
- **The store `update` write completes before the `TaskOutcome::Handoff` event is sent**
  (NFR-PERF-03). This ordering is load-bearing for `<shared-state>` read correctness, not an
  optimization detail — never reorder it for latency.
- **A node that emits `Handoff` becomes terminal (`Completed`) in the same
  `handle_completed_outcome` pass that processes the event.** This is what makes forward-only
  (`goto` must not target `Completed`) a sound livelock guard: each hop consumes exactly one
  not-yet-terminal node, so A↔B ping-pong is structurally impossible and `max_handoffs` is a true
  backstop, not the primary bound.
- **`goto` targets must have satisfied dependencies** — empty `depends_on` or all dependencies
  already `Completed` — mirroring `validate_route_to`'s existing constraint (`dag.rs:209-214`).
  A target with unsatisfied dependencies is rejected, never force-activated with a partial
  `<completed-dependencies>` context (FR-B-010).
- **Every store row is scoped by `owner_key`** on every read and write method — no method call can
  cross an `owner_key` boundary (FR-A-006, NFR-SEC-02).
- **The parsed `HandoffCommand` is sanitizer/`ExfiltrationGuard`-scanned before it drives any
  routing or store write** (FR-B-003). A scan rejection is a loud `TaskOutcome::Failed`, never a
  silent `Completed`.
- **A malformed, partial, or scan-rejected `zeph-command` block produces `TaskOutcome::Failed`**,
  never a silent fallback to ordinary `Completed` (FR-B-009). This discards the node's otherwise-
  good output — an accepted MVP tradeoff (the node did not fulfill its declared contract), not an
  oversight; note this in the playbook so it is not mistaken for a bug during live testing.
- **`try_handoff` rejects a `goto` targeting a live `route_to` reservation** held by a non-terminal
  source node — preserving the plan-time invariant `validate_route_to` cannot defend at runtime
  (mirrors design-review finding F6). Reject, never silently disable the fallback.
- **`<shared-state>` prompt blocks are wrapped as untrusted/spotlighted content** per
  `ContentTrustLevel` (FR-A-007/FR-B-011) — their provenance may include previously injected
  `Command.update` values.
- **Both primitives default to disabled** (`[memory.store].enabled = false`,
  `[orchestration.command].enabled = false`) and produce zero behavior change when disabled
  (FR-A-001, FR-B-001).
- **`route_to`'s own code path, validation, and PR #6346 hardening remain untouched.** `Command`
  shares only the low-level Dormant→Ready activation machinery; it is a parallel mechanism at the
  policy/trigger layer, not a subsuming one.

### Ask First

- Whether `TaskRef::ByTitle` resolution (name-based `goto` target lookup, vs. `ById` only) ships
  in v1 or is deferred — the design review did not pin this down explicitly; if title-based lookup
  proves ambiguous (duplicate titles) during implementation, restricting v1 to `ById` only is an
  acceptable narrowing but should be confirmed with the lead before silently dropping
  `ByTitle` from the parser.
- Whether the CLI `zeph store` subcommand's `owner_key` resolution for non-ACP channels needs a
  dedicated flag or can default silently to `"local"` — v1 assumption is silent default is
  acceptable for CLI/Telegram/gateway (§7 OQ-1), but this is a UX call, not purely technical.
- Extending `update`'s namespace beyond the emitting task's own `orch/{graph_id}` bucket (letting
  a Command specify an arbitrary namespace) — explicitly deferred as a security-surface expansion
  (§7 OQ-2); requires an explicit architectural decision if ever revisited.

### Never

- **NEVER** let `zeph-orchestration` take a production dependency on `zeph-memory`. If a future
  change seems to require this, it is a violation of this spec's central invariant and needs a new
  architecture pass, not a quiet `Cargo.toml` edit.
- **NEVER** allow `Command.goto` to target a `Completed` node (no backward/looping routes) in this
  MVP — true cycles are deferred to Phase 2 behind a visit-counter design; forward-only is what
  makes the livelock guard sound (see Always, above).
- **NEVER** make `owner_key` nullable on `cross_thread_store` — NULL in a composite primary key is
  invalid on Postgres and breaks upsert semantics on SQLite; the column is `NOT NULL DEFAULT
  'local'`.
- **NEVER** silently fall back to `Completed` when a `zeph-command` block is detected but
  malformed, partial, or sanitizer-rejected — always `TaskOutcome::Failed` (FR-B-009).
- **NEVER** reorder the store-write-before-`Handoff`-emit sequence for latency or refactoring
  convenience (NFR-PERF-03) — this ordering is a correctness guarantee, not incidental.
- **NEVER** introduce a new `tokio::spawn()` call site for this feature — reuse the existing
  `spawn_oneshot`-detached path or the inline `RunInline` `async fn`, per
  `[[039-background-task-supervisor/spec]]`'s binding project-wide constraint.
- **NEVER** treat "not a registered tool" as a security mitigation in documentation, code comments,
  or review discussion — the real mitigation is the sanitizer scan plus bounded blast radius
  (NFR-SEC-01). This framing must not regress into an implicit assumption during implementation.
- **NEVER** hardcode migration file number `110` or migrate-step numbers as final without
  re-verifying against HEAD immediately before implementation — this repo has repeatedly hit
  migration-step-number collisions with concurrently-merged PRs (e.g. #6343/#6342 collided on step
  89/90); re-run the relevant count/number checks at implementation start, not from this spec's
  numbers.
- **NEVER** implement a Phase-2 native `orch_handoff` tool as a drop-in extension of this MVP
  without first reconciling it against issue #6234 / spec-073 INV-9 (NFR-SEC-04) — it is not a
  free follow-up PR.

---

## 7. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| `[orchestration.command].enabled = false`, node output happens to end with a `zeph-command`-shaped block | Ignored as ordinary text; `TaskOutcome::Completed` as today (FR-B-001) |
| Node emits a well-formed `zeph-command` block that is NOT the sole trailing content (extra text after it) | Treated as malformed → `TaskOutcome::Failed` (FR-B-009) |
| `goto` references a `TaskId`/title that does not exist in the graph | `InvalidHandoffTarget`, loud failure (FR-B-007) |
| `goto` targets a node already `Completed` | Rejected — forward-only invariant (FR-B-007, §6 Always) |
| `goto` targets a node whose `depends_on` are not all `Completed` | Rejected — FR-B-010 / N1 resolution |
| `goto` targets a node currently reserved as another (non-terminal) node's `route_to` fallback | Rejected — preserves the plan-time `validate_route_to` invariant (§6 Always) |
| `goto` targets a node whose `route_to`-reserving source has already completed (reservation is dead) | Allowed — the reservation no longer blocks anything (mirrors design-review F6 scoping to non-terminal sources) |
| Source task X fails, its static `recovery.route_to = Y` fallback fires and activates Y (`Dormant → Ready`); a *later*, unrelated Command emits `goto = Y` | **Accepted, understood, harmless edge case — not a bug.** By the time the Command evaluates, Y is `Ready`, not `Dormant`, so it no longer matches "a live `route_to` reservation" (that check is Dormant-scoped, §6 Always / F6) and it is not `Completed` (forward-only check also passes). `try_handoff` therefore activates Y a second time: `commanded_from` is set alongside its earlier `route_to`-driven activation, and one `max_handoffs` budget unit is consumed. Y itself still executes exactly once — a node already `Ready`/dispatched is not re-run or duplicated by a redundant activation call. Net effect: a spurious `commanded_from` marker and one wasted budget unit, nothing else. Documented here specifically so it is not later mistaken for a bug during implementation or review |
| `max_handoffs` budget exhausted mid-graph | `HandoffBudgetExhausted`, graph-level loud failure |
| `update` value exceeds `max_value_bytes` | Write rejected with descriptive error (FR-A-005); whether this alone causes the whole `HandoffCommand` to be treated as malformed (FR-B-009) or only that key's write to fail is an implementation detail to confirm during Phase 2, defaulting to treating it as a scan/write failure → `TaskOutcome::Failed` for consistency with FR-B-009's "never silent degrade" posture |
| Two concurrent writers `store_put` the same `(owner_key, namespace, key)` with stale `expected_version` | Second writer receives `VersionConflict` (FR-A-003); caller (Command produce-side or CLI) surfaces this as a failure, does not silently retry-and-clobber |
| ACP client A attempts to read/write a store row owned by ACP client B's `owner_key` | Impossible by construction — every method filters on `owner_key` (FR-A-006, NFR-SEC-02) |
| `<shared-state>` block is empty (no prior writes in this graph's namespace) | Block omitted or renders as empty — no error, no placeholder confusion with `<completed-dependencies>` |
| A graph runs with `[memory.store].enabled = true` but `[orchestration.command].enabled = false` | Store is usable via CLI/slash-command surfaces; no Command handoff occurs; the two config flags are independent (FR-A-001/FR-B-001 are separately gated) |
| A graph runs with `[orchestration.command].enabled = true` but `[memory.store].enabled = false` | Command handoff produce-side parse still runs, but the `update` write (FR-B-004) has nowhere to persist — treat as a configuration error at graph-plan-validation time (`orchestration.command.enabled` requires `memory.store.enabled`), rejecting the graph plan rather than silently dropping updates at runtime |
| Malformed `zeph-command` block on a node whose actual text output was otherwise valid and useful | Output is discarded, node is `Failed`, existing failure recovery (`route_to`/`Retry`/`Ask`) engages as it would for any other failure — accepted MVP tradeoff (§6 Always, F5/N4) |

---

## 8. Success Criteria

- [ ] Default-disabled regression: with both config flags at their default `false`, a graph
      containing a node whose output happens to end with a `zeph-command`-shaped block behaves
      byte-for-byte identically to pre-feature `Completed` handling
- [ ] Store CRUD round-trip test: `put`/`get`/`list`/`delete`/`search`, including
      `expected_version` conflict detection (FR-A-003)
- [ ] `owner_key` isolation test: two distinct `owner_key`s cannot read or overwrite each other's
      rows under the same `(namespace, key)` (FR-A-006)
- [ ] `migration_parity.rs` passes for migration 110 across SQLite and Postgres (FR-A-008)
- [ ] Handoff end-to-end test: a node emits a valid Command, its `update` is persisted, the goto
      target activates and its subsequent `<shared-state>` prompt block contains the update
      (FR-B-002..006, FR-B-011)
- [ ] Livelock test: an adversarially-constructed graph attempting A↔B ping-pong terminates
      because the emitting node becomes terminal each hop (§6 Always) — never exhausts
      `max_handoffs` via ping-pong, and separately, a forward-fan-out-only construction that
      would exceed `max_handoffs` is stopped by the budget
- [ ] Forward-only rejection test: `goto` targeting a `Completed` node is rejected
      (`InvalidHandoffTarget`)
- [ ] N1/FR-B-010 test: `goto` targeting a `Pending` node with unsatisfied `depends_on` is
      rejected; `goto` targeting a node with empty or fully-satisfied `depends_on` succeeds
- [ ] route_to-reservation contention test: `goto` targeting a live `route_to` reservation from a
      non-terminal source is rejected; targeting one whose source already completed succeeds
      (§6 Always / F6 mirror)
- [ ] Redundant-activation edge case test (§7): a node Y activated via a static `route_to`
      fallback, then later targeted by an unrelated `goto = Y`, is accepted (Y is `Ready`, not
      `Dormant`/`Completed`), consumes one `max_handoffs` unit, and executes exactly once — not
      duplicated or corrupted by the second activation
- [ ] Malformed-block test: a detected-but-malformed or sanitizer-rejected `zeph-command` block
      produces `TaskOutcome::Failed`, never `Completed` (FR-B-009)
- [ ] Sanitizer-scan test: a `HandoffCommand` crafted to fail `ExfiltrationGuard` validation is
      rejected before any store write or routing occurs (FR-B-003)
- [ ] `<shared-state>` trust-wrapping test: the prompt block is assembled with the
      untrusted/spotlighted wrapper, verifiable by inspecting the built prompt (FR-A-007/FR-B-011)
- [ ] Write-before-send ordering test: a race-oriented test (or code-review-verifiable structural
      guarantee) confirms the store write completes before the `Handoff` event send
      (NFR-PERF-03)
- [ ] `--migrate-config` idempotency test for both new config sections (FR-A-009, FR-B-012)
- [ ] `--init` wizard prompts verified live for both new sections (FR-A-010, FR-B-014)
- [ ] Zero new `tokio::spawn()` call sites: async-supervision scan count non-increasing per
      `.claude/rules/continuous-improvement.md`
- [ ] `cargo +nightly fmt --check`, `cargo clippy --profile ci ... -D warnings`,
      `cargo nextest run ...`, and the rustdoc gate all pass per `.claude/rules/branching.md`
- [ ] `.local/testing/playbooks/cross-thread-store-handoff.md` created (main-repo path)
- [ ] `.local/testing/coverage-status.md` rows added (main-repo path, status `Untested`)
- [ ] Migration file number `110` and migrate-step numbers re-verified against HEAD immediately
      before implementation (§6 Never)

---

## 9. Agent Boundaries

### Always (without asking)
- Keep all cross-thread-store I/O in `zeph-core`; never add a production `zeph-memory` dependency
  to `zeph-orchestration`.
- Route every parsed `HandoffCommand` through the sanitizer/`ExfiltrationGuard` scan before it
  drives routing or a store write.
- Enforce `owner_key` scoping on every store method — no unscoped read/write path.
- Fail loudly (`TaskOutcome::Failed`) on malformed/partial/rejected Command blocks — never
  silently degrade to `Completed`.
- Add both new config sections to `--migrate-config` and `--init` per `CLAUDE.md` Development
  Rules (mandatory integration points).
- Write/update `.local/testing/playbooks/cross-thread-store-handoff.md` and
  `.local/testing/coverage-status.md` before opening the PR (mandatory per `CLAUDE.md`).

### Ask First
- Whether `TaskRef::ByTitle` ships in v1 vs. `ById`-only (§6 Ask First).
- Any change to `route_to`'s own validation, activation code, or PR #6346 hardening — Command
  must remain additive alongside it, not a modification of it.
- Extending `update`'s writable namespace beyond the emitting task's own `orch/{graph_id}` bucket.
- Any new `MessagePart`/prompt-construction contract change beyond appending the `<shared-state>`
  block described here.
- Building the deferred native `orch_handoff` tool — requires its own architecture pass reconciled
  against #6234/spec-073 INV-9 (NFR-SEC-04) before any implementation starts.

### Never
- Never let `Command.goto` target a `Completed` node (forward-only is load-bearing for the
  livelock guard).
- Never make `owner_key` nullable in the `cross_thread_store` schema.
- Never reorder the store-write-before-`Handoff`-emit sequence.
- Never introduce a new `tokio::spawn()` call site for this feature.
- Never treat "not a registered tool" as a security mitigation in code comments, docs, or design
  discussion for this feature.
- Never ship this feature with either config flag defaulting to `true`.
- Never skip the sanitizer scan on a parsed `HandoffCommand`, even for a "trusted" or internal
  graph — there is no trust-tier exemption defined for this path.

---

## 10. Open Questions

Only two items remain, both explicitly deferred (not blocking implementation):

| ID | Question | Status |
|----|----------|--------|
| OQ-1 | **`owner_key` source at dispatch for non-ACP channels.** CLI/Telegram collapsing to the `"local"` bucket is correct (single-user). Gateway/A2A collapsing to the same `"local"` bucket is a real, deferred tenancy blind spot (NFR-SEC-02): both share one bearer token across multiple spoofable caller identities, all landing in one bucket, so two distinct gateway/A2A callers can read/write each other's store rows in v1. | Deferred with a default, not silently: v1 ships `"local"` for all non-ACP channels; ACP passes its real per-client key. Accepted for MVP because the fix is additive (thread a real `owner_key` into the gateway/A2A dispatch site — the schema/method signatures already take `owner_key` everywhere, no breaking migration needed). Tracked as GitHub **#6389**, filed per this row's original instruction so the blind spot isn't silently forgotten once the MVP is merged |
| OQ-2 | **`update` namespace generality.** Should a future revision let a `Command` write outside its own `orch/{graph_id}` bucket (e.g., to a user-scoped namespace)? | Deferred with a default: MVP hard-codes `orch/{graph_id}` as the only writable namespace for Command `update`; any generalization is a security-surface expansion requiring an explicit Ask-First architecture decision (§6 Ask First), not a mechanical follow-up |

All other open questions raised across the two-round design review (store-handle reachability,
forward-only-vs-loops parity stance, produce-side robustness, tenancy, the F1-F7 finding set, and
N1) are resolved into the functional requirements and invariants above — see §11 for the
traceability mapping.

---

## 11. Affected Subsystems

| Crate | Change level | What changes |
|-------|-------------|--------------|
| `zeph-memory` | Medium | New `store/cross_thread.rs` (`StoreItem`, `CrossThreadStore` methods); `MemoryError::VersionConflict` |
| `zeph-db` | Small | Migration 110 (both dialects); `migration_parity.rs` coverage |
| `zeph-orchestration` | Medium | `HandoffCommand`/`TaskRef` types in `command.rs`; `TaskOutcome::Handoff` variant; `TaskNode.commanded_from`; `dag::try_handoff` + validation; `OrchestrationError` variants — no new `zeph-memory` dependency |
| `zeph-core` | Medium-Large | Produce-side parse, sanitizer scan, store write, and `<shared-state>` read all live here (`scheduler_loop.rs`); this is the security-critical integration seam |
| `zeph-config` | Small | `[memory.store]` (`zeph-config/src/memory/`), `[orchestration.command]` (`zeph-config/src/experiment.rs` or sibling); two `--migrate-config` steps |
| `zeph-commands` / `src/` (binary) | Small | `zeph store {get,put,list,delete}` CLI subcommand; `/store` slash command; `--init` wizard prompts for both new sections |
| `zeph-sanitizer` | None (reuse) | No new sanitizer code — `HandoffCommand` and `<shared-state>` reuse existing `ExfiltrationGuard`/`ContentTrustLevel` machinery |

---

## 12. See Also

- [[MOC-specs]] — Map of all specifications
- [[constitution]] — Project-wide non-negotiable principles
- [[001-system-invariants/spec]] — Cross-cutting architectural invariants (Ask-First items this
  spec's own §9 extends)
- [[004-memory/spec]] — `zeph-memory` crate this spec's Store primitive extends
- [[009-orchestration/spec]] — DAG planner, `DagScheduler`, `TaskGraph` this spec's Command
  primitive extends; `route_to` (Mode 2) precedent
- [[010-security/spec]] — Sanitizer/`ExfiltrationGuard`/injection-defense contract this spec
  reuses rather than replaces
- [[031-database-abstraction/spec]] — `zeph-db` dual-backend model, `migration_parity.rs` gate
- [[039-background-task-supervisor/spec]] — Binding no-new-`tokio::spawn` constraint (NFR-CONC-01)
- [[040-sanitizer/spec]] — `ContentSanitizer`/`ContentTrustLevel`/quarantine flow this spec's
  `<shared-state>` wrapping and `HandoffCommand` scan both integrate with
- [[075-orchestration-node-control-parity/spec]] — `route_to`/`RecoveryAction` Mode 2 precedent
  this spec's `goto`-vs-`depends_on` validation (FR-B-010) and Dormant-contention handling (§6)
  both mirror
- GitHub issue #6363 — source issue
- `.local/handoff/2026-07-17T05-20-29-architect.md` — initial design
- `.local/handoff/2026-07-17T05-27-21-critic.md` — first critique (verdict: significant, F1-F7)
- `.local/handoff/2026-07-17T05-35-30-architect.md` — revision addressing F1-F7
- `.local/handoff/2026-07-17T05-37-33-critic.md` — final critique (verdict: approved/minor, N1-N4)
