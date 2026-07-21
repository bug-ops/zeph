---
aliases:
  - ClawVM Typed Pages
  - Typed Page Compaction
  - Minimum-Fidelity Compaction
tags:
  - sdd
  - spec
  - memory
  - compaction
  - context
  - experimental
created: 2026-04-19
status: implemented
related:
  - "[[MOC-specs]]"
  - "[[constitution]]"
  - "[[001-system-invariants/spec]]"
  - "[[004-memory/spec]]"
  - "[[004-2-compaction]]"
  - "[[021-zeph-context/spec]]"
---

# Spec: ClawVM Typed Page Compaction

> [!info]
> Classifies every context segment into a typed "page" with a per-type minimum-fidelity
> invariant enforced at every compaction boundary. Replaces the current untyped
> truncation/summarization pipeline in `zeph-context` with a page-aware compactor.
> Resolves GitHub issue [#3221](https://github.com/bug-ops/zeph/issues/3221).

## Sources

### External
- **ClawVM: Typed Memory Pages for Agent Context** (research memo, 2026) — per-type fidelity floors, structured ToolOutput summaries
- Inspiration from ClawVM register-page discipline: every page has a declared schema and a minimum-information contract

### Internal
| File | Contents |
|---|---|
| `crates/zeph-context/src/assembler.rs` | Parallel context gather (current compaction boundary) |
| `crates/zeph-context/src/slot.rs` | Per-slot content + token count |
| `crates/zeph-context/src/manager.rs` | `CompactionState` lifecycle |
| `crates/zeph-context/src/summarization.rs` | Summarization prompt helpers |
| `crates/zeph-memory/src/compaction_probe.rs` | Semantic loss probe |
| `crates/zeph-memory/src/anchored_summary.rs` | Tool pair summary storage |

---

## 1. Overview

### Problem Statement

Today compaction treats every message uniformly: tool output, free-form conversation turn,
semantic recall, and system prompts all go through the same summarizer prompt and the same
60/90% pressure gates. This produces two recurring failure modes observed in CI cycles
(see `#3221` description):

1. **Structured tool outputs lose structure.** A 4 KB JSON response from `shell` or an
   HTTP tool becomes a lossy English paragraph; downstream tool chains that depended on
   a path, status code, or field name silently drift.
2. **System context gets compressed.** Session digests, persona memory, and skill
   instructions are high-density information that should either be retained verbatim or
   replaced with a pointer — never paraphrased.

### Goal

Classify each context segment into a **typed page** with a declared schema and a
per-type minimum-fidelity invariant. Every compaction boundary (soft mark, hard apply,
microcompact eviction) must honour the invariant or refuse to compact the page.

### Out of Scope

- Changes to the token budget arithmetic in `ContextBudget` (021 already stable)
- Changes to graph recall or admission control (separate specs)
- Channel-level output formatting
- Changing the compaction provider registry (see 024)

---

## 2. User Stories

### US-001: Structured tool output retention
AS A developer running long agent sessions with tool chains
I WANT tool outputs to retain their structure after compaction
SO THAT downstream tool calls can reference specific fields from prior outputs

**Acceptance criteria:**
```
GIVEN a tool output page containing a JSON body with multiple top-level keys
WHEN compaction runs under memory pressure
THEN the compacted form preserves tool name, exit status, and at least one structural key
AND downstream tool calls that reference those keys continue to resolve correctly
```

### US-002: System context fidelity
AS AN operator deploying Zeph with custom skill instructions
I WANT system context to never be paraphrased during compaction
SO THAT skill behavior remains consistent throughout the session

**Acceptance criteria:**
```
GIVEN a SystemContext page containing skill instructions or persona memory
WHEN the compactor considers it under hard pressure
THEN the page is replaced with a pointer record (hash + retrieval hint)
AND no paraphrase of the original content is injected into the context
AND the original bytes remain retrievable via the pointer
```

### US-003: Compaction audit trail
AS A developer debugging unexpected agent behavior
I WANT a per-turn audit log of what was compacted with fidelity details
SO THAT I can diagnose information loss post-hoc

**Acceptance criteria:**
```
GIVEN any compaction event occurs during a session
WHEN the audit log is inspected after the session
THEN each compacted page has exactly one audit record
AND the record contains page_type, page_id, original_tokens, compacted_tokens, fidelity_level, and any violations
AND the log is readable as valid JSONL with a stable schema
```

---

## 3. Functional Requirements

| ID | Requirement | Priority |
|----|------------|----------|
| FR-001 | WHEN a context segment enters the assembler THE SYSTEM SHALL tag it with a `PageType` (`ToolOutput`, `ConversationTurn`, `MemoryExcerpt`, `SystemContext`) | must |
| FR-002 | WHEN the compactor considers a page THE SYSTEM SHALL look up the registered minimum-fidelity invariant for its `PageType` and reject compaction plans that violate it | must |
| FR-003 | WHEN a `ToolOutput` page is compacted THE SYSTEM SHALL emit a structured summary retaining: tool name, exit status, structural keys (top-level JSON field names or first-column headers), byte/line count, first N bytes preview | must |
| FR-004 | WHEN a `ConversationTurn` page is compacted THE SYSTEM SHALL produce a semantic summary preserving speaker role, intent verb, and any named entity with confidence above the graph extraction threshold | must |
| FR-005 | WHEN a `MemoryExcerpt` page is compacted THE SYSTEM SHALL preserve its source label and `MessageId` reference; summarization is allowed only below the per-excerpt byte ceiling | must |
| FR-006 | WHEN a `SystemContext` page is compacted THE SYSTEM SHALL replace it with a pointer (stable hash + retrieval hint) — never paraphrase | must |
| FR-007 | WHEN a page is compacted THE SYSTEM SHALL append one audit-log entry containing: `page_type`, `page_id`, `original_tokens`, `compacted_tokens`, `fidelity_level`, `invariant_version`, `provider_name`, `timestamp` | must |
| FR-008 | WHEN classification is ambiguous THE SYSTEM SHALL default to `ConversationTurn` and log at `WARN` level | should |
| FR-009 | WHEN `[memory.compaction.typed_pages]` is disabled THE SYSTEM SHALL fall back to the legacy untyped compactor without behavior change | must |
| FR-010 | WHEN a page's compacted form would still violate its invariant THE SYSTEM SHALL prefer omission (slot = None) over a lossy paraphrase | must |

---

## 4. Non-Functional Requirements

| ID | Category | Requirement |
|----|----------|-------------|
| NFR-001 | Performance | Classification of a single page SHALL complete in < 1 ms (deterministic, no I/O); compaction of one page (including LLM call) SHALL complete in < 2 s at p95 under the 500 ms fast-tier provider budget |
| NFR-002 | Performance | Audit sink write SHALL not block the compaction hot path; bounded mpsc channel ensures < 5 ms overhead per compacted page |
| NFR-003 | Reliability | When the audit path is unwritable the system SHALL fail-closed for the current turn (skip typed-page compaction, fall back to legacy) — never silently swallow the error |
| NFR-004 | Reliability | `max_retry_on_violation = 1` is the hard ceiling; the system SHALL never enter an unbounded retry loop on a fidelity violation |
| NFR-005 | Reliability | Feature flag `enabled = false` SHALL reproduce legacy behavior exactly — no behavioral drift or partial state changes |
| NFR-006 | Maintainability | `PageInvariant` implementations SHALL be independently registrable and testable; new `PageType` variants can be added without modifying the compactor core |
| NFR-007 | Maintainability | Classification rules are deterministic and pure (no LLM call, no DB read) — verifiable by unit test without external dependencies |
| NFR-008 | Observability | Prometheus counters SHALL be exported: `compaction_pages_total{page_type}`, `compaction_violations_total{page_type}`, `compaction_bytes_saved_total{page_type}` |
| NFR-009 | Observability | Each compaction boundary SHALL emit a `tracing::info_span!` named `context.compaction.typed_page` capturing `page_type` and `fidelity_level` attributes |
| NFR-010 | Security | Audit log keys (`page_id`, `origin`) SHALL NOT include raw page body content — only structural metadata and BLAKE3 hash; no PII leakage via audit path |
| NFR-011 | Security | `tool_output_preview_bytes` is capped at 1024; raising it above that threshold requires an explicit `Ask First` decision (cost and information-exposure impact) |

---

## 5. Architecture

### Page Classification

```
TypedPage {
    id: PageId,                  // stable BLAKE3 over source bytes
    page_type: PageType,
    origin: PageOrigin,          // ToolPair(tool_name), Turn(MessageId), Excerpt(source_label), System(key)
    tokens: u32,
    body: Arc<str>,
    schema_hint: Option<SchemaHint>,  // Json, Text, Diff, Table — for ToolOutput only
}
```

Classification happens in `ContextAssembler::gather()` before budget allocation, so the
compactor works with pre-typed inputs. Classification rules (deterministic):

| Source | Assigned PageType |
|---|---|
| Tool request/response pair from memory | `ToolOutput` |
| User or assistant message (no tool role) | `ConversationTurn` |
| `MessagePart::CrossSession`, `MessagePart::Summary`, graph facts | `MemoryExcerpt` |
| Session digest, persona, skill instructions, compression guidelines | `SystemContext` |

### Invariant Enforcement

A `PageInvariant` trait (object-safe) is registered per `PageType`:

```
trait PageInvariant: Send + Sync {
    fn page_type(&self) -> PageType;
    fn minimum_fidelity(&self, page: &TypedPage) -> FidelityContract;
    fn verify(&self, original: &TypedPage, compacted: &CompactedPage) -> Result<(), FidelityViolation>;
}
```

`FidelityContract` declares required fields (e.g., `ToolOutput` → `["tool_name",
"exit_status", "structural_keys", "byte_count"]`). `verify()` runs **after** the
summarization call and **before** the compacted page is swapped into the assembled context.
Verification failure → the compactor records a `FidelityViolation` in the audit log,
increments `memory.compaction.violations` counter, and either (a) re-runs with a stricter
prompt (once) or (b) omits the slot.

### Integration with CompactionState

Typed-page compaction is a strict subset of the existing 60/90% soft/hard pressure flow
in `[[004-2-compaction]]`. The state machine is unchanged; only the inner transformation
becomes page-aware. `CompactionState::Exhausted` still fires when no pages are compactable.

### Audit Log

One `CompactedPageRecord` per compacted page is appended to
`.local/audit/compaction.jsonl` (mirroring tool-audit format):

```
{
  "ts": "2026-04-19T12:34:56Z",
  "turn_id": "...",
  "page_id": "blake3:...",
  "page_type": "ToolOutput",
  "origin": {"kind": "tool_pair", "tool_name": "shell"},
  "original_tokens": 1240,
  "compacted_tokens": 86,
  "fidelity_level": "structured_summary_v1",
  "invariant_version": 1,
  "provider_name": "fast",
  "violations": []
}
```

### Config

```toml
[memory.compaction.typed_pages]
enabled = true
provider = "fast"                       # references [[llm.providers]]
audit_path = ".local/audit/compaction.jsonl"
tool_output_preview_bytes = 256
tool_output_retain_structural_keys = true
system_context_pointer_only = true      # never paraphrase
max_retry_on_violation = 1              # single reprompt, then omit
```

---

## 6. Key Invariants

### Always (without asking)
- Every context segment entering the assembler carries a `PageType` — no untyped pages reach the compactor
- Every compaction produces exactly one audit record; the record is flushed before the LLM call that uses the compacted context
- `SystemContext` pages are never paraphrased — always pointer-replace or verbatim-retain
- `ToolOutput` summary always includes tool name, exit status, and at least one structural key
- `FidelityViolation` is a hard signal — the compacted page is dropped, not injected "best-effort"
- Classification is deterministic and side-effect free (no LLM call, no DB read)
- Audit record `page_id` equals BLAKE3 of the source bytes — stable across runs for the same input

### Ask First
- Adding a new `PageType` variant (affects all invariant registrations and migration)
- Raising `tool_output_preview_bytes` above 1024 (security and cost impact)
- Disabling `system_context_pointer_only` (regression risk — why 006 specs restrict paraphrase)
- Changing the fidelity contract schema version (`invariant_version`)

### Never
- Paraphrase a `SystemContext` page
- Emit a `ToolOutput` summary as free-form English only (structural fields are mandatory)
- Swallow `FidelityViolation` — always log, always audit, always drop the compaction
- Perform I/O or LLM calls inside the classifier
- Run the typed-page compactor without the audit sink active (fail-closed when audit path is unwritable)

---

## 7. Edge Cases and Error Handling

| Scenario | Expected Behavior |
|----------|-------------------|
| Classification ambiguous (e.g., tool result embedded in assistant turn) | Default to `ConversationTurn`; log at `WARN`; audit marks `classification = fallback` |
| `ToolOutput` body is binary (non-UTF8) | Retain tool_name + exit_status + byte_count only; body body is `<binary:N bytes>` |
| Provider returns a summary missing required fields | Re-prompt once with explicit schema; on second failure, omit slot and emit violation |
| Audit path unwritable | `typed_pages` compaction disabled for the turn; fall back to legacy compactor; log at `ERROR` |
| Page already compacted earlier in session | Use existing `compacted_at` form; skip re-compaction; no audit record |
| Disabled feature flag | Bypass classification entirely; legacy behavior |
| `SystemContext` pointer lookup fails later | Treat as cache miss; fetch original from source-of-truth; never substitute arbitrary text |

---

## 8. Success Criteria

- [ ] Every `PageType` has at least one invariant test covering compaction + verification
- [ ] Property test: for 10k random tool outputs, compacted form always contains the tool name and exit status
- [ ] Integration test: disabling the flag reproduces legacy behavior byte-for-byte in audit-free paths
- [ ] Live test: multi-turn session with shell tool produces structured summaries verifiable against original JSON
- [ ] Audit JSONL parses cleanly (one record per line, schema version stable)
- [ ] Zero `SystemContext` paraphrase events in 1000-turn synthetic benchmark
- [ ] CompactionState transitions unchanged (021 tests pass)
- [ ] Violation counter is exposed in `MetricsSnapshot` and Prometheus export

---

## 9. Acceptance Criteria (Given / When / Then)

```
GIVEN a tool pair page of 1200 tokens (JSON body)
WHEN typed_pages compaction runs with provider "fast"
THEN the compacted page is ≤ 128 tokens
AND contains tool_name, exit_status, at least one structural JSON key
AND one audit record is flushed with fidelity_level = "structured_summary_v1"

GIVEN a SystemContext page containing skill instructions
WHEN the compactor considers it under hard pressure
THEN the page is replaced by a pointer record (hash + retrieval hint)
AND the original bytes remain retrievable via the pointer
AND no LLM call is made for this page

GIVEN a compacted page that fails invariant verification
WHEN max_retry_on_violation = 1
THEN the compactor reprompts once with a stricter schema prompt
AND on second failure, the slot is set to None
AND an audit record with non-empty violations is emitted
```

---

## 10. Implementation Notes

- `PageType` lives in `zeph-context` (no `zeph-memory` dep creep — 021 layering honored)
- Invariants registered via a small registry struct owned by `ContextAssembler`; tests can swap a mock registry
- Audit sink reuses `zeph-tools` audit writer pattern (bounded mpsc, drop counter)
- `PageId` = `BLAKE3(page_type_tag || origin_tag || body_bytes)` — 16 bytes base32 encoded
- Retry path uses the same provider; no cascading to a more expensive model (cost discipline)
- Metrics: `compaction_pages_total{page_type}`, `compaction_violations_total{page_type}`, `compaction_bytes_saved_total{page_type}`

---

## 11. Open Questions

> [!question]
> - **PageId semantics**: should `PageId` be a content hash (BLAKE3, stable across identical inputs) or a per-turn unique id (monotonic, unique across sessions)? The current spec implies BLAKE3, but audit deduplication and invariant tests behave differently under each scheme. Needs an explicit decision before invariant tests are written.
> - **Audit sink flush guarantee**: should the audit record be flushed strictly before the LLM call that uses the compacted context (guaranteeing observability of every compaction), or is best-effort flush (background write, possible loss on crash) acceptable? Pick one and document the failure mode explicitly.

---

## 12. See Also

- [[constitution]] — project principles
- [[001-system-invariants/spec]] — cross-cutting invariants
- [[004-memory/spec]] — memory pipeline
- [[004-2-compaction]] — existing compaction (extended by this spec)
- [[021-zeph-context/spec]] — assembler integration point
- [[040-sanitizer/spec]] — content sanitizer runs before classification
- [[MOC-specs]] — all specifications
