---
aliases:
  - MARCH Self-Check
  - Proposer-Checker Pipeline
  - Quality Layer
tags:
  - sdd
  - spec
  - quality
  - self-check
created: 2026-04-19
status: approved
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[002-agent-loop/spec]]"
  - "[[003-llm-providers/spec]]"
  - "[[024-multi-model-design/spec]]"
---

# Spec: MARCH Proposer+Checker Self-Check Pipeline

> [!info]
> Post-response factual consistency layer. After each LLM response the Proposer
> sub-agent decomposes the output into atomic verifiable assertions; the Checker
> sub-agent validates each against retrieved context only — without seeing the
> original response — breaking confirmation bias. Resolves issues #2352 and #2285.
> Landed in #3226.

## Sources

### External
- **MARCH: Multi-Agent Reinforced Claim Hallucination reduction** (research memo, 2026)
- **Information-asymmetry self-check** principle: Checker never receives the original
  response, only retrieved context, preventing the model from simply agreeing with itself

### Internal

| File | Contents |
|---|---|
| `crates/zeph-core/src/quality/mod.rs` | Module root, feature gate |
| `crates/zeph-core/src/quality/pipeline.rs` | `MarchPipeline` orchestrator |
| `crates/zeph-core/src/quality/proposer.rs` | Proposer sub-agent |
| `crates/zeph-core/src/quality/checker.rs` | Checker sub-agent |
| `crates/zeph-core/src/quality/types.rs` | `Assertion`, `CheckResult`, `MarchVerdict` |
| `crates/zeph-core/src/quality/parser.rs` | JSON assertion parser with retry |
| `crates/zeph-core/src/quality/prompts.rs` | Proposer and Checker prompt templates |
| `crates/zeph-core/src/quality/config.rs` | `QualityConfig` deserialized from TOML |
| `crates/zeph-core/src/agent/quality_hook.rs` | Turn-level hook wired into agent loop |

---

## 1. Overview

### Problem Statement

LLM responses can contain factual inconsistencies with information retrieved from
memory, graph, or code context. Without a verification pass, hallucinated claims
are indistinguishable from grounded ones at the channel output layer. Standard
self-verification fails because the model has already seen its own response and
tends to agree with it (confirmation bias).

### Goal

Add an optional, feature-gated post-response quality layer that:

1. Decomposes each response into atomic verifiable assertions (Proposer).
2. Validates each assertion independently against retrieved context only
   (Checker, with confirmation-bias prevention).
3. Surfaces a per-turn verdict for operator inspection and optional user feedback.

The layer is **non-blocking** by default — a failed or timed-out check does not
suppress the response. It is an observability and optional annotation layer, not
a response gate in the MVP.

### Out of Scope

- Blocking or replacing the response on check failure (gate mode is post-MVP)
- Multilingual assertion decomposition (English-only prompts in MVP)
- Self-learning feedback loop from check verdicts (separate spec)
- Modifying the agent loop's context assembly (Checker uses already-assembled context)

---

## 2. User Stories

### US-001: Factual consistency visibility
AS AN operator running a production session
I WANT post-response factual checks to run automatically
SO THAT I can detect responses that conflict with retrieved memory or documents

**Acceptance criteria:**
```
GIVEN self_check = true and the agent produces a response asserting a fact
WHEN the fact contradicts a graph edge or memory excerpt in the retrieved context
THEN a WARN log entry is emitted with the failing assertion and context source
AND the MarchVerdict is available in the turn debug dump
AND the response is delivered to the user unchanged
```

### US-002: Checker asymmetry guarantee
AS A developer maintaining the quality pipeline
I WANT the Checker to never receive the original response
SO THAT confirmation bias is prevented at compile time

**Acceptance criteria:**
```
GIVEN the Checker function signature
WHEN reviewing run_checker's parameter list
THEN there is no `response: &str` parameter
AND the Checker context contains only retrieved_context and the assertion text
```

### US-003: Non-blocking degradation
AS A user on a high-latency connection
I WANT MARCH checks to fail gracefully under timeout
SO THAT my response is never delayed by the quality layer

**Acceptance criteria:**
```
GIVEN proposer_timeout_ms = 2000
  AND the Proposer provider exceeds that timeout
WHEN the quality hook runs
THEN the response is delivered with verdict = Skipped(timeout)
AND no error is surfaced to the user
AND quality.march_timeouts_total increments
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN `[quality].self_check = true` AND a response is produced THE SYSTEM SHALL invoke `MarchPipeline::run()` before the turn completes | must |
| FR-002 | THE SYSTEM SHALL decompose the response into atomic assertions via the Proposer; each assertion is a single verifiable claim in natural language | must |
| FR-003 | THE SYSTEM SHALL validate each assertion via the Checker using only `retrieved_context` — the original response MUST NOT be passed to the Checker | must |
| FR-004 | WHEN the Proposer or Checker times out THE SYSTEM SHALL return `MarchVerdict::Skipped` and deliver the response unchanged | must |
| FR-005 | WHEN any assertion fails validation THE SYSTEM SHALL log at `WARN` level with the assertion text and conflicting context snippet | must |
| FR-006 | THE SYSTEM SHALL expose `MarchVerdict` in the debug dump for the current turn | should |
| FR-007 | WHEN `[quality].self_check = false` (default) THE SYSTEM SHALL skip the quality hook entirely — zero overhead | must |
| FR-008 | THE SYSTEM SHALL support `async_run = true` config field for schema stability; the current implementation logs a `WARN` noting async mode is unimplemented | should |
| FR-009 | WHEN the Checker provider is Claude-backed THE SYSTEM SHALL disable `cache_control` markers on Checker calls via `AnyProvider::with_prompt_cache_disabled()` to prevent context leakage | must |
| FR-010 | The JSON assertion parser SHALL retry once on malformed output before returning `ParseError`; truncation of oversized outputs SHALL use `floor_char_boundary(4096)` (UTF-8 safe) | must |
| FR-011 | WHEN `[quality].self_check` is added to config THE SYSTEM SHALL include it in `--migrate-config` step 22 and `--init` step_quality | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Proposer and Checker calls each respect their configured timeout (default 2000 ms each); total quality check overhead SHALL not exceed 5 s before the response is delivered |
| NFR-002 | Performance | When `self_check = false` the hook invocation cost SHALL be zero (no allocations, no provider lookup) |
| NFR-003 | Reliability | Proposer or Checker failure SHALL not propagate to the caller — always `Ok(MarchVerdict::Skipped)` on error |
| NFR-004 | Reliability | `async_run = true` is accepted by the parser and stored in config without panic; behavior is degraded-graceful (WARN, no runtime error) |
| NFR-005 | Reliability | The pipeline does not modify `Agent` message state — it is a read-only observer of the assembled context |
| NFR-006 | Security | Retrieved context passed to Checker is the same context already assembled for the LLM — no additional data access or privilege escalation |
| NFR-007 | Security | Assertion texts are never logged at ERROR or WARN with full user message content; only the assertion claim and a context snippet are logged |
| NFR-008 | Observability | Prometheus counters SHALL be exported: `quality_march_runs_total`, `quality_march_assertions_total`, `quality_march_failures_total`, `quality_march_timeouts_total` |
| NFR-009 | Maintainability | Proposer and Checker prompt templates are in `prompts.rs` as static strings; modifying them does not require changing pipeline logic |
| NFR-010 | Maintainability | Adding a new verification strategy requires only a new module under `quality/` and a config enum variant — no changes to the agent loop |

---

## 5. Architecture

### Pipeline Flow

```
process_user_message_inner()
    │
    ├── LLM response produced
    │
    └── quality_hook::run_if_enabled(response, retrieved_context, config)
            │
            ├── MarchPipeline::run(response, retrieved_context)
            │       │
            │       ├── Proposer::decompose(response) → Vec<Assertion>
            │       │
            │       └── for each Assertion:
            │               Checker::validate(assertion, retrieved_context)
            │               (response NOT passed to Checker)
            │
            └── MarchVerdict → debug_dump, metrics, optional WARN log
```

### Checker Asymmetry (Compile-Time Invariant)

```rust
// run_checker has no `response` parameter — enforced at the type level.
async fn run_checker(
    assertion: &Assertion,
    retrieved_context: &RetrievedContext,
    provider: &AnyProvider,
    timeout: Duration,
) -> Result<CheckResult, CheckerError>
```

This signature is the primary correctness invariant. Any change that adds a
`response` parameter requires explicit architectural review.

### Config

```toml
[quality]
self_check = false           # opt-in; included in `full` feature bundle

proposer_provider = ""       # references [[llm.providers]]; empty = default provider
checker_provider = ""        # references [[llm.providers]]; empty = default provider
proposer_timeout_ms = 2000
checker_timeout_ms = 2000
max_assertions_per_response = 10  # cap to bound cost on long responses
async_run = false            # reserved; current impl is synchronous
```

### Types

```rust
pub struct Assertion {
    pub claim: String,
    pub confidence: f32,     // Proposer-assigned; 0.0–1.0
}

pub enum CheckResult {
    Supported,
    Contradicted { snippet: String },
    Inconclusive,
}

pub enum MarchVerdict {
    AllSupported,
    PartialFailure { failures: Vec<(Assertion, CheckResult)> },
    Skipped(SkipReason),
}

pub enum SkipReason {
    Disabled,
    Timeout,
    ParseError,
    ProviderError,
}
```

---

## 6. Key Invariants

### Always (without asking)
- `self_check = false` is the default; the feature must be explicitly opted into
- The Checker NEVER receives the original response text — only the assertion and retrieved context
- Quality hook failure never blocks or delays the response to the user
- Proposer and Checker use separate provider instances (allows different models)
- Claude-backed Checker disables `cache_control` via `with_prompt_cache_disabled()`
- JSON parser retries exactly once before returning `ParseError`
- Truncation uses `floor_char_boundary(4096)` — never truncates at a non-char boundary
- All MARCH paths are gated behind the `self-check` compile-time feature flag

### Ask First
- Enabling gate mode (blocking response delivery on check failure)
- Raising `max_assertions_per_response` above 20 (cost and latency impact)
- Adding the Checker to the hot path before response delivery
- Changing the Checker signature to accept the original response

### Never
- Pass the original response to the Checker sub-agent
- Block, panic, or return `Err` to the agent loop on any quality check failure
- Log full user message content at WARN or above in quality check paths
- Run the pipeline when `self_check = false` (zero-overhead contract)

---

## 7. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Proposer times out | Return `Skipped(Timeout)`; response delivered unchanged; timeout counter increments |
| Proposer returns malformed JSON | Retry once; on second failure return `Skipped(ParseError)` |
| Retrieved context is empty | Checker returns `Inconclusive` for all assertions; no false positives |
| Response has more assertions than `max_assertions_per_response` | Truncate assertion list; log at `DEBUG`; check first N assertions only |
| Checker provider returns network error | Return `Skipped(ProviderError)`; no error propagated |
| `async_run = true` in config | Pipeline runs synchronously; emits `WARN` noting unimplemented async mode |
| Contradiction detected | Log at `WARN` with assertion and snippet; verdict = `PartialFailure`; response delivered unchanged |
| All assertions supported | Verdict = `AllSupported`; no log output; counter increments |

---

## 8. Success Criteria

- [ ] Integration test: contradiction on a known graph fact produces `PartialFailure` verdict
- [ ] Unit test: Checker function has no `response` parameter (compile-time invariant enforced)
- [ ] Timeout test: induced 3 s provider latency produces `Skipped(Timeout)` with response delivered
- [ ] Disabled test: `self_check = false` produces zero allocations on the hook path
- [ ] Prometheus counters export `quality_march_runs_total`, `quality_march_assertions_total`, `quality_march_failures_total`, `quality_march_timeouts_total`
- [ ] `--migrate-config` step 22 adds `[quality]` section to existing configs
- [ ] `--init` step_quality offers `[quality]` configuration

---

## 9. Acceptance Criteria

```
GIVEN self_check = true
  AND a response asserting "Alice works at Acme"
  AND a graph edge (Alice, works_at, Globex) in retrieved context
WHEN quality_hook::run_if_enabled executes
THEN verdict = PartialFailure
AND WARN log contains "Alice works at" and the conflicting snippet
AND the response is delivered to the user unchanged
AND quality_march_failures_total increments by 1

GIVEN self_check = false (default)
WHEN quality_hook::run_if_enabled executes
THEN no provider call is made
AND verdict = Skipped(Disabled)
AND zero allocations on the hook path

GIVEN self_check = true
  AND proposer_timeout_ms = 100
  AND the proposer provider takes 500 ms
WHEN quality_hook::run_if_enabled executes
THEN verdict = Skipped(Timeout)
AND quality_march_timeouts_total increments
AND the response is delivered without delay
```

---

## 10. Open Questions

> [!question]
> - **Gate mode post-MVP**: when gate mode (blocking on check failure) is implemented,
>   what is the policy for contradicted assertions — suppress, append a disclaimer, or
>   re-generate? This decision affects the Proposer prompt and the user-facing UX
>   significantly and must be specced before implementation.
> - **Async run**: `async_run = true` is in the config schema for stability but the
>   implementation is synchronous. Define the async execution model (background task,
>   channel notification) before enabling it.

---

## 11. See Also

- [[constitution]] — project principles
- [[002-agent-loop/spec]] — turn lifecycle (hook injection point)
- [[003-llm-providers/spec]] — provider resolution
- [[024-multi-model-design/spec]] — provider tier guidance for Proposer/Checker
- [[004-memory/spec]] — retrieved context used by Checker
- [[MOC-specs]] — all specifications
